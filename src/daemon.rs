//! The resident daemon: watch for the phone, hold the tunnel up while it is
//! attached, restore the system on unplug.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::netstack::MacAddr;
use crate::rndis::HOST_MAX_TRANSFER_SIZE;
use crate::usb::{NusbBackend, UsbBackend};
use anyhow::Result;
use log::{error, info, warn};

use crate::link::{self, Link, LinkEvent, SwitchableSink, TxLimits};
use crate::status::{Status, StatusServer};
use crate::tunnel::Tunnel;
use crate::{device, signals};

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Pause before retrying after a failed session, so a phone that keeps
/// rejecting us is not hammered.
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How long to block waiting for a hotplug event between scans.
const HOTPLUG_WAIT: Duration = Duration::from_secs(2);

/// Run until interrupted. Returns once a shutdown signal has been handled.
pub fn run() -> Result<()> {
    signals::install();

    let backend = NusbBackend;
    let status = match StatusServer::start() {
        Ok(s) => Some(s),
        // Losing the status socket is not worth refusing to tether over.
        Err(e) => {
            warn!("status socket unavailable: {e}");
            None
        }
    };

    let mut watch = backend.watch().ok();
    info!("waiting for a phone with USB tethering enabled");

    let mut reported = Vec::new();

    while !signals::requested() {
        match device::find(&backend) {
            Ok(scan) => match scan.rndis {
                Some((info, function)) => {
                    reported.clear();
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
                None => {
                    report_idle(&scan.idle, &mut reported);
                    wait_for_attach(&mut watch);
                }
            },
            Err(e) => {
                warn!("scanning for devices failed: {e:#}");
                std::thread::sleep(RETRY_DELAY);
            }
        }
    }

    info!("shutting down");
    Ok(())
}

/// Name the devices that are attached without an RNDIS function, once per
/// change rather than once per scan. A phone that is plugged in but not
/// tethering looks exactly like this, and is the usual reason the wait drags on.
fn report_idle(seen: &[String], reported: &mut Vec<String>) {
    if reported.as_slice() == seen {
        return;
    }
    for label in seen {
        info!("{label} is attached but not tethering; enable USB tethering on the phone");
    }
    reported.clear();
    reported.extend_from_slice(seen);
}

/// Block until something is plugged in, or the wait elapses.
fn wait_for_attach(watch: &mut Option<Box<dyn crate::usb::HotplugWatch>>) {
    match watch {
        Some(w) => {
            w.next_event(HOTPLUG_WAIT);
        }
        None => std::thread::sleep(HOTPLUG_WAIT),
    }
}

/// Hold one phone's tunnel up until it fails, detaches, or we are asked to stop.
fn run_session(
    backend: &NusbBackend,
    info: crate::usb::DeviceInfo,
    function: crate::usb::RndisFunction,
    status: Option<&StatusServer>,
) -> Result<()> {
    info!("found {}", info.label());
    let mut device = device::open(backend, info, function)?;

    let sink = Arc::new(SwitchableSink::default());
    let link = Link::start(
        device.bulk_in,
        device.bulk_out,
        MacAddr(device.session.host_mac),
        TxLimits {
            max_transfer_size: device.session.device_max_transfer_size as usize,
            max_packets: device.session.max_packets_per_transfer as usize,
            alignment: device.session.packet_alignment as usize,
        },
        HOST_MAX_TRANSFER_SIZE,
        sink.clone(),
    );
    info!(
        "TX batching: up to {} packets / {} B per transfer",
        device.session.max_packets_per_transfer, device.session.device_max_transfer_size
    );

    let mut tunnel: Option<Tunnel> = None;
    let mut next_keepalive = Instant::now() + KEEPALIVE_INTERVAL;
    let (mut last_in, mut last_out) = (0u64, 0u64);

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
            let sent = link.sent.load(Ordering::Relaxed);
            let bytes_in = sink.bytes_in.load(Ordering::Relaxed);
            let bytes_out = link.bytes_out.load(Ordering::Relaxed);
            let transfers = link.transfers.load(Ordering::Relaxed).max(1);
            if let Some(s) = status {
                s.update(|st| {
                    st.packets_in = delivered;
                    st.packets_out = sent;
                });
            }
            let rate = |bytes: u64, before: u64| {
                (bytes.saturating_sub(before)) as f64 * 8.0 / KEEPALIVE_INTERVAL.as_secs_f64() / 1e6
            };
            match &tunnel {
                Some(t) => info!(
                    "{} up, link {:?}, {:.1}/{:.1} Mbit/s in/out, {delivered} in / {sent} out, {:.2} frames/transfer",
                    t.utun.name(),
                    device.control.link_state(),
                    rate(bytes_in, last_in),
                    rate(bytes_out, last_out),
                    sent as f64 / transfers as f64,
                ),
                None => warn!("still waiting for a DHCP lease"),
            }
            (last_in, last_out) = (bytes_in, bytes_out);
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
