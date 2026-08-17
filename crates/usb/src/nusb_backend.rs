//! `UsbBackend` over nusb (pure Rust, talks to IOKit directly).

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

use futures_lite::StreamExt;
use nusb::transfer::{Buffer, Completion, TransferError};
use nusb::MaybeFuture;

use crate::backend::{
    ControlSetup, ControlType, HotplugEvent, HotplugWatch, InEndpoint, OutEndpoint, Recipient,
    UsbBackend, UsbDevice, UsbInterface,
};
use crate::descriptor::{
    ConfigDescriptor, DeviceId, DeviceInfo, EndpointDescriptor, InterfaceDescriptor, TransferType,
};
use crate::error::{Result, UsbError};

#[derive(Default)]
pub struct NusbBackend;

impl NusbBackend {
    pub fn new() -> Self {
        Self
    }
}

impl UsbBackend for NusbBackend {
    fn list(&self) -> Result<Vec<DeviceInfo>> {
        Ok(list_nusb()?.into_iter().map(|(_, info)| info).collect())
    }

    fn open(&self, id: DeviceId) -> Result<Box<dyn UsbDevice>> {
        let info = list_nusb()?
            .into_iter()
            .find(|(_, i)| i.id == id)
            .ok_or(UsbError::NotFound)?;

        let device = info.0.open().wait().map_err(err)?;
        Ok(Box::new(NusbDeviceHandle {
            info: info.1,
            device,
        }))
    }

    fn watch(&self) -> Result<Box<dyn HotplugWatch>> {
        let mut watch = nusb::watch_devices().map_err(err)?;

        // Seed the id map with devices already present so their disconnects can
        // still be resolved to a stable `DeviceId`.
        let mut ids: HashMap<nusb::DeviceId, DeviceId> = list_nusb()?
            .iter()
            .map(|(raw, info)| (raw.id(), info.id))
            .collect();

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("usb-hotplug".into())
            .spawn(move || {
                futures_lite::future::block_on(async move {
                    while let Some(event) = watch.next().await {
                        let translated = match event {
                            nusb::hotplug::HotplugEvent::Connected(raw) => {
                                let info = device_info(&raw);
                                ids.insert(raw.id(), info.id);
                                HotplugEvent::Connected(info)
                            }
                            nusb::hotplug::HotplugEvent::Disconnected(raw) => {
                                match ids.remove(&raw) {
                                    Some(id) => HotplugEvent::Disconnected(id),
                                    // A device we never saw connect; nothing to tear down.
                                    None => continue,
                                }
                            }
                        };
                        if tx.send(translated).is_err() {
                            break;
                        }
                    }
                })
            })
            .map_err(|e| UsbError::Other(e.to_string()))?;

        Ok(Box::new(ChannelHotplugWatch { rx }))
    }
}

struct ChannelHotplugWatch {
    rx: Receiver<HotplugEvent>,
}

impl HotplugWatch for ChannelHotplugWatch {
    fn next_event(&mut self, timeout: Duration) -> Option<HotplugEvent> {
        match self.rx.recv_timeout(timeout) {
            Ok(event) => Some(event),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

fn list_nusb() -> Result<Vec<(nusb::DeviceInfo, DeviceInfo)>> {
    Ok(nusb::list_devices()
        .wait()
        .map_err(err)?
        .map(|raw| {
            let info = device_info(&raw);
            (raw, info)
        })
        .collect())
}

fn device_info(raw: &nusb::DeviceInfo) -> DeviceInfo {
    DeviceInfo {
        id: stable_id(raw),
        vendor_id: raw.vendor_id(),
        product_id: raw.product_id(),
        manufacturer: raw.manufacturer_string().map(str::to_owned),
        product: raw.product_string().map(str::to_owned),
        serial: raw.serial_number().map(str::to_owned),
    }
}

#[cfg(target_os = "macos")]
fn stable_id(raw: &nusb::DeviceInfo) -> DeviceId {
    DeviceId(raw.registry_entry_id())
}

#[cfg(not(target_os = "macos"))]
fn stable_id(raw: &nusb::DeviceInfo) -> DeviceId {
    DeviceId(((raw.bus_id().len() as u64) << 32) | raw.device_address() as u64)
}

struct NusbDeviceHandle {
    info: DeviceInfo,
    device: nusb::Device,
}

impl UsbDevice for NusbDeviceHandle {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn configurations(&self) -> Result<Vec<ConfigDescriptor>> {
        Ok(self
            .device
            .configurations()
            .map(config_descriptor)
            .collect())
    }

    fn active_configuration(&self) -> Result<u8> {
        self.device
            .active_configuration()
            .map(|c| c.configuration_value())
            .map_err(|e| UsbError::Other(e.to_string()))
    }

    fn set_configuration(&self, value: u8) -> Result<()> {
        self.device.set_configuration(value).wait().map_err(err)
    }

    fn claim_interface(&self, number: u8) -> Result<Box<dyn UsbInterface>> {
        let interface = self.device.claim_interface(number).wait().map_err(err)?;
        Ok(Box::new(NusbInterfaceHandle { interface }))
    }
}

fn config_descriptor(config: nusb::descriptors::ConfigurationDescriptor<'_>) -> ConfigDescriptor {
    ConfigDescriptor {
        value: config.configuration_value(),
        interfaces: config
            .interface_alt_settings()
            .map(|alt| InterfaceDescriptor {
                number: alt.interface_number(),
                alt_setting: alt.alternate_setting(),
                class: alt.class(),
                subclass: alt.subclass(),
                protocol: alt.protocol(),
                endpoints: alt
                    .endpoints()
                    .map(|ep| EndpointDescriptor {
                        address: ep.address(),
                        transfer_type: transfer_type(ep.transfer_type()),
                        max_packet_size: ep.max_packet_size() as u16,
                    })
                    .collect(),
                extra: alt.descriptors().as_bytes().to_vec(),
            })
            .collect(),
    }
}

fn transfer_type(t: nusb::descriptors::TransferType) -> TransferType {
    use nusb::descriptors::TransferType as T;
    match t {
        T::Control => TransferType::Control,
        T::Isochronous => TransferType::Isochronous,
        T::Bulk => TransferType::Bulk,
        T::Interrupt => TransferType::Interrupt,
    }
}

struct NusbInterfaceHandle {
    interface: nusb::Interface,
}

impl UsbInterface for NusbInterfaceHandle {
    fn set_alt_setting(&self, alt: u8) -> Result<()> {
        self.interface.set_alt_setting(alt).wait().map_err(err)
    }

    fn control_out(&self, setup: ControlSetup, data: &[u8], timeout: Duration) -> Result<()> {
        self.interface
            .control_out(
                nusb::transfer::ControlOut {
                    control_type: control_type(setup.control_type),
                    recipient: recipient(setup.recipient),
                    request: setup.request,
                    value: setup.value,
                    index: setup.index,
                    data,
                },
                timeout,
            )
            .wait()
            .map_err(transfer_err)
    }

    fn control_in(&self, setup: ControlSetup, len: u16, timeout: Duration) -> Result<Vec<u8>> {
        self.interface
            .control_in(
                nusb::transfer::ControlIn {
                    control_type: control_type(setup.control_type),
                    recipient: recipient(setup.recipient),
                    request: setup.request,
                    value: setup.value,
                    index: setup.index,
                    length: len,
                },
                timeout,
            )
            .wait()
            .map_err(transfer_err)
    }

    fn open_in(&self, address: u8, ty: TransferType) -> Result<Box<dyn InEndpoint>> {
        use nusb::transfer::{Bulk, In, Interrupt};
        Ok(match ty {
            TransferType::Bulk => Box::new(NusbIn {
                ep: self.interface.endpoint::<Bulk, In>(address).map_err(err)?,
            }) as Box<dyn InEndpoint>,
            TransferType::Interrupt => Box::new(NusbIn {
                ep: self
                    .interface
                    .endpoint::<Interrupt, In>(address)
                    .map_err(err)?,
            }),
            other => {
                return Err(UsbError::Other(format!(
                    "unsupported IN endpoint {other:?}"
                )))
            }
        })
    }

    fn open_out(&self, address: u8, ty: TransferType) -> Result<Box<dyn OutEndpoint>> {
        use nusb::transfer::{Bulk, Interrupt, Out};
        Ok(match ty {
            TransferType::Bulk => Box::new(NusbOut {
                ep: self.interface.endpoint::<Bulk, Out>(address).map_err(err)?,
            }) as Box<dyn OutEndpoint>,
            TransferType::Interrupt => Box::new(NusbOut {
                ep: self
                    .interface
                    .endpoint::<Interrupt, Out>(address)
                    .map_err(err)?,
            }),
            other => {
                return Err(UsbError::Other(format!(
                    "unsupported OUT endpoint {other:?}"
                )))
            }
        })
    }
}

struct NusbIn<T: nusb::transfer::BulkOrInterrupt> {
    ep: nusb::Endpoint<T, nusb::transfer::In>,
}

impl<T: nusb::transfer::BulkOrInterrupt> InEndpoint for NusbIn<T> {
    fn max_packet_size(&self) -> usize {
        self.ep.max_packet_size()
    }

    fn pending(&self) -> usize {
        self.ep.pending()
    }

    fn submit(&mut self, len: usize) {
        let mps = self.ep.max_packet_size().max(1);
        let rounded = len.div_ceil(mps) * mps;
        self.ep.submit(Buffer::new(rounded.max(mps)));
    }

    fn wait(&mut self, timeout: Duration) -> Option<Result<Vec<u8>>> {
        if self.ep.pending() == 0 {
            return None;
        }
        self.ep.wait_next_complete(timeout).map(completion_bytes)
    }

    fn clear_halt(&mut self) -> Result<()> {
        self.ep.cancel_all();
        while self.ep.pending() > 0 {
            self.ep.wait_next_complete(Duration::from_secs(1));
        }
        self.ep.clear_halt().wait().map_err(err)
    }
}

struct NusbOut<T: nusb::transfer::BulkOrInterrupt> {
    ep: nusb::Endpoint<T, nusb::transfer::Out>,
}

impl<T: nusb::transfer::BulkOrInterrupt> OutEndpoint for NusbOut<T> {
    fn max_packet_size(&self) -> usize {
        self.ep.max_packet_size()
    }

    fn pending(&self) -> usize {
        self.ep.pending()
    }

    fn submit(&mut self, data: Vec<u8>) {
        self.ep.submit(Buffer::from(data));
    }

    fn wait(&mut self, timeout: Duration) -> Option<Result<()>> {
        if self.ep.pending() == 0 {
            return None;
        }
        self.ep
            .wait_next_complete(timeout)
            .map(|c| c.status.map_err(transfer_err))
    }

    fn clear_halt(&mut self) -> Result<()> {
        self.ep.cancel_all();
        while self.ep.pending() > 0 {
            self.ep.wait_next_complete(Duration::from_secs(1));
        }
        self.ep.clear_halt().wait().map_err(err)
    }
}

fn completion_bytes(c: Completion) -> Result<Vec<u8>> {
    match c.status {
        Ok(()) => Ok(c.buffer.into_vec()),
        Err(e) => Err(transfer_err(e)),
    }
}

fn control_type(t: ControlType) -> nusb::transfer::ControlType {
    match t {
        ControlType::Standard => nusb::transfer::ControlType::Standard,
        ControlType::Class => nusb::transfer::ControlType::Class,
        ControlType::Vendor => nusb::transfer::ControlType::Vendor,
    }
}

fn recipient(r: Recipient) -> nusb::transfer::Recipient {
    match r {
        Recipient::Device => nusb::transfer::Recipient::Device,
        Recipient::Interface => nusb::transfer::Recipient::Interface,
        Recipient::Endpoint => nusb::transfer::Recipient::Endpoint,
        Recipient::Other => nusb::transfer::Recipient::Other,
    }
}

fn transfer_err(e: TransferError) -> UsbError {
    match e {
        TransferError::Cancelled => UsbError::Timeout,
        TransferError::Stall => UsbError::Stall,
        TransferError::Disconnected => UsbError::Disconnected,
        other => UsbError::Other(other.to_string()),
    }
}

fn err(e: nusb::Error) -> UsbError {
    use nusb::ErrorKind;
    match e.kind() {
        ErrorKind::Disconnected => UsbError::Disconnected,
        ErrorKind::NotFound => UsbError::NotFound,
        _ => UsbError::Other(e.to_string()),
    }
}
