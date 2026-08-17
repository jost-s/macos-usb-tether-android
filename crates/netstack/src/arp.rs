//! ARP: we answer requests for our own address and resolve the gateway's MAC.
//!
//! The daemon owns both sides of ARP because utun never sees layer 2.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use log::debug;

use crate::error::{Error, Result};
use crate::ethernet::{self, MacAddr, ETHERTYPE_ARP};

pub const OP_REQUEST: u16 = 1;
pub const OP_REPLY: u16 = 2;

const HTYPE_ETHERNET: u16 = 1;
const PACKET_LEN: usize = 28;

/// How long a learned mapping stays valid.
const ENTRY_TTL: Duration = Duration::from_secs(300);
/// Minimum gap between repeat requests for the same address.
const RETRY_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArpPacket {
    pub op: u16,
    pub sender_mac: MacAddr,
    pub sender_ip: Ipv4Addr,
    pub target_mac: MacAddr,
    pub target_ip: Ipv4Addr,
}

pub fn parse(buf: &[u8]) -> Result<ArpPacket> {
    if buf.len() < PACKET_LEN {
        return Err(Error::Truncated);
    }
    let htype = u16::from_be_bytes([buf[0], buf[1]]);
    let ptype = u16::from_be_bytes([buf[2], buf[3]]);
    if htype != HTYPE_ETHERNET || ptype != ethernet::ETHERTYPE_IPV4 {
        return Err(Error::Malformed("not Ethernet/IPv4 ARP"));
    }
    if buf[4] != 6 || buf[5] != 4 {
        return Err(Error::Malformed("unexpected ARP address sizes"));
    }

    Ok(ArpPacket {
        op: u16::from_be_bytes([buf[6], buf[7]]),
        sender_mac: MacAddr::from_slice(&buf[8..14])?,
        sender_ip: ipv4(&buf[14..18]),
        target_mac: MacAddr::from_slice(&buf[18..24])?,
        target_ip: ipv4(&buf[24..28]),
    })
}

pub fn build(p: &ArpPacket) -> Vec<u8> {
    let mut b = Vec::with_capacity(PACKET_LEN);
    b.extend_from_slice(&HTYPE_ETHERNET.to_be_bytes());
    b.extend_from_slice(&ethernet::ETHERTYPE_IPV4.to_be_bytes());
    b.push(6);
    b.push(4);
    b.extend_from_slice(&p.op.to_be_bytes());
    b.extend_from_slice(p.sender_mac.as_bytes());
    b.extend_from_slice(&p.sender_ip.octets());
    b.extend_from_slice(p.target_mac.as_bytes());
    b.extend_from_slice(&p.target_ip.octets());
    b
}

fn ipv4(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

#[derive(Clone, Copy)]
struct Entry {
    mac: MacAddr,
    learned: Instant,
}

/// ARP responder and resolver for one link.
pub struct Arp {
    host_mac: MacAddr,
    host_ip: Ipv4Addr,
    cache: HashMap<Ipv4Addr, Entry>,
    last_request: HashMap<Ipv4Addr, Instant>,
}

impl Arp {
    pub fn new(host_mac: MacAddr, host_ip: Ipv4Addr) -> Self {
        Self {
            host_mac,
            host_ip,
            cache: HashMap::new(),
            last_request: HashMap::new(),
        }
    }

    /// Our address changes when DHCP hands us a lease.
    pub fn set_host_ip(&mut self, ip: Ipv4Addr) {
        self.host_ip = ip;
    }

    pub fn lookup(&self, ip: Ipv4Addr) -> Option<MacAddr> {
        self.cache
            .get(&ip)
            .filter(|e| e.learned.elapsed() < ENTRY_TTL)
            .map(|e| e.mac)
    }

    pub fn insert(&mut self, ip: Ipv4Addr, mac: MacAddr) {
        self.cache.insert(
            ip,
            Entry {
                mac,
                learned: Instant::now(),
            },
        );
    }

    /// A broadcast ARP request for `ip`, rate-limited to one per second.
    pub fn request(&mut self, ip: Ipv4Addr) -> Option<Vec<u8>> {
        let now = Instant::now();
        if let Some(sent) = self.last_request.get(&ip) {
            if now.duration_since(*sent) < RETRY_INTERVAL {
                return None;
            }
        }
        self.last_request.insert(ip, now);

        let packet = build(&ArpPacket {
            op: OP_REQUEST,
            sender_mac: self.host_mac,
            sender_ip: self.host_ip,
            target_mac: MacAddr::ZERO,
            target_ip: ip,
        });
        Some(ethernet::build(
            MacAddr::BROADCAST,
            self.host_mac,
            ETHERTYPE_ARP,
            &packet,
        ))
    }

    /// Handle an inbound ARP payload, learning what it tells us and returning a
    /// reply frame when the request is for our address.
    pub fn handle(&mut self, payload: &[u8]) -> Result<Option<Vec<u8>>> {
        let p = parse(payload)?;

        // Learn from any ARP traffic, as hosts normally do — but never from an
        // unspecified sender, which would poison the cache.
        if !p.sender_ip.is_unspecified() && !p.sender_mac.is_multicast() {
            self.insert(p.sender_ip, p.sender_mac);
        }

        if p.op != OP_REQUEST || p.target_ip != self.host_ip || self.host_ip.is_unspecified() {
            return Ok(None);
        }

        debug!("ARP: replying to {} for {}", p.sender_ip, p.target_ip);
        let reply = build(&ArpPacket {
            op: OP_REPLY,
            sender_mac: self.host_mac,
            sender_ip: self.host_ip,
            target_mac: p.sender_mac,
            target_ip: p.sender_ip,
        });
        Ok(Some(ethernet::build(
            p.sender_mac,
            self.host_mac,
            ETHERTYPE_ARP,
            &reply,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_MAC: MacAddr = MacAddr([0x02, 0, 0, 0, 0, 1]);
    const GW_MAC: MacAddr = MacAddr([0x3e, 0x02, 0x24, 0x8f, 0xb0, 0xcc]);
    const HOST_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 42, 130);
    const GW_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 42, 129);

    fn request_from_gateway(target: Ipv4Addr) -> Vec<u8> {
        build(&ArpPacket {
            op: OP_REQUEST,
            sender_mac: GW_MAC,
            sender_ip: GW_IP,
            target_mac: MacAddr::ZERO,
            target_ip: target,
        })
    }

    #[test]
    fn round_trips_a_packet() {
        let p = ArpPacket {
            op: OP_REPLY,
            sender_mac: GW_MAC,
            sender_ip: GW_IP,
            target_mac: HOST_MAC,
            target_ip: HOST_IP,
        };
        assert_eq!(parse(&build(&p)).unwrap(), p);
    }

    #[test]
    fn replies_to_a_request_for_our_address() {
        let mut arp = Arp::new(HOST_MAC, HOST_IP);
        let frame = arp.handle(&request_from_gateway(HOST_IP)).unwrap().unwrap();

        let eth = ethernet::parse(&frame).unwrap();
        assert_eq!(eth.dst, GW_MAC);
        assert_eq!(eth.src, HOST_MAC);
        assert_eq!(eth.ethertype, ETHERTYPE_ARP);

        let reply = parse(eth.payload).unwrap();
        assert_eq!(reply.op, OP_REPLY);
        assert_eq!(reply.sender_mac, HOST_MAC);
        assert_eq!(reply.sender_ip, HOST_IP);
        assert_eq!(reply.target_ip, GW_IP);
    }

    #[test]
    fn ignores_requests_for_other_addresses() {
        let mut arp = Arp::new(HOST_MAC, HOST_IP);
        let other = Ipv4Addr::new(192, 168, 42, 200);
        assert!(arp.handle(&request_from_gateway(other)).unwrap().is_none());
    }

    #[test]
    fn learns_the_sender_from_any_arp_traffic() {
        let mut arp = Arp::new(HOST_MAC, HOST_IP);
        arp.handle(&request_from_gateway(HOST_IP)).unwrap();
        assert_eq!(arp.lookup(GW_IP), Some(GW_MAC));
    }

    #[test]
    fn does_not_learn_from_an_unspecified_sender() {
        // A DHCP-probe ARP from 0.0.0.0 must not become a cache entry.
        let mut arp = Arp::new(HOST_MAC, HOST_IP);
        let probe = build(&ArpPacket {
            op: OP_REQUEST,
            sender_mac: GW_MAC,
            sender_ip: Ipv4Addr::UNSPECIFIED,
            target_mac: MacAddr::ZERO,
            target_ip: HOST_IP,
        });
        arp.handle(&probe).unwrap();
        assert_eq!(arp.lookup(Ipv4Addr::UNSPECIFIED), None);
    }

    #[test]
    fn stays_silent_before_we_have_an_address() {
        let mut arp = Arp::new(HOST_MAC, Ipv4Addr::UNSPECIFIED);
        let probe = request_from_gateway(Ipv4Addr::UNSPECIFIED);
        assert!(arp.handle(&probe).unwrap().is_none());
    }

    #[test]
    fn rate_limits_repeat_requests() {
        let mut arp = Arp::new(HOST_MAC, HOST_IP);
        assert!(arp.request(GW_IP).is_some());
        assert!(arp.request(GW_IP).is_none(), "second request is suppressed");
    }

    #[test]
    fn rejects_truncated_and_non_ipv4_packets() {
        let mut arp = Arp::new(HOST_MAC, HOST_IP);
        assert_eq!(arp.handle(&[0u8; 27]), Err(Error::Truncated));

        let mut wrong = build(&ArpPacket {
            op: OP_REQUEST,
            sender_mac: GW_MAC,
            sender_ip: GW_IP,
            target_mac: MacAddr::ZERO,
            target_ip: HOST_IP,
        });
        wrong[2..4].copy_from_slice(&0x86DDu16.to_be_bytes());
        assert!(matches!(arp.handle(&wrong), Err(Error::Malformed(_))));
    }

    #[test]
    fn rejects_implausible_address_sizes() {
        let mut p = build(&ArpPacket {
            op: OP_REQUEST,
            sender_mac: GW_MAC,
            sender_ip: GW_IP,
            target_mac: MacAddr::ZERO,
            target_ip: HOST_IP,
        });
        p[4] = 8;
        assert!(matches!(parse(&p), Err(Error::Malformed(_))));
    }
}
