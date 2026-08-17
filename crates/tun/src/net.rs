//! Interface addressing and routing.
//!
//! These go through `ifconfig` and `route` rather than hand-built ioctl and
//! PF_ROUTE structs: the same thing every macOS VPN client does, and the
//! commands are auditable and easy to undo.

use std::net::Ipv4Addr;
use std::process::Command;

use anyhow::{bail, Context, Result};
use log::{debug, info, warn};

const IFCONFIG: &str = "/sbin/ifconfig";
const ROUTE: &str = "/sbin/route";

fn run(program: &str, args: &[&str]) -> Result<String> {
    debug!("{program} {}", args.join(" "));
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    if !output.status.success() {
        bail!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Give the tunnel an address, with the phone's gateway as the peer.
pub fn configure_interface(
    interface: &str,
    ip: Ipv4Addr,
    peer: Ipv4Addr,
    mtu: u16,
) -> Result<()> {
    run(
        IFCONFIG,
        &[
            interface,
            "inet",
            &ip.to_string(),
            &peer.to_string(),
            // A point-to-point tunnel; reachability comes from explicit routes.
            "netmask",
            "255.255.255.255",
            "mtu",
            &mtu.to_string(),
            "up",
        ],
    )?;
    info!("{interface}: {ip} peer {peer} mtu {mtu}");
    Ok(())
}

/// Routes we installed, removed on teardown.
pub struct Routes {
    destinations: Vec<String>,
}

impl Routes {
    /// Take over routing with the VPN split-default trick: two halves of the
    /// address space beat the physical default without replacing it, so
    /// teardown is a clean delete rather than a restore.
    pub fn install_default(interface: &str, gateway: Ipv4Addr, subnet: Ipv4Addr, prefix: u8) -> Result<Self> {
        let mut routes = Routes {
            destinations: Vec::new(),
        };

        // The phone's own subnet, reachable directly over the tunnel.
        let local = format!("{subnet}/{prefix}");
        routes.add(&local, &["-interface", interface])?;

        for half in ["0.0.0.0/1", "128.0.0.0/1"] {
            routes.add(half, &[&gateway.to_string()])?;
        }

        info!("{interface}: default route via {gateway}");
        Ok(routes)
    }

    fn add(&mut self, destination: &str, via: &[&str]) -> Result<()> {
        // Replace any leftover from an unclean shutdown.
        let _ = run(ROUTE, &["-n", "delete", "-net", destination]);

        let mut args = vec!["-n", "add", "-net", destination];
        args.extend_from_slice(via);
        run(ROUTE, &args)?;
        self.destinations.push(destination.to_string());
        Ok(())
    }

    /// Remove everything we added. Safe to call more than once.
    pub fn remove(&mut self) {
        for destination in self.destinations.drain(..) {
            if let Err(e) = run(ROUTE, &["-n", "delete", "-net", &destination]) {
                warn!("could not remove route {destination}: {e}");
            }
        }
    }
}

impl Drop for Routes {
    fn drop(&mut self) {
        self.remove();
    }
}

/// The network address of `ip` under `netmask`.
pub fn subnet_of(ip: Ipv4Addr, netmask: Ipv4Addr) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(ip) & u32::from(netmask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_the_network_address() {
        assert_eq!(
            subnet_of(Ipv4Addr::new(10, 71, 51, 112), Ipv4Addr::new(255, 255, 255, 0)),
            Ipv4Addr::new(10, 71, 51, 0)
        );
        assert_eq!(
            subnet_of(Ipv4Addr::new(192, 168, 42, 130), Ipv4Addr::new(255, 255, 255, 192)),
            Ipv4Addr::new(192, 168, 42, 128)
        );
    }
}
