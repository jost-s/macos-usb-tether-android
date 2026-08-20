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
pub fn configure_interface(interface: &str, ip: Ipv4Addr, peer: Ipv4Addr, mtu: u16) -> Result<()> {
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
    /// Install the on-link route for the phone's subnet.
    ///
    /// The default route deliberately is not ours. macOS installs one from the
    /// service `Dns` publishes, for as long as that service ranks primary, and
    /// steps aside when a VPN outranks it. Claiming the address space directly —
    /// the `0.0.0.0/1` plus `128.0.0.0/1` pair VPNs use — would beat any VPN
    /// layered on the tether and send its traffic out in the clear.
    pub fn install(interface: &str, subnet: Ipv4Addr, prefix: u8) -> Result<Self> {
        let mut routes = Routes {
            destinations: Vec::new(),
        };

        // The /32 point-to-point address reaches the router, but not the rest of
        // the subnet behind it.
        let local = format!("{subnet}/{prefix}");
        routes.add(&local, &["-interface", interface])?;

        info!("{interface}: on-link route for {local}");
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
            subnet_of(
                Ipv4Addr::new(10, 71, 51, 112),
                Ipv4Addr::new(255, 255, 255, 0)
            ),
            Ipv4Addr::new(10, 71, 51, 0)
        );
        assert_eq!(
            subnet_of(
                Ipv4Addr::new(192, 168, 42, 130),
                Ipv4Addr::new(255, 255, 255, 192)
            ),
            Ipv4Addr::new(192, 168, 42, 128)
        );
    }
}
