use std::time::Duration;

use anyhow::{Context, Result};
use log::info;
use rndis_tether_usb::NusbBackend;

mod device;
mod transport;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

fn main() -> Result<()> {
    let verbose = std::env::args().any(|a| a == "-v" || a == "--verbose");
    // nusb logs every transfer at debug/info; keep it out of our own output.
    let default = if verbose {
        "debug,nusb=info"
    } else {
        "info,nusb=warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
    .format_timestamp_millis()
    .init();

    let backend = NusbBackend::new();

    let (info, function) = device::find(&backend)?
        .context("no RNDIS device found — enable USB tethering on the phone")?;
    info!("found {} — {:?}", info.label(), function);

    let mut device = device::open(&backend, info, function)?;
    info!(
        "link up: device MTU budget {} B, {} packets/transfer",
        device.session.device_max_transfer_size, device.session.max_packets_per_transfer
    );

    // Until the data path lands, prove the link stays alive.
    loop {
        std::thread::sleep(KEEPALIVE_INTERVAL);
        device
            .control
            .keepalive()
            .map_err(|e| anyhow::anyhow!("keepalive failed: {e}"))?;
        info!("keepalive ok, link {:?}", device.control.link_state());
    }
}
