use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use rndis_tether_rndis::HOST_MAX_TRANSFER_SIZE;
use rndis_tether_usb::NusbBackend;

mod device;
mod link;
mod transport;

use link::{Link, LinkEvent, NullSink};

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

fn main() -> Result<()> {
    let verbose = std::env::args().any(|a| a == "-v" || a == "--verbose");
    // nusb logs every transfer; keep it out of our own output.
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
    info!("found {}", info.label());

    let mut device = device::open(&backend, info, function)?;
    let sink = Arc::new(NullSink::default());
    let link = Link::start(
        device.bulk_in,
        device.bulk_out,
        rndis_tether_netstack::MacAddr(device.session.device_mac),
        device.session.device_max_transfer_size,
        HOST_MAX_TRANSFER_SIZE,
        sink.clone(),
    );

    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
    let mut lease_logged = false;

    loop {
        std::thread::sleep(POLL_INTERVAL);

        match link::try_next_event(&link.events) {
            Some(LinkEvent::Bound(lease)) => {
                info!(
                    "DHCP lease: {}/{} gw {:?} dns {:?} for {:?}",
                    lease.ip,
                    lease.prefix_len(),
                    lease.router,
                    lease.dns,
                    lease.lease_time
                );
                if let Some(gw) = lease.router {
                    link.gateway_mac(gw);
                }
                lease_logged = true;
            }
            Some(LinkEvent::Failed(e)) => {
                link.shutdown();
                return Err(anyhow!("link failed: {e}"));
            }
            None => {}
        }

        if Instant::now() >= next_keepalive {
            next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
            if let Err(e) = device.control.keepalive() {
                link.shutdown();
                return Err(anyhow!("keepalive failed: {e}"));
            }
            if lease_logged {
                info!(
                    "link {:?}, {} IP packets seen",
                    device.control.link_state(),
                    sink.packets.load(Ordering::Relaxed)
                );
            } else {
                warn!("still waiting for a DHCP lease");
            }
        }
    }
}
