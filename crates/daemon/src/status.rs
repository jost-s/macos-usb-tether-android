//! Status shared with `rndis-tetherctl` over a unix socket.
//!
//! One line of tab-separated `key=value` pairs. Tabs rather than spaces
//! because values (the device name) contain spaces.

use std::io::{BufRead, BufReader, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use log::{debug, warn};

pub const SOCKET_PATH: &str = "/var/run/rndis-tether.sock";

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub device: Option<String>,
    pub link_up: bool,
    pub interface: Option<String>,
    pub address: Option<Ipv4Addr>,
    pub gateway: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub packets_in: u64,
    pub packets_out: u64,
}

impl Status {
    pub fn encode(&self) -> String {
        let dns = self
            .dns
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "state={}\tdevice={}\tinterface={}\taddress={}\tgateway={}\tdns={}\tin={}\tout={}\n",
            if self.link_up { "connected" } else { "waiting" },
            // A tab in the descriptor string would break the framing.
            self.device.as_deref().unwrap_or("-").replace('\t', " "),
            self.interface.as_deref().unwrap_or("-"),
            self.address.map_or("-".into(), |a| a.to_string()),
            self.gateway.map_or("-".into(), |a| a.to_string()),
            if dns.is_empty() { "-".into() } else { dns },
            self.packets_in,
            self.packets_out,
        )
    }
}

/// Serves the current status to anyone who connects.
pub struct StatusServer {
    shared: Arc<Mutex<Status>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl StatusServer {
    pub fn start() -> Result<Self> {
        // A socket left by a previous run would make bind fail.
        let _ = std::fs::remove_file(SOCKET_PATH);
        let listener =
            UnixListener::bind(SOCKET_PATH).with_context(|| format!("binding {SOCKET_PATH}"))?;
        listener.set_nonblocking(true)?;
        // The daemon is root, so the socket would otherwise be unreachable by
        // the user running `rndis-tetherctl`. It only serves status, and any
        // future mutating command must gate on the peer's uid instead.
        std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o666))
            .with_context(|| format!("relaxing permissions on {SOCKET_PATH}"))?;

        let shared = Arc::new(Mutex::new(Status::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread = {
            let shared = shared.clone();
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("ctl-server".into())
                .spawn(move || serve(listener, shared, shutdown))?
        };

        Ok(Self {
            shared,
            shutdown,
            thread: Some(thread),
        })
    }

    pub fn update(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.shared.lock().expect("status lock"));
    }
}

impl Drop for StatusServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(SOCKET_PATH);
    }
}

fn serve(listener: UnixListener, shared: Arc<Mutex<Status>>, shutdown: Arc<AtomicBool>) {
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let status = shared.lock().expect("status lock").clone();
                if let Err(e) = handle(stream, &status) {
                    debug!("ctl client: {e}");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                warn!("ctl accept failed: {e}");
                return;
            }
        }
    }
}

fn handle(mut stream: UnixStream, status: &Status) -> Result<()> {
    stream.set_nonblocking(false)?;
    // The only command so far is `status`; anything else gets the same reply.
    let mut line = String::new();
    let mut reader = BufReader::new(stream.try_clone()?);
    let _ = reader.read_line(&mut line);
    stream.write_all(status.encode().as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_connected_status() {
        let status = Status {
            device: Some("Xiaomi Redmi Note 10S".into()),
            link_up: true,
            interface: Some("utun4".into()),
            address: Some(Ipv4Addr::new(10, 71, 51, 112)),
            gateway: Some(Ipv4Addr::new(10, 71, 51, 57)),
            dns: vec![Ipv4Addr::new(10, 71, 51, 57)],
            packets_in: 12,
            packets_out: 34,
        };
        let line = status.encode();
        assert!(line.starts_with("state=connected\t"));
        assert!(line.contains("interface=utun4"));
        // The device name has spaces; it must stay one field.
        assert!(line.contains("device=Xiaomi Redmi Note 10S\t"));
        assert!(line.contains("address=10.71.51.112"));
        assert!(line.ends_with('\n'));
    }

    #[test]
    fn encodes_missing_fields_as_placeholders() {
        let line = Status::default().encode();
        assert!(line.starts_with("state=waiting\t"));
        assert!(line.contains("interface=-\t"));
        assert!(line.contains("dns=-\t"));
    }
}
