//! `rndis-tetherd` — brings up Android USB tethering over RNDIS.
//!
//! Runs as a resident daemon: it watches for the phone, holds the tunnel up
//! while it is attached, and restores the system on unplug.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use log::{error, info, warn};
use rndis_tether_netstack::MacAddr;
use rndis_tether_rndis::HOST_MAX_TRANSFER_SIZE;
use rndis_tether_usb::{DefaultBackend, UsbBackend};

mod device;
mod link;
mod signals;
mod status;
mod transport;
mod tunnel;

use link::{Link, LinkEvent, SwitchableSink};
use status::{Status, StatusServer};
use tunnel::Tunnel;

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Pause before retrying after a failed session, so a phone that keeps
/// rejecting us is not hammered.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long to block waiting for a hotplug event between scans.
const HOTPLUG_WAIT: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");
    // nusb logs every transfer; keep it out of our own output.
    let default = if verbose {
        "debug,nusb=info"
    } else {
        "info,nusb=warn"
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default))
        .format_timestamp_millis()
        .init();

    signals::install();

    let backend = DefaultBackend::default();
    // Without root there is no /var/run socket; the daemon still works for
    // read-only probing, so this is a warning rather than a failure.
    let status = match StatusServer::start() {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("status socket unavailable: {e}");
            None
        }
    };

    let mut watch = backend.watch().ok();
    info!("waiting for a phone with USB tethering enabled");

    while !signals::requested() {
        match device::find(&backend) {
            Ok(Some((info, function))) => {
                let label = info.label();
                if let Some(s) = &status {
                    s.update(|st| st.device = Some(label.clone()));
                }
                if let Err(e) = run_session(&backend, info, function, status.as_ref()) {
                    error!("session ended: {e:#}");
                }
                if let Some(s) = &status {
                    s.update(|st| *st = Status::default());
                }
                if !signals::requested() {
                    info!("waiting for the phone to come back");
                    std::thread::sleep(RETRY_DELAY);
                }
            }
            Ok(None) => wait_for_attach(&mut watch),
            Err(e) => {
                warn!("scanning for devices failed: {e:#}");
                std::thread::sleep(RETRY_DELAY);
            }
        }
    }

    info!("shutting down");
    Ok(())
}

/// Block until something is plugged in, or the wait elapses.
fn wait_for_attach(watch: &mut Option<Box<dyn rndis_tether_usb::HotplugWatch>>) {
    match watch {
        Some(w) => {
            w.next_event(HOTPLUG_WAIT);
        }
        None => std::thread::sleep(HOTPLUG_WAIT),
    }
}

/// Hold one phone's tunnel up until it fails, detaches, or we are asked to stop.
fn run_session(
    backend: &DefaultBackend,
    info: rndis_tether_usb::DeviceInfo,
    function: rndis_tether_usb::RndisFunction,
    status: Option<&StatusServer>,
) -> Result<()> {
    info!("found {}", info.label());
    let mut device = device::open(backend, info, function)?;

    let sink = Arc::new(SwitchableSink::default());
    let link = Link::start(
        device.bulk_in,
        device.bulk_out,
        MacAddr(device.session.host_mac),
        device.session.device_max_transfer_size,
        HOST_MAX_TRANSFER_SIZE,
        sink.clone(),
    );

    let mut tunnel: Option<Tunnel> = None;
    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;

    let result = loop {
        if signals::requested() {
            break Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);

        match link::try_next_event(&link.events) {
            Some(LinkEvent::Bound(lease)) => {
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
                    // Drop the old tunnel first so its routes are gone before
                    // the new ones go in.
                    sink.detach();
                    tunnel = None;

                    match Tunnel::up(&link, *lease) {
                        Ok(t) => {
                            sink.attach(t.sink());
                            if let Some(s) = status {
                                let name = t.utun.name().to_string();
                                let lease = t.lease.clone();
                                s.update(|st| {
                                    st.link_up = true;
                                    st.interface = Some(name);
                                    st.address = Some(lease.ip);
                                    st.gateway = lease.router;
                                    st.dns = lease.dns;
                                });
                            }
                            tunnel = Some(t);
                        }
                        Err(e) => break Err(e),
                    }
                }
            }
            Some(LinkEvent::Failed(e)) => break Err(anyhow::anyhow!("link failed: {e}")),
            None => {}
        }

        if Instant::now() >= next_keepalive {
            next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
            if let Err(e) = device.control.keepalive() {
                break Err(anyhow::anyhow!("keepalive failed: {e}"));
            }
            let delivered = sink.delivered.load(Ordering::Relaxed);
            if let Some(s) = status {
                s.update(|st| st.packets_in = delivered);
            }
            match &tunnel {
                Some(t) => info!(
                    "{} up, link {:?}, {delivered} packets in",
                    t.utun.name(),
                    device.control.link_state()
                ),
                None => warn!("still waiting for a DHCP lease"),
            }
        }
    };

    // Order matters: stop delivering, restore routes and DNS, then stop the
    // link threads.
    sink.detach();
    drop(tunnel);
    let _ = device.control.halt();
    link.shutdown();
    result
}
