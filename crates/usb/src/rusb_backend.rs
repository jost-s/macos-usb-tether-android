//! `UsbBackend` over rusb — Rust bindings to the same libusb a C driver would
//! use. Selected with `--features libusb` if nusb ever disappoints.
//!
//! libusb's synchronous API has no submission queue, so each endpoint gets
//! worker threads: several concurrent blocking transfers give the same
//! pipelining the queue-based backend gets from one thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use rusb::{DeviceHandle, GlobalContext};

use crate::backend::{
    ControlSetup, ControlType, HotplugEvent, HotplugWatch, InEndpoint, OutEndpoint, Recipient,
    UsbBackend, UsbDevice, UsbInterface,
};
use crate::descriptor::{
    ConfigDescriptor, DeviceId, DeviceInfo, EndpointDescriptor, InterfaceDescriptor, TransferType,
};
use crate::error::{Result, UsbError};

/// Concurrent transfers per endpoint.
const WORKERS: usize = 4;
/// Bound on how long a worker blocks before checking for shutdown.
const WORKER_TIMEOUT: Duration = Duration::from_millis(500);
/// How often `watch` rescans for attached devices.
const SCAN_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct RusbBackend;

impl RusbBackend {
    pub fn new() -> Self {
        Self
    }
}

impl UsbBackend for RusbBackend {
    fn list(&self) -> Result<Vec<DeviceInfo>> {
        Ok(enumerate()?.into_iter().map(|(_, info)| info).collect())
    }

    fn open(&self, id: DeviceId) -> Result<Box<dyn UsbDevice>> {
        let (device, info) = enumerate()?
            .into_iter()
            .find(|(_, i)| i.id == id)
            .ok_or(UsbError::NotFound)?;
        let handle = Arc::new(device.open().map_err(err)?);
        Ok(Box::new(RusbDevice {
            info,
            device,
            handle,
        }))
    }

    fn watch(&self) -> Result<Box<dyn HotplugWatch>> {
        // libusb hotplug callbacks vary by platform; polling the device list is
        // simpler and this is only a fallback path.
        let mut known: HashMap<DeviceId, DeviceInfo> = enumerate()?
            .into_iter()
            .map(|(_, info)| (info.id, info))
            .collect();

        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();

        std::thread::Builder::new()
            .name("usb-hotplug".into())
            .spawn(move || {
                while !thread_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(SCAN_INTERVAL);
                    let Ok(current) = enumerate() else { continue };
                    let current: HashMap<DeviceId, DeviceInfo> =
                        current.into_iter().map(|(_, i)| (i.id, i)).collect();

                    for (id, info) in &current {
                        if !known.contains_key(id)
                            && tx.send(HotplugEvent::Connected(info.clone())).is_err()
                        {
                            return;
                        }
                    }
                    for id in known.keys() {
                        if !current.contains_key(id)
                            && tx.send(HotplugEvent::Disconnected(*id)).is_err()
                        {
                            return;
                        }
                    }
                    known = current;
                }
            })
            .map_err(|e| UsbError::Other(e.to_string()))?;

        Ok(Box::new(PollingWatch { rx, stop }))
    }
}

struct PollingWatch {
    rx: Receiver<HotplugEvent>,
    stop: Arc<AtomicBool>,
}

impl HotplugWatch for PollingWatch {
    fn next_event(&mut self, timeout: Duration) -> Option<HotplugEvent> {
        self.rx.recv_timeout(timeout).ok()
    }
}

impl Drop for PollingWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn enumerate() -> Result<Vec<(rusb::Device<GlobalContext>, DeviceInfo)>> {
    let mut out = Vec::new();
    for device in rusb::devices().map_err(err)?.iter() {
        let Ok(desc) = device.device_descriptor() else {
            continue;
        };
        // Strings need an open handle; a device we cannot open still enumerates.
        let strings = device.open().ok();
        let read = |index: Option<u8>| -> Option<String> {
            let handle = strings.as_ref()?;
            handle.read_string_descriptor_ascii(index?).ok()
        };

        let info = DeviceInfo {
            id: DeviceId(((device.bus_number() as u64) << 8) | device.address() as u64),
            vendor_id: desc.vendor_id(),
            product_id: desc.product_id(),
            manufacturer: read(desc.manufacturer_string_index()),
            product: read(desc.product_string_index()),
            serial: read(desc.serial_number_string_index()),
        };
        out.push((device, info));
    }
    Ok(out)
}

struct RusbDevice {
    info: DeviceInfo,
    device: rusb::Device<GlobalContext>,
    handle: Arc<DeviceHandle<GlobalContext>>,
}

impl UsbDevice for RusbDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn configurations(&self) -> Result<Vec<ConfigDescriptor>> {
        let count = self
            .device
            .device_descriptor()
            .map_err(err)?
            .num_configurations();
        let mut configs = Vec::new();
        for i in 0..count {
            let Ok(config) = self.device.config_descriptor(i) else {
                continue;
            };
            configs.push(ConfigDescriptor {
                value: config.number(),
                interfaces: config
                    .interfaces()
                    .flat_map(|iface| {
                        iface.descriptors().map(|alt| InterfaceDescriptor {
                            number: alt.interface_number(),
                            alt_setting: alt.setting_number(),
                            class: alt.class_code(),
                            subclass: alt.sub_class_code(),
                            protocol: alt.protocol_code(),
                            endpoints: alt
                                .endpoint_descriptors()
                                .map(|ep| EndpointDescriptor {
                                    address: ep.address(),
                                    transfer_type: transfer_type(ep.transfer_type()),
                                    max_packet_size: ep.max_packet_size(),
                                })
                                .collect(),
                            extra: alt.extra().to_vec(),
                        })
                    })
                    .collect(),
            });
        }
        Ok(configs)
    }

    fn active_configuration(&self) -> Result<u8> {
        self.handle.active_configuration().map_err(err)
    }

    fn set_configuration(&self, value: u8) -> Result<()> {
        self.handle.set_active_configuration(value).map_err(err)
    }

    fn claim_interface(&self, number: u8) -> Result<Box<dyn UsbInterface>> {
        self.handle.claim_interface(number).map_err(err)?;
        Ok(Box::new(RusbInterface {
            handle: self.handle.clone(),
            number,
        }))
    }
}

fn transfer_type(t: rusb::TransferType) -> TransferType {
    match t {
        rusb::TransferType::Control => TransferType::Control,
        rusb::TransferType::Isochronous => TransferType::Isochronous,
        rusb::TransferType::Bulk => TransferType::Bulk,
        rusb::TransferType::Interrupt => TransferType::Interrupt,
    }
}

struct RusbInterface {
    handle: Arc<DeviceHandle<GlobalContext>>,
    number: u8,
}

impl UsbInterface for RusbInterface {
    fn set_alt_setting(&self, alt: u8) -> Result<()> {
        self.handle
            .set_alternate_setting(self.number, alt)
            .map_err(err)
    }

    fn control_out(&self, setup: ControlSetup, data: &[u8], timeout: Duration) -> Result<()> {
        self.handle
            .write_control(
                request_type(rusb::Direction::Out, setup),
                setup.request,
                setup.value,
                setup.index,
                data,
                timeout,
            )
            .map_err(err)?;
        Ok(())
    }

    fn control_in(&self, setup: ControlSetup, len: u16, timeout: Duration) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        let n = self
            .handle
            .read_control(
                request_type(rusb::Direction::In, setup),
                setup.request,
                setup.value,
                setup.index,
                &mut buf,
                timeout,
            )
            .map_err(err)?;
        buf.truncate(n);
        Ok(buf)
    }

    fn open_in(&self, address: u8, ty: TransferType) -> Result<Box<dyn InEndpoint>> {
        Ok(Box::new(RusbIn::new(self.handle.clone(), address, ty)))
    }

    fn open_out(&self, address: u8, ty: TransferType) -> Result<Box<dyn OutEndpoint>> {
        Ok(Box::new(RusbOut::new(self.handle.clone(), address, ty)))
    }
}

fn request_type(direction: rusb::Direction, setup: ControlSetup) -> u8 {
    rusb::request_type(
        direction,
        match setup.control_type {
            ControlType::Standard => rusb::RequestType::Standard,
            ControlType::Class => rusb::RequestType::Class,
            ControlType::Vendor => rusb::RequestType::Vendor,
        },
        match setup.recipient {
            Recipient::Device => rusb::Recipient::Device,
            Recipient::Interface => rusb::Recipient::Interface,
            Recipient::Endpoint => rusb::Recipient::Endpoint,
            Recipient::Other => rusb::Recipient::Other,
        },
    )
}

/// Endpoint max packet size, read back from the active configuration.
fn max_packet_size(handle: &DeviceHandle<GlobalContext>, address: u8) -> usize {
    let fallback = 512;
    let Ok(config) = handle.device().active_config_descriptor() else {
        return fallback;
    };
    config
        .interfaces()
        .flat_map(|i| i.descriptors())
        .flat_map(|alt| alt.endpoint_descriptors())
        .find(|ep| ep.address() == address)
        .map_or(fallback, |ep| ep.max_packet_size() as usize)
}

struct RusbIn {
    rx: Receiver<Result<Vec<u8>>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    handle: Arc<DeviceHandle<GlobalContext>>,
    address: u8,
    max_packet_size: usize,
    /// Reads the caller has asked for but not yet collected.
    outstanding: usize,
}

impl RusbIn {
    fn new(handle: Arc<DeviceHandle<GlobalContext>>, address: u8, ty: TransferType) -> Self {
        let max_packet_size = max_packet_size(&handle, address);
        let (tx, rx) = mpsc::sync_channel(WORKERS * 2);
        let stop = Arc::new(AtomicBool::new(false));

        let threads = (0..WORKERS)
            .map(|_| {
                let handle = handle.clone();
                let tx = tx.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 16384];
                    while !stop.load(Ordering::Relaxed) {
                        let result = match ty {
                            TransferType::Interrupt => {
                                handle.read_interrupt(address, &mut buf, WORKER_TIMEOUT)
                            }
                            _ => handle.read_bulk(address, &mut buf, WORKER_TIMEOUT),
                        };
                        let message = match result {
                            Ok(n) => Ok(buf[..n].to_vec()),
                            // A timeout just means nothing arrived.
                            Err(rusb::Error::Timeout) => continue,
                            Err(e) => Err(err(e)),
                        };
                        let fatal = message.is_err();
                        if tx.send(message).is_err() || fatal {
                            return;
                        }
                    }
                })
            })
            .collect();

        Self {
            rx,
            stop,
            threads,
            handle,
            address,
            max_packet_size,
            outstanding: 0,
        }
    }
}

impl InEndpoint for RusbIn {
    fn max_packet_size(&self) -> usize {
        self.max_packet_size
    }

    fn pending(&self) -> usize {
        self.outstanding
    }

    fn submit(&mut self, _len: usize) {
        // The workers read continuously; this only records demand so the
        // caller's queue accounting matches the nusb backend.
        self.outstanding += 1;
    }

    fn wait(&mut self, timeout: Duration) -> Option<Result<Vec<u8>>> {
        if self.outstanding == 0 {
            return None;
        }
        match self.rx.recv_timeout(timeout) {
            Ok(result) => {
                self.outstanding -= 1;
                Some(result)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.outstanding -= 1;
                Some(Err(UsbError::Disconnected))
            }
        }
    }

    fn clear_halt(&mut self) -> Result<()> {
        self.handle.clear_halt(self.address).map_err(err)
    }
}

impl Drop for RusbIn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

struct RusbOut {
    tx: Option<SyncSender<Vec<u8>>>,
    done: Receiver<Result<()>>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    handle: Arc<DeviceHandle<GlobalContext>>,
    address: u8,
    max_packet_size: usize,
    outstanding: usize,
}

impl RusbOut {
    fn new(handle: Arc<DeviceHandle<GlobalContext>>, address: u8, ty: TransferType) -> Self {
        let max_packet_size = max_packet_size(&handle, address);
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(WORKERS * 2);
        let (done_tx, done) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let rx = Arc::new(std::sync::Mutex::new(rx));

        let threads = (0..WORKERS)
            .map(|_| {
                let handle = handle.clone();
                let rx = rx.clone();
                let done_tx = done_tx.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let frame = {
                            let rx = rx.lock().expect("tx queue lock");
                            match rx.recv_timeout(WORKER_TIMEOUT) {
                                Ok(f) => f,
                                Err(RecvTimeoutError::Timeout) => continue,
                                Err(RecvTimeoutError::Disconnected) => return,
                            }
                        };
                        let result = match ty {
                            TransferType::Interrupt => {
                                handle.write_interrupt(address, &frame, WORKER_TIMEOUT)
                            }
                            _ => handle.write_bulk(address, &frame, WORKER_TIMEOUT),
                        };
                        if done_tx.send(result.map(|_| ()).map_err(err)).is_err() {
                            return;
                        }
                    }
                })
            })
            .collect();

        Self {
            tx: Some(tx),
            done,
            stop,
            threads,
            handle,
            address,
            max_packet_size,
            outstanding: 0,
        }
    }
}

impl OutEndpoint for RusbOut {
    fn max_packet_size(&self) -> usize {
        self.max_packet_size
    }

    fn pending(&self) -> usize {
        self.outstanding
    }

    fn submit(&mut self, data: Vec<u8>) {
        if let Some(tx) = &self.tx {
            if tx.send(data).is_ok() {
                self.outstanding += 1;
            }
        }
    }

    fn wait(&mut self, timeout: Duration) -> Option<Result<()>> {
        if self.outstanding == 0 {
            return None;
        }
        match self.done.recv_timeout(timeout) {
            Ok(result) => {
                self.outstanding -= 1;
                Some(result)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.outstanding -= 1;
                Some(Err(UsbError::Disconnected))
            }
        }
    }

    fn clear_halt(&mut self) -> Result<()> {
        self.handle.clear_halt(self.address).map_err(err)
    }
}

impl Drop for RusbOut {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Close the queue so idle workers stop waiting on it.
        self.tx = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn err(e: rusb::Error) -> UsbError {
    match e {
        rusb::Error::NoDevice => UsbError::Disconnected,
        rusb::Error::NotFound => UsbError::NotFound,
        rusb::Error::Timeout => UsbError::Timeout,
        rusb::Error::Pipe => UsbError::Stall,
        other => UsbError::Other(other.to_string()),
    }
}
