use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use log::{info, warn};
use rndis_tether_netstack::MacAddr;
use rndis_tether_rndis::HOST_MAX_TRANSFER_SIZE;
use rndis_tether_usb::NusbBackend;

mod device;
mod link;
mod transport;
mod tunnel;

use link::{Link, LinkEvent, SwitchableSink};
use tunnel::Tunnel;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

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
    let sink = Arc::new(SwitchableSink::default());
    let link = Link::start(
        device.bulk_in,
        device.bulk_out,
        MacAddr(device.session.device_mac),
        device.session.device_max_transfer_size,
        HOST_MAX_TRANSFER_SIZE,
        sink.clone(),
    );

    let mut tunnel: Option<Tunnel> = None;
    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;

    let result = loop {
        std::thread::sleep(POLL_INTERVAL);

        match link::try_next_event(&link.events) {
            Some(LinkEvent::Bound(lease)) => {
                // A renewal of the same address needs no new tunnel.
                if tunnel.as_ref().is_some_and(|t| t.lease.ip == lease.ip) {
                    info!("DHCP lease renewed: {}", lease.ip);
                } else {
                    info!(
                        "DHCP lease: {}/{} gw {:?} dns {:?}",
                        lease.ip,
                        lease.prefix_len(),
                        lease.router,
                        lease.dns
                    );
                    // Tear the old one down first so its routes are gone
                    // before the new ones are installed.
                    sink.detach();
                    tunnel = None;

                    match Tunnel::up(&link, *lease) {
                        Ok(t) => {
                            sink.attach(t.sink());
                            tunnel = Some(t);
                        }
                        Err(e) => break Err(e.context("bringing the tunnel up")),
                    }
                }
            }
            Some(LinkEvent::Failed(e)) => break Err(anyhow!("link failed: {e}")),
            None => {}
        }

        if Instant::now() >= next_keepalive {
            next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
            if let Err(e) = device.control.keepalive() {
                break Err(anyhow!("keepalive failed: {e}"));
            }
            match &tunnel {
                Some(t) => info!(
                    "{} up, link {:?}, {} packets in",
                    t.utun.name(),
                    device.control.link_state(),
                    sink.delivered.load(Ordering::Relaxed)
                ),
                None => warn!("still waiting for a DHCP lease"),
            }
        }
    };

    // Drop order matters: the tunnel restores routes and DNS, then the link
    // threads stop.
    sink.detach();
    drop(tunnel);
    link.shutdown();
    result
}
