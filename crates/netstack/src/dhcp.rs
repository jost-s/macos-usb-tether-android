//! Minimal DHCP client for the phone's tether server (RFC 2131).
//!
//! Pure state machine: it consumes DHCP message bytes and produces the next
//! message to send. The caller wraps them in UDP/IP/Ethernet and does the
//! timing.

use std::net::Ipv4Addr;
use std::time::Duration;

use log::{debug, warn};

use crate::error::{Error, Result};
use crate::ethernet::MacAddr;

pub const CLIENT_PORT: u16 = 68;
pub const SERVER_PORT: u16 = 67;

const OP_REQUEST: u8 = 1;
const OP_REPLY: u8 = 2;
const HTYPE_ETHERNET: u8 = 1;
const FLAG_BROADCAST: u16 = 0x8000;
const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// BOOTP fixed header, before the magic cookie.
const BOOTP_LEN: usize = 236;

// Option codes we use.
const OPT_PAD: u8 = 0;
const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_DOMAIN_NAME: u8 = 15;
const OPT_INTERFACE_MTU: u8 = 26;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MESSAGE_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_PARAM_REQUEST: u8 = 55;
const OPT_RENEWAL_TIME: u8 = 58;
const OPT_CLIENT_ID: u8 = 61;
const OPT_END: u8 = 255;

// Message types.
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const DECLINE: u8 = 4;
const ACK: u8 = 5;
const NAK: u8 = 6;
const RELEASE: u8 = 7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub router: Option<Ipv4Addr>,
    pub dns: Vec<Ipv4Addr>,
    pub domain: Option<String>,
    pub mtu: Option<u16>,
    pub server_id: Ipv4Addr,
    pub lease_time: Duration,
    /// When to start renewing; defaults to half the lease.
    pub renewal_time: Duration,
}

impl Lease {
    pub fn prefix_len(&self) -> u8 {
        u32::from(self.netmask).count_ones() as u8
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Init,
    Selecting,
    Requesting,
    Bound,
    Renewing,
}

/// What the caller should do after feeding in a received message.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Nothing to do; the message was not for us or not interesting.
    Ignored,
    /// Send these DHCP bytes to the server.
    Send(Vec<u8>),
    /// A lease was obtained or renewed.
    Bound(Box<Lease>),
    /// The server refused; the caller should restart after a backoff.
    Nak,
}

pub struct DhcpClient {
    mac: MacAddr,
    xid: u32,
    state: State,
    /// The address we are trying to get, carried from OFFER into REQUEST.
    offered: Option<Ipv4Addr>,
    server_id: Option<Ipv4Addr>,
    lease: Option<Lease>,
}

impl DhcpClient {
    /// `xid` should be random per client so replies can be matched.
    pub fn new(mac: MacAddr, xid: u32) -> Self {
        Self {
            mac,
            xid,
            state: State::Init,
            offered: None,
            server_id: None,
            lease: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn lease(&self) -> Option<&Lease> {
        self.lease.as_ref()
    }

    /// Begin (or restart) discovery.
    pub fn discover(&mut self) -> Vec<u8> {
        self.state = State::Selecting;
        self.offered = None;
        self.server_id = None;
        // A fresh transaction id keeps stale replies from matching.
        self.xid = self.xid.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.build(DISCOVER, Ipv4Addr::UNSPECIFIED, None, None)
    }

    /// Re-send whatever the current state is waiting on.
    pub fn retransmit(&mut self) -> Option<Vec<u8>> {
        match self.state {
            State::Selecting => Some(self.build(DISCOVER, Ipv4Addr::UNSPECIFIED, None, None)),
            State::Requesting => {
                Some(self.build(REQUEST, Ipv4Addr::UNSPECIFIED, self.offered, self.server_id))
            }
            State::Renewing => {
                let ip = self.lease.as_ref()?.ip;
                Some(self.build(REQUEST, ip, None, None))
            }
            State::Init | State::Bound => None,
        }
    }

    /// Unicast REQUEST to extend the current lease.
    pub fn renew(&mut self) -> Option<Vec<u8>> {
        let ip = self.lease.as_ref()?.ip;
        self.state = State::Renewing;
        Some(self.build(REQUEST, ip, None, None))
    }

    /// Tell the server we are done. Best effort; no reply is expected.
    pub fn release(&mut self) -> Option<Vec<u8>> {
        let lease = self.lease.take()?;
        self.state = State::Init;
        // RELEASE carries no requested-IP option; ciaddr identifies the lease.
        Some(self.build(RELEASE, lease.ip, None, Some(lease.server_id)))
    }

    /// Tell the server the offered address is unusable (e.g. already in use).
    pub fn decline(&mut self) -> Option<Vec<u8>> {
        let ip = self.offered?;
        let server = self.server_id;
        self.state = State::Init;
        Some(self.build(DECLINE, Ipv4Addr::UNSPECIFIED, Some(ip), server))
    }

    pub fn handle(&mut self, msg: &[u8]) -> Result<Event> {
        let parsed = parse(msg)?;
        if parsed.xid != self.xid || parsed.chaddr != self.mac {
            return Ok(Event::Ignored);
        }

        match (self.state, parsed.message_type) {
            (State::Selecting, OFFER) => {
                self.offered = Some(parsed.yiaddr);
                self.server_id = parsed.server_id;
                self.state = State::Requesting;
                debug!("DHCP: offered {} by {:?}", parsed.yiaddr, parsed.server_id);
                Ok(Event::Send(self.build(
                    REQUEST,
                    Ipv4Addr::UNSPECIFIED,
                    Some(parsed.yiaddr),
                    parsed.server_id,
                )))
            }
            (State::Requesting | State::Renewing, ACK) => {
                let lease = self.lease_from(&parsed)?;
                self.state = State::Bound;
                self.lease = Some(lease.clone());
                Ok(Event::Bound(Box::new(lease)))
            }
            (State::Requesting | State::Renewing, NAK) => {
                warn!("DHCP: server refused our request");
                self.state = State::Init;
                self.lease = None;
                self.offered = None;
                Ok(Event::Nak)
            }
            _ => Ok(Event::Ignored),
        }
    }

    fn lease_from(&self, p: &Parsed) -> Result<Lease> {
        if p.yiaddr.is_unspecified() {
            return Err(Error::Malformed("ACK without an address"));
        }
        let server_id = p.server_id.or(self.server_id).unwrap_or(p.siaddr);
        // Android's tether server is also the gateway; fall back to it when the
        // router option is missing.
        let router = p.router.or(if server_id.is_unspecified() {
            None
        } else {
            Some(server_id)
        });
        let netmask = p.netmask.unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
        let lease_time = p.lease_time.unwrap_or(Duration::from_secs(3600));

        Ok(Lease {
            ip: p.yiaddr,
            netmask,
            router,
            dns: p.dns.clone(),
            domain: p.domain.clone(),
            mtu: p.mtu,
            server_id,
            lease_time,
            renewal_time: p.renewal_time.unwrap_or(lease_time / 2),
        })
    }

    fn build(
        &self,
        message_type: u8,
        ciaddr: Ipv4Addr,
        requested_ip: Option<Ipv4Addr>,
        server_id: Option<Ipv4Addr>,
    ) -> Vec<u8> {
        let mut m = Vec::with_capacity(300);
        m.push(OP_REQUEST);
        m.push(HTYPE_ETHERNET);
        m.push(6); // hardware address length
        m.push(0); // hops
        m.extend_from_slice(&self.xid.to_be_bytes());
        m.extend_from_slice(&0u16.to_be_bytes()); // secs
                                                  // Ask for broadcast replies: we have no address to receive unicast at
                                                  // until the lease is bound.
        let flags = if ciaddr.is_unspecified() {
            FLAG_BROADCAST
        } else {
            0
        };
        m.extend_from_slice(&flags.to_be_bytes());
        m.extend_from_slice(&ciaddr.octets());
        m.extend_from_slice(&[0; 4]); // yiaddr
        m.extend_from_slice(&[0; 4]); // siaddr
        m.extend_from_slice(&[0; 4]); // giaddr
        m.extend_from_slice(self.mac.as_bytes());
        m.resize(BOOTP_LEN, 0); // chaddr padding, sname, file
        m.extend_from_slice(&MAGIC_COOKIE);

        push_option(&mut m, OPT_MESSAGE_TYPE, &[message_type]);
        let mut client_id = vec![HTYPE_ETHERNET];
        client_id.extend_from_slice(self.mac.as_bytes());
        push_option(&mut m, OPT_CLIENT_ID, &client_id);
        if let Some(ip) = requested_ip {
            push_option(&mut m, OPT_REQUESTED_IP, &ip.octets());
        }
        if let Some(ip) = server_id {
            push_option(&mut m, OPT_SERVER_ID, &ip.octets());
        }
        if matches!(message_type, DISCOVER | REQUEST) {
            push_option(
                &mut m,
                OPT_PARAM_REQUEST,
                &[
                    OPT_SUBNET_MASK,
                    OPT_ROUTER,
                    OPT_DNS,
                    OPT_DOMAIN_NAME,
                    OPT_INTERFACE_MTU,
                    OPT_LEASE_TIME,
                    OPT_RENEWAL_TIME,
                ],
            );
        }
        m.push(OPT_END);

        // Pad to the 300-byte minimum some servers still expect.
        m.resize(m.len().max(300), 0);
        m
    }
}

fn push_option(m: &mut Vec<u8>, code: u8, data: &[u8]) {
    m.push(code);
    m.push(data.len() as u8);
    m.extend_from_slice(data);
}

#[derive(Debug)]
struct Parsed {
    xid: u32,
    yiaddr: Ipv4Addr,
    siaddr: Ipv4Addr,
    chaddr: MacAddr,
    message_type: u8,
    netmask: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    domain: Option<String>,
    mtu: Option<u16>,
    server_id: Option<Ipv4Addr>,
    lease_time: Option<Duration>,
    renewal_time: Option<Duration>,
}

impl Default for Parsed {
    fn default() -> Self {
        Self {
            xid: 0,
            yiaddr: Ipv4Addr::UNSPECIFIED,
            siaddr: Ipv4Addr::UNSPECIFIED,
            chaddr: MacAddr::ZERO,
            message_type: 0,
            netmask: None,
            router: None,
            dns: Vec::new(),
            domain: None,
            mtu: None,
            server_id: None,
            lease_time: None,
            renewal_time: None,
        }
    }
}

fn parse(buf: &[u8]) -> Result<Parsed> {
    if buf.len() < BOOTP_LEN + 4 {
        return Err(Error::Truncated);
    }
    if buf[0] != OP_REPLY {
        return Err(Error::Malformed("not a BOOTP reply"));
    }
    if buf[BOOTP_LEN..BOOTP_LEN + 4] != MAGIC_COOKIE {
        return Err(Error::Malformed("bad magic cookie"));
    }

    let mut p = Parsed {
        xid: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
        yiaddr: ipv4(&buf[16..20]),
        siaddr: ipv4(&buf[20..24]),
        chaddr: MacAddr::from_slice(&buf[28..34])?,
        ..Default::default()
    };

    let mut rest = &buf[BOOTP_LEN + 4..];
    while let Some((&code, tail)) = rest.split_first() {
        match code {
            OPT_PAD => {
                rest = tail;
                continue;
            }
            OPT_END => break,
            _ => {}
        }

        let Some((&len, tail)) = tail.split_first() else {
            return Err(Error::Truncated);
        };
        let len = len as usize;
        let data = tail.get(..len).ok_or(Error::Truncated)?;
        rest = &tail[len..];

        match code {
            OPT_MESSAGE_TYPE if len >= 1 => p.message_type = data[0],
            OPT_SUBNET_MASK if len >= 4 => p.netmask = Some(ipv4(data)),
            OPT_ROUTER if len >= 4 => p.router = Some(ipv4(data)),
            OPT_SERVER_ID if len >= 4 => p.server_id = Some(ipv4(data)),
            OPT_DNS => p.dns = data.chunks_exact(4).map(ipv4).collect(),
            OPT_DOMAIN_NAME => {
                p.domain = String::from_utf8(data.to_vec())
                    .ok()
                    .map(|s| s.trim_end_matches('\0').to_string())
                    .filter(|s| !s.is_empty())
            }
            OPT_INTERFACE_MTU if len >= 2 => p.mtu = Some(u16::from_be_bytes([data[0], data[1]])),
            OPT_LEASE_TIME if len >= 4 => p.lease_time = Some(seconds(data)),
            OPT_RENEWAL_TIME if len >= 4 => p.renewal_time = Some(seconds(data)),
            _ => {}
        }
    }

    if p.message_type == 0 {
        return Err(Error::Malformed("no DHCP message type"));
    }
    Ok(p)
}

fn ipv4(b: &[u8]) -> Ipv4Addr {
    Ipv4Addr::new(b[0], b[1], b[2], b[3])
}

fn seconds(b: &[u8]) -> Duration {
    Duration::from_secs(u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: MacAddr = MacAddr([0x02, 0x02, 0x24, 0x8f, 0xb0, 0xcd]);
    const SERVER: Ipv4Addr = Ipv4Addr::new(192, 168, 42, 129);
    const OFFERED: Ipv4Addr = Ipv4Addr::new(192, 168, 42, 130);

    /// A reply as Android's tether server sends it.
    fn server_reply(
        xid: u32,
        message_type: u8,
        yiaddr: Ipv4Addr,
        options: &[(u8, Vec<u8>)],
    ) -> Vec<u8> {
        let mut m = vec![OP_REPLY, HTYPE_ETHERNET, 6, 0];
        m.extend_from_slice(&xid.to_be_bytes());
        m.extend_from_slice(&[0; 4]); // secs, flags
        m.extend_from_slice(&[0; 4]); // ciaddr
        m.extend_from_slice(&yiaddr.octets());
        m.extend_from_slice(&SERVER.octets()); // siaddr
        m.extend_from_slice(&[0; 4]); // giaddr
        m.extend_from_slice(MAC.as_bytes());
        m.resize(BOOTP_LEN, 0);
        m.extend_from_slice(&MAGIC_COOKIE);

        push_option(&mut m, OPT_MESSAGE_TYPE, &[message_type]);
        for (code, data) in options {
            push_option(&mut m, *code, data);
        }
        m.push(OPT_END);
        m
    }

    fn full_offer(xid: u32, message_type: u8) -> Vec<u8> {
        server_reply(
            xid,
            message_type,
            OFFERED,
            &[
                (OPT_SUBNET_MASK, vec![255, 255, 255, 0]),
                (OPT_ROUTER, SERVER.octets().to_vec()),
                (OPT_DNS, vec![8, 8, 8, 8, 1, 1, 1, 1]),
                (OPT_SERVER_ID, SERVER.octets().to_vec()),
                (OPT_LEASE_TIME, 3600u32.to_be_bytes().to_vec()),
                (OPT_DOMAIN_NAME, b"lan".to_vec()),
                (OPT_INTERFACE_MTU, 1500u16.to_be_bytes().to_vec()),
            ],
        )
    }

    fn xid_of(msg: &[u8]) -> u32 {
        u32::from_be_bytes([msg[4], msg[5], msg[6], msg[7]])
    }

    fn option(msg: &[u8], code: u8) -> Option<Vec<u8>> {
        let mut rest = &msg[BOOTP_LEN + 4..];
        while let Some((&c, tail)) = rest.split_first() {
            if c == OPT_END {
                return None;
            }
            if c == OPT_PAD {
                rest = tail;
                continue;
            }
            let (&len, tail) = tail.split_first()?;
            let data = tail.get(..len as usize)?;
            if c == code {
                return Some(data.to_vec());
            }
            rest = &tail[len as usize..];
        }
        None
    }

    /// Drive the client through a full DISCOVER/OFFER/REQUEST/ACK exchange.
    fn bind() -> (DhcpClient, Lease) {
        let mut c = DhcpClient::new(MAC, 0x1234);
        let discover = c.discover();
        let xid = xid_of(&discover);

        let Event::Send(request) = c.handle(&full_offer(xid, OFFER)).unwrap() else {
            panic!("expected a REQUEST");
        };
        assert_eq!(option(&request, OPT_MESSAGE_TYPE), Some(vec![REQUEST]));

        let Event::Bound(lease) = c.handle(&full_offer(xid, ACK)).unwrap() else {
            panic!("expected a lease");
        };
        (c, *lease)
    }

    #[test]
    fn completes_the_four_way_exchange() {
        let (client, lease) = bind();
        assert_eq!(client.state(), State::Bound);
        assert_eq!(lease.ip, OFFERED);
        assert_eq!(lease.netmask, Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(lease.router, Some(SERVER));
        assert_eq!(
            lease.dns,
            vec![Ipv4Addr::new(8, 8, 8, 8), Ipv4Addr::new(1, 1, 1, 1)]
        );
        assert_eq!(lease.domain.as_deref(), Some("lan"));
        assert_eq!(lease.mtu, Some(1500));
        assert_eq!(lease.lease_time, Duration::from_secs(3600));
        assert_eq!(lease.renewal_time, Duration::from_secs(1800));
        assert_eq!(lease.prefix_len(), 24);
    }

    #[test]
    fn discover_asks_for_the_options_we_need() {
        let mut c = DhcpClient::new(MAC, 1);
        let d = c.discover();
        assert_eq!(option(&d, OPT_MESSAGE_TYPE), Some(vec![DISCOVER]));
        let params = option(&d, OPT_PARAM_REQUEST).unwrap();
        for wanted in [OPT_SUBNET_MASK, OPT_ROUTER, OPT_DNS, OPT_LEASE_TIME] {
            assert!(params.contains(&wanted), "must request option {wanted}");
        }
        assert!(d.len() >= 300, "must meet the minimum message size");
    }

    #[test]
    fn request_echoes_the_offered_address_and_server() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        let xid = xid_of(&c.discover());
        let Event::Send(request) = c.handle(&full_offer(xid, OFFER)).unwrap() else {
            panic!("expected a REQUEST");
        };
        assert_eq!(
            option(&request, OPT_REQUESTED_IP),
            Some(OFFERED.octets().to_vec())
        );
        assert_eq!(
            option(&request, OPT_SERVER_ID),
            Some(SERVER.octets().to_vec())
        );
    }

    #[test]
    fn ignores_replies_for_another_transaction() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        let xid = xid_of(&c.discover());
        assert_eq!(
            c.handle(&full_offer(xid ^ 0xFFFF, OFFER)).unwrap(),
            Event::Ignored
        );
        assert_eq!(c.state(), State::Selecting);
    }

    #[test]
    fn ignores_replies_addressed_to_another_client() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        let xid = xid_of(&c.discover());
        let mut offer = full_offer(xid, OFFER);
        offer[28] ^= 0xFF; // corrupt chaddr
        assert_eq!(c.handle(&offer).unwrap(), Event::Ignored);
    }

    #[test]
    fn a_nak_drops_the_lease() {
        let (mut c, _) = bind();
        c.renew().unwrap();
        let xid = c.xid;
        assert_eq!(
            c.handle(&server_reply(xid, NAK, Ipv4Addr::UNSPECIFIED, &[]))
                .unwrap(),
            Event::Nak
        );
        assert_eq!(c.state(), State::Init);
        assert!(c.lease().is_none());
    }

    #[test]
    fn renewal_rebinds_on_ack() {
        let (mut c, _) = bind();
        let renew = c.renew().unwrap();
        // Renewal is unicast from our address, so no broadcast flag.
        assert_eq!(&renew[10..12], &[0, 0]);
        assert_eq!(&renew[12..16], &OFFERED.octets());

        let xid = c.xid;
        assert!(matches!(
            c.handle(&full_offer(xid, ACK)).unwrap(),
            Event::Bound(_)
        ));
        assert_eq!(c.state(), State::Bound);
    }

    #[test]
    fn falls_back_to_the_server_as_gateway_when_no_router_option() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        let xid = xid_of(&c.discover());
        let minimal = |t| {
            server_reply(
                xid,
                t,
                OFFERED,
                &[(OPT_SERVER_ID, SERVER.octets().to_vec())],
            )
        };
        c.handle(&minimal(OFFER)).unwrap();
        let Event::Bound(lease) = c.handle(&minimal(ACK)).unwrap() else {
            panic!("expected a lease");
        };
        assert_eq!(lease.router, Some(SERVER));
        // Missing lease time gets a sane default rather than zero.
        assert!(lease.lease_time > Duration::ZERO);
    }

    #[test]
    fn rejects_an_ack_without_an_address() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        let xid = xid_of(&c.discover());
        c.handle(&full_offer(xid, OFFER)).unwrap();
        let bad = server_reply(
            xid,
            ACK,
            Ipv4Addr::UNSPECIFIED,
            &[(OPT_SERVER_ID, SERVER.octets().to_vec())],
        );
        assert!(matches!(c.handle(&bad), Err(Error::Malformed(_))));
    }

    #[test]
    fn rejects_truncated_and_corrupt_messages() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        c.discover();
        assert_eq!(c.handle(&[0u8; 100]), Err(Error::Truncated));

        let mut bad_cookie = full_offer(c.xid, OFFER);
        bad_cookie[BOOTP_LEN] = 0;
        assert!(matches!(c.handle(&bad_cookie), Err(Error::Malformed(_))));

        let mut not_a_reply = full_offer(c.xid, OFFER);
        not_a_reply[0] = OP_REQUEST;
        assert!(matches!(c.handle(&not_a_reply), Err(Error::Malformed(_))));
    }

    #[test]
    fn rejects_an_option_running_past_the_message() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        c.discover();
        let mut msg = full_offer(c.xid, OFFER);
        // Replace the terminator with an option claiming more data than remains.
        msg.pop();
        msg.extend_from_slice(&[OPT_DNS, 200]);
        assert!(matches!(c.handle(&msg), Err(Error::Truncated)));

        // A declared length that stops exactly at the end is still fine.
        let mut exact = full_offer(c.xid, OFFER);
        exact.pop();
        exact.extend_from_slice(&[OPT_DNS, 4, 9, 9, 9, 9]);
        assert!(c.handle(&exact).is_ok());
    }

    #[test]
    fn rejects_a_message_with_no_type_option() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        c.discover();
        let mut m = vec![OP_REPLY, HTYPE_ETHERNET, 6, 0];
        m.extend_from_slice(&c.xid.to_be_bytes());
        m.resize(BOOTP_LEN, 0);
        m.extend_from_slice(&MAGIC_COOKIE);
        m.push(OPT_END);
        assert!(matches!(c.handle(&m), Err(Error::Malformed(_))));
    }

    #[test]
    fn a_new_discover_uses_a_fresh_transaction_id() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        let first = xid_of(&c.discover());
        let second = xid_of(&c.discover());
        assert_ne!(first, second);
    }

    #[test]
    fn retransmit_repeats_the_pending_message_only() {
        let mut c = DhcpClient::new(MAC, 0x1234);
        assert!(c.retransmit().is_none(), "nothing pending in Init");

        let xid = xid_of(&c.discover());
        assert_eq!(
            option(&c.retransmit().unwrap(), OPT_MESSAGE_TYPE),
            Some(vec![DISCOVER])
        );

        c.handle(&full_offer(xid, OFFER)).unwrap();
        assert_eq!(
            option(&c.retransmit().unwrap(), OPT_MESSAGE_TYPE),
            Some(vec![REQUEST])
        );

        c.handle(&full_offer(xid, ACK)).unwrap();
        assert!(c.retransmit().is_none(), "nothing to retransmit once bound");
    }

    #[test]
    fn release_clears_the_lease() {
        let (mut c, _) = bind();
        let msg = c.release().unwrap();
        assert_eq!(option(&msg, OPT_MESSAGE_TYPE), Some(vec![RELEASE]));
        assert!(c.lease().is_none());
        assert!(c.release().is_none(), "releasing twice is a no-op");
    }
}
