//! The `utun` device: a point-to-point, IP-only interface.
//!
//! Every read and write carries a 4-byte address-family header that is not
//! part of the packet.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use anyhow::{bail, Context, Result};
use log::debug;

const UTUN_CONTROL_NAME: &[u8] = b"com.apple.net.utun_control";
/// The 4-byte address family prefix on every packet.
pub const AF_HEADER_LEN: usize = 4;
/// Highest unit we will probe when no unit is specified.
const MAX_UNIT: u32 = 255;

pub struct Utun {
    fd: OwnedFd,
    name: String,
}

/// Whether the failure means "this unit is taken", as opposed to something that
/// would defeat every unit equally — not being root, above all.
fn is_unit_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<io::Error>()
        .is_some_and(|e| matches!(e.raw_os_error(), Some(libc::EBUSY) | Some(libc::EADDRINUSE)))
}

impl Utun {
    /// Open the first free utun device.
    pub fn open() -> Result<Self> {
        let mut last_error = None;
        for unit in 0..MAX_UNIT {
            match Self::open_unit(unit) {
                Ok(utun) => return Ok(utun),
                Err(e) => {
                    // Only a busy unit is worth retrying; anything else applies
                    // to every unit and would just repeat 255 times.
                    if !is_unit_busy(&e) {
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }
        bail!(
            "no free utun device: {}",
            last_error.map_or_else(|| "unknown".to_string(), |e| e.to_string())
        )
    }

    fn open_unit(unit: u32) -> Result<Self> {
        // SAFETY: plain syscalls; every pointer below refers to a live local.
        unsafe {
            let fd = libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL);
            if fd < 0 {
                return Err(io::Error::last_os_error()).context("creating a PF_SYSTEM socket");
            }
            let fd = OwnedFd::from_raw_fd(fd);

            let mut info: libc::ctl_info = mem::zeroed();
            info.ctl_name[..UTUN_CONTROL_NAME.len()]
                .copy_from_slice(&*(UTUN_CONTROL_NAME as *const [u8] as *const [libc::c_char]));
            if libc::ioctl(fd.as_raw_fd(), libc::CTLIOCGINFO, &mut info) < 0 {
                return Err(io::Error::last_os_error()).context("looking up the utun control id");
            }

            let mut addr: libc::sockaddr_ctl = mem::zeroed();
            addr.sc_len = mem::size_of::<libc::sockaddr_ctl>() as u8;
            addr.sc_family = libc::AF_SYSTEM as u8;
            addr.ss_sysaddr = libc::AF_SYS_CONTROL as u16;
            addr.sc_id = info.ctl_id;
            // The kernel numbers units from 1, so utun0 is unit 1.
            addr.sc_unit = unit + 1;

            if libc::connect(
                fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                mem::size_of::<libc::sockaddr_ctl>() as libc::socklen_t,
            ) < 0
            {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("connecting to utun{unit}"));
            }

            let name = Self::interface_name(fd.as_raw_fd())?;
            debug!("opened {name}");
            Ok(Self { fd, name })
        }
    }

    /// SAFETY: `fd` must be a connected utun socket.
    unsafe fn interface_name(fd: RawFd) -> Result<String> {
        let mut buf = [0u8; 32];
        let mut len = buf.len() as libc::socklen_t;
        if libc::getsockopt(
            fd,
            libc::SYSPROTO_CONTROL,
            libc::UTUN_OPT_IFNAME,
            buf.as_mut_ptr().cast(),
            &mut len,
        ) < 0
        {
            return Err(io::Error::last_os_error()).context("reading the utun interface name");
        }
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        Ok(String::from_utf8_lossy(&buf[..end]).into_owned())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Read one IP packet, without its address-family header. Returns the
    /// packet length within `buf`.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        // SAFETY: `buf` is a live slice of the length passed.
        let n = unsafe { libc::read(self.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error()).context("reading from utun");
        }
        let n = n as usize;
        if n < AF_HEADER_LEN {
            return Ok(0);
        }
        buf.copy_within(AF_HEADER_LEN..n, 0);
        Ok(n - AF_HEADER_LEN)
    }

    /// Write one IP packet, prepending the address-family header.
    pub fn write(&self, packet: &[u8]) -> Result<()> {
        let family = match packet.first().map(|b| b >> 4) {
            Some(4) => libc::AF_INET as u32,
            Some(6) => libc::AF_INET6 as u32,
            _ => return Ok(()),
        };

        let mut buf = Vec::with_capacity(AF_HEADER_LEN + packet.len());
        buf.extend_from_slice(&family.to_be_bytes());
        buf.extend_from_slice(packet);

        // SAFETY: `buf` is a live slice of the length passed.
        let n = unsafe { libc::write(self.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            return Err(io::Error::last_os_error()).context("writing to utun");
        }
        Ok(())
    }

    /// Make reads return after `millis` instead of blocking forever, so the
    /// reader thread can notice shutdown.
    pub fn set_read_timeout(&self, millis: i64) -> Result<()> {
        let tv = libc::timeval {
            tv_sec: millis / 1000,
            tv_usec: ((millis % 1000) * 1000) as i32,
        };
        // SAFETY: `tv` is a live local of the size passed.
        let rc = unsafe {
            libc::setsockopt(
                self.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const _ as *const libc::c_void,
                mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error()).context("setting the utun read timeout");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_ignores_packets_that_are_not_ip() {
        // No fd is touched: the family check rejects these first.
        let utun = Utun {
            // SAFETY: never used, since `write` returns before any syscall.
            fd: unsafe { OwnedFd::from_raw_fd(libc::dup(0)) },
            name: "test".into(),
        };
        assert!(utun.write(&[]).is_ok());
        assert!(utun.write(&[0x00]).is_ok());
    }
}
