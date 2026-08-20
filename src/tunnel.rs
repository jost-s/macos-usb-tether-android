//! The utun side of the shim: system configuration plus the thread that pumps
//! IP packets from the tunnel onto the RNDIS link.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::netstack::Lease;
use crate::tun::{configure_interface, net, Dns, Routes, Utun};
use anyhow::{Context, Result};
use log::{debug, info};

use crate::link::{IpSink, Link};

/// Ethernet payload limit; the phone's tether link is a normal 1500-byte MTU.
const DEFAULT_MTU: u16 = 1500;
/// Bound on how long the reader blocks before re-checking for shutdown.
const READ_TIMEOUT_MS: i64 = 250;

/// Writes packets received from the phone into the tunnel.
struct UtunSink(Arc<Utun>);

impl IpSink for UtunSink {
    fn deliver(&self, packet: &[u8]) {
        if let Err(e) = self.0.write(packet) {
            debug!("utun write failed: {e}");
        }
    }
}

/// An up tunnel with its routes and DNS. Dropping it restores the system.
pub struct Tunnel {
    pub utun: Arc<Utun>,
    pub lease: Lease,
    _routes: Routes,
    _dns: Dns,
    shutdown: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl Tunnel {
    /// Bring the tunnel up for `lease` and start forwarding.
    pub fn up(link: &Link, lease: Lease) -> Result<Self> {
        let gateway = lease
            .router
            .context("lease has no gateway; cannot route through the phone")?;

        let utun = Arc::new(Utun::open()?);
        utun.set_read_timeout(READ_TIMEOUT_MS)?;
        let name = utun.name().to_string();

        let mtu = lease.mtu.unwrap_or(DEFAULT_MTU);
        configure_interface(&name, lease.ip, gateway, mtu)?;

        let routes = Routes::install(
            &name,
            net::subnet_of(lease.ip, lease.netmask),
            lease.prefix_len(),
        )?;

        // macOS derives our default route from this service, so without it the
        // tunnel would come up unable to carry anything.
        let dns = Dns::install(
            &name,
            lease.ip,
            gateway,
            &lease.dns,
            lease.domain.as_deref(),
        )
        .context("publishing the tunnel as a network service")?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let reader = spawn_reader(utun.clone(), link, gateway, mtu, shutdown.clone());

        info!(
            "{name} up: {}/{} via {gateway}",
            lease.ip,
            lease.prefix_len()
        );
        Ok(Self {
            utun,
            lease,
            _routes: routes,
            _dns: dns,
            shutdown,
            reader: Some(reader),
        })
    }

    /// The sink that delivers inbound packets into this tunnel.
    pub fn sink(&self) -> Arc<dyn IpSink> {
        Arc::new(UtunSink(self.utun.clone()))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        info!("{} down", self.utun.name());
    }
}

/// Read IP packets from the tunnel, wrap them in Ethernet, hand them to the
/// RNDIS TX queue.
fn spawn_reader(
    utun: Arc<Utun>,
    link: &Link,
    gateway: Ipv4Addr,
    mtu: u16,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let tx = link.tx.clone();
    let arp = link.arp.clone();
    let host_mac = link.host_mac;

    std::thread::Builder::new()
        .name("utun-rx".into())
        .spawn(move || {
            let mut buf = vec![0u8; mtu as usize + crate::tun::utun::AF_HEADER_LEN + 64];
            while !shutdown.load(Ordering::Relaxed) {
                let len = match utun.read(&mut buf) {
                    Ok(0) => continue,
                    Ok(n) => n,
                    // A read timeout is the normal way this loop ticks.
                    Err(_) => continue,
                };

                // Without the gateway's MAC the frame has nowhere to go; the
                // request is queued and the packet dropped, as ARP resolution
                // normally does.
                let gateway_mac = {
                    let mut arp = arp.lock().expect("ARP lock");
                    match arp.lookup(gateway) {
                        Some(mac) => mac,
                        None => {
                            if let Some(request) = arp.request(gateway) {
                                tx.send(request);
                            }
                            continue;
                        }
                    }
                };

                tx.send(crate::netstack::ethernet::build(
                    gateway_mac,
                    host_mac,
                    crate::netstack::ethernet::ETHERTYPE_IPV4,
                    &buf[..len],
                ));
            }
        })
        .expect("spawning the utun reader")
}
