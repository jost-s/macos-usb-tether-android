//! Opening a phone's RNDIS function and bringing the link up.

use anyhow::{anyhow, Context, Result};
use log::{debug, info, warn};
use rndis_tether_rndis::{Rndis, Session};
use rndis_tether_usb::{
    find_rndis, DeviceInfo, InEndpoint, OutEndpoint, RndisFunction, TransferType, UsbBackend,
    UsbDevice,
};

use crate::transport::UsbControlTransport;

/// An open, initialized RNDIS link.
pub struct RndisDevice {
    pub session: Session,
    pub control: Rndis<UsbControlTransport>,
    pub bulk_in: Box<dyn InEndpoint>,
    pub bulk_out: Box<dyn OutEndpoint>,
    /// Held so the device handle outlives the claimed interfaces.
    _device: Box<dyn UsbDevice>,
}

/// The first attached device exposing an RNDIS function, with that function.
pub fn find(backend: &impl UsbBackend) -> Result<Option<(DeviceInfo, RndisFunction)>> {
    for info in backend.list().context("listing USB devices")? {
        let device = match backend.open(info.id) {
            Ok(d) => d,
            // Devices claimed by other drivers are simply not ours.
            Err(e) => {
                debug!("skipping {}: {e}", info.label());
                continue;
            }
        };
        let configs = match device.configurations() {
            Ok(c) => c,
            Err(e) => {
                debug!("skipping {}: {e}", info.label());
                continue;
            }
        };
        if let Some(function) = find_rndis(&configs) {
            return Ok(Some((info, function)));
        }
    }
    Ok(None)
}

/// Claim the interfaces and run the RNDIS bring-up sequence.
pub fn open(
    backend: &impl UsbBackend,
    info: DeviceInfo,
    function: RndisFunction,
) -> Result<RndisDevice> {
    let device = backend
        .open(info.id)
        .with_context(|| format!("opening {}", info.label()))?;

    match device.active_configuration() {
        Ok(active) if active == function.config_value => {}
        Ok(active) => {
            info!(
                "switching from configuration {active} to {}",
                function.config_value
            );
            device
                .set_configuration(function.config_value)
                .context("selecting the RNDIS configuration")?;
        }
        // Some devices report no active configuration until one is selected.
        Err(e) => {
            warn!("cannot read active configuration ({e}); selecting the RNDIS one");
            device
                .set_configuration(function.config_value)
                .context("selecting the RNDIS configuration")?;
        }
    }

    let control_interface = device
        .claim_interface(function.control_interface)
        .with_context(|| format!("claiming control interface {}", function.control_interface))?;
    let data_interface = device
        .claim_interface(function.data_interface)
        .with_context(|| format!("claiming data interface {}", function.data_interface))?;

    if function.data_alt_setting != 0 {
        data_interface
            .set_alt_setting(function.data_alt_setting)
            .context("selecting the data interface alt setting")?;
    }

    let transport = UsbControlTransport::new(
        control_interface,
        function.control_interface,
        function.interrupt_in,
    )
    .map_err(|e| anyhow!("opening the control transport: {e}"))?;

    let mut control = Rndis::new(transport);
    let session = control
        .bring_up()
        .map_err(|e| anyhow!("RNDIS bring-up: {e}"))?;

    let bulk_in = data_interface
        .open_in(function.bulk_in, TransferType::Bulk)
        .context("opening the bulk IN endpoint")?;
    let bulk_out = data_interface
        .open_out(function.bulk_out, TransferType::Bulk)
        .context("opening the bulk OUT endpoint")?;

    Ok(RndisDevice {
        session,
        control,
        bulk_in,
        bulk_out,
        _device: device,
    })
}
