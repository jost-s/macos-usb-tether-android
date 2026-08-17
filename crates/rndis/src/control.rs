//! RNDIS control channel: bring-up sequence and keepalives.
//!
//! Hardware-free — the caller supplies a `ControlTransport` over the USB
//! control endpoint, so the whole sequence is testable without a phone.

use std::time::{Duration, Instant};

use log::{debug, info, warn};

use crate::error::{Error, Result};
use crate::wire::{self, InitComplete};

/// How long we wait for a control response before giving up.
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on a single `await_response` wait, so a device that never raises
/// RESPONSE_AVAILABLE still gets polled.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Size we advertise as the largest transfer we can receive.
pub const HOST_MAX_TRANSFER_SIZE: u32 = 16384;

/// The USB control endpoint, as the RNDIS layer needs it.
pub trait ControlTransport {
    /// `SEND_ENCAPSULATED_COMMAND`
    fn send(&mut self, msg: &[u8]) -> Result<()>;
    /// `GET_ENCAPSULATED_RESPONSE`. An empty vec means nothing was pending.
    fn receive(&mut self) -> Result<Vec<u8>>;
    /// Block until the device raises RESPONSE_AVAILABLE, or `timeout` elapses.
    /// Implementations without an interrupt endpoint may just sleep.
    fn await_response(&mut self, timeout: Duration) -> Result<()>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    Unknown,
    Up,
    Down,
}

/// What the device told us during bring-up.
#[derive(Clone, Copy, Debug)]
pub struct Session {
    /// The MAC the gadget designates for the *host* side of the link, as
    /// Linux's rndis_host assigns to its own interface.
    pub host_mac: [u8; 6],
    /// Largest transfer the device will accept from us.
    pub device_max_transfer_size: u32,
    pub max_packets_per_transfer: u32,
    pub packet_alignment: u32,
}

pub struct Rndis<T> {
    transport: T,
    next_request_id: u32,
    link: LinkState,
}

impl<T: ControlTransport> Rndis<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            next_request_id: 1,
            link: LinkState::Unknown,
        }
    }

    pub fn link_state(&self) -> LinkState {
        self.link
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// INITIALIZE, learn the phone's MAC, then set the packet filter to bring
    /// the link up. This is the whole bring-up.
    pub fn bring_up(&mut self) -> Result<Session> {
        let init = self.initialize()?;
        info!(
            "RNDIS v{}.{} up: max transfer {} B, {} packets/transfer, {} B alignment",
            init.major_version,
            init.minor_version,
            init.max_transfer_size,
            init.max_packets_per_transfer,
            init.packet_alignment
        );
        if init.medium != 0 {
            warn!("device reports non-802.3 medium {}", init.medium);
        }

        let host_mac = self.query_mac()?;
        info!(
            "designated host MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            host_mac[0], host_mac[1], host_mac[2], host_mac[3], host_mac[4], host_mac[5]
        );

        self.set(
            wire::OID_GEN_CURRENT_PACKET_FILTER,
            &wire::DEFAULT_PACKET_FILTER.to_le_bytes(),
        )?;
        if self.link == LinkState::Unknown {
            self.link = LinkState::Up;
        }

        Ok(Session {
            host_mac,
            device_max_transfer_size: init.max_transfer_size,
            max_packets_per_transfer: init.max_packets_per_transfer.max(1),
            packet_alignment: init.packet_alignment.max(1),
        })
    }

    pub fn initialize(&mut self) -> Result<InitComplete> {
        let id = self.take_request_id();
        let reply = self.transact(
            wire::encode_init(id, HOST_MAX_TRANSFER_SIZE),
            wire::MSG_INIT_C,
            id,
        )?;
        let init = wire::decode_init_complete(&reply)?;
        if init.status != wire::STATUS_SUCCESS {
            return Err(Error::Status(init.status));
        }
        if init.max_transfer_size == 0 {
            return Err(Error::Malformed("device advertised a zero transfer size"));
        }
        Ok(init)
    }

    /// Prefer the permanent address, as Linux does; fall back to the current
    /// one for gadgets that only implement the latter.
    fn query_mac(&mut self) -> Result<[u8; 6]> {
        for oid in [
            wire::OID_802_3_PERMANENT_ADDRESS,
            wire::OID_802_3_CURRENT_ADDRESS,
        ] {
            match self.query(oid) {
                Ok(info) if info.len() >= 6 => {
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(&info[..6]);
                    if mac != [0; 6] {
                        return Ok(mac);
                    }
                    warn!("device returned an all-zero MAC for OID 0x{oid:08x}");
                }
                Ok(info) => warn!("short MAC reply ({} bytes) for OID 0x{oid:08x}", info.len()),
                Err(e) => warn!("MAC query 0x{oid:08x} failed: {e}"),
            }
        }
        Err(Error::Malformed("device did not report a usable MAC"))
    }

    pub fn query(&mut self, oid: u32) -> Result<Vec<u8>> {
        let id = self.take_request_id();
        let reply = self.transact(wire::encode_query(id, oid), wire::MSG_QUERY_C, id)?;
        let (_, status, info) = wire::decode_query_complete(&reply)?;
        if status != wire::STATUS_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(info.to_vec())
    }

    pub fn set(&mut self, oid: u32, data: &[u8]) -> Result<()> {
        let id = self.take_request_id();
        let reply = self.transact(wire::encode_set(id, oid, data), wire::MSG_SET_C, id)?;
        let (_, status) = wire::decode_completion(&reply)?;
        if status != wire::STATUS_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(())
    }

    pub fn keepalive(&mut self) -> Result<()> {
        let id = self.take_request_id();
        let reply = self.transact(wire::encode_keepalive(id), wire::MSG_KEEPALIVE_C, id)?;
        let (_, status) = wire::decode_completion(&reply)?;
        if status != wire::STATUS_SUCCESS {
            return Err(Error::Status(status));
        }
        Ok(())
    }

    /// Best-effort shutdown; the device has no completion for HALT.
    pub fn halt(&mut self) -> Result<()> {
        let id = self.take_request_id();
        self.transport.send(&wire::encode_halt(id))
    }

    fn take_request_id(&mut self) -> u32 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        id
    }

    /// Send a command and wait for its matching completion, servicing any
    /// device-initiated messages that arrive in between.
    fn transact(&mut self, msg: Vec<u8>, expect: u32, request_id: u32) -> Result<Vec<u8>> {
        self.transport.send(&msg)?;
        let deadline = Instant::now() + TRANSACTION_TIMEOUT;

        while Instant::now() < deadline {
            self.transport.await_response(POLL_INTERVAL)?;
            let reply = self.transport.receive()?;
            if reply.len() < 12 {
                continue;
            }

            let msg_type = wire::u32_at(&reply, 0)?;
            if msg_type == expect {
                let (id, _) = wire::decode_completion(&reply)?;
                if id == request_id {
                    return Ok(reply);
                }
                debug!("ignoring completion for stale request {id}");
                continue;
            }

            self.handle_unsolicited(msg_type, &reply)?;
        }

        Err(Error::NoResponse)
    }

    /// Messages the device sends on its own: link-state changes, and its own
    /// keepalive pings, which must be answered.
    fn handle_unsolicited(&mut self, msg_type: u32, reply: &[u8]) -> Result<()> {
        match msg_type {
            wire::MSG_INDICATE => {
                let status = wire::decode_indicate_status(reply)?;
                self.link = match status {
                    wire::STATUS_MEDIA_CONNECT => LinkState::Up,
                    wire::STATUS_MEDIA_DISCONNECT => LinkState::Down,
                    _ => self.link,
                };
                info!("link status 0x{status:08x} -> {:?}", self.link);
            }
            wire::MSG_KEEPALIVE => {
                let id = wire::u32_at(reply, 8)?;
                self.transport.send(&wire::encode_keepalive_complete(id))?;
            }
            other => debug!("ignoring unsolicited message 0x{other:08x}"),
        }
        Ok(())
    }

    /// Drain device-initiated messages without sending a command. Called
    /// between keepalives so link-down is noticed promptly.
    pub fn poll_unsolicited(&mut self) -> Result<LinkState> {
        loop {
            let reply = self.transport.receive()?;
            if reply.len() < 12 {
                return Ok(self.link);
            }
            let msg_type = wire::u32_at(&reply, 0)?;
            self.handle_unsolicited(msg_type, &reply)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Scripted device: each sent command pushes zero or more replies.
    #[derive(Default)]
    struct MockDevice {
        sent: Vec<Vec<u8>>,
        replies: VecDeque<Vec<u8>>,
        /// Replies queued ahead of any command, e.g. an INDICATE_STATUS.
        preloaded: VecDeque<Vec<u8>>,
        mac: [u8; 6],
        fail_permanent_address: bool,
    }

    impl MockDevice {
        fn new(mac: [u8; 6]) -> Self {
            Self {
                mac,
                ..Default::default()
            }
        }

        fn respond(&mut self, msg: &[u8]) {
            let msg_type = wire::u32_at(msg, 0).unwrap();
            let id = wire::u32_at(msg, 8).unwrap();
            match msg_type {
                wire::MSG_INIT => {
                    let mut r = Vec::new();
                    for v in [
                        wire::MSG_INIT_C,
                        52,
                        id,
                        wire::STATUS_SUCCESS,
                        1,
                        0,
                        0,
                        0,
                        8,
                        16384,
                        0,
                        0,
                        0,
                    ] {
                        r.extend_from_slice(&v.to_le_bytes());
                    }
                    self.replies.push_back(r);
                }
                wire::MSG_QUERY => {
                    let oid = wire::u32_at(msg, 12).unwrap();
                    let fail =
                        self.fail_permanent_address && oid == wire::OID_802_3_PERMANENT_ADDRESS;
                    let mut r = Vec::new();
                    if fail {
                        for v in [wire::MSG_QUERY_C, 24, id, 0xC000_0001, 0, 0] {
                            r.extend_from_slice(&v.to_le_bytes());
                        }
                    } else {
                        for v in [wire::MSG_QUERY_C, 30, id, wire::STATUS_SUCCESS, 6, 16] {
                            r.extend_from_slice(&v.to_le_bytes());
                        }
                        r.extend_from_slice(&self.mac);
                    }
                    self.replies.push_back(r);
                }
                wire::MSG_SET | wire::MSG_KEEPALIVE => {
                    let complete = if msg_type == wire::MSG_SET {
                        wire::MSG_SET_C
                    } else {
                        wire::MSG_KEEPALIVE_C
                    };
                    let mut r = Vec::new();
                    for v in [complete, 16, id, wire::STATUS_SUCCESS] {
                        r.extend_from_slice(&v.to_le_bytes());
                    }
                    self.replies.push_back(r);
                }
                _ => {}
            }
        }
    }

    impl ControlTransport for MockDevice {
        fn send(&mut self, msg: &[u8]) -> Result<()> {
            self.sent.push(msg.to_vec());
            self.respond(msg);
            Ok(())
        }

        fn receive(&mut self) -> Result<Vec<u8>> {
            Ok(self
                .preloaded
                .pop_front()
                .or_else(|| self.replies.pop_front())
                .unwrap_or_default())
        }

        fn await_response(&mut self, _timeout: Duration) -> Result<()> {
            Ok(())
        }
    }

    fn indicate(status: u32) -> Vec<u8> {
        let mut m = Vec::new();
        for v in [wire::MSG_INDICATE, 20, status, 0, 0] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m
    }

    #[test]
    fn bring_up_runs_init_query_set_in_order() {
        let mac = [0xAA, 0xBB, 0xCC, 0x01, 0x02, 0x03];
        let mut rndis = Rndis::new(MockDevice::new(mac));

        let session = rndis.bring_up().unwrap();
        assert_eq!(session.host_mac, mac);
        assert_eq!(session.device_max_transfer_size, 16384);

        let types: Vec<u32> = rndis
            .transport_mut()
            .sent
            .iter()
            .map(|m| wire::u32_at(m, 0).unwrap())
            .collect();
        assert_eq!(types, vec![wire::MSG_INIT, wire::MSG_QUERY, wire::MSG_SET]);

        // The SET must be the packet filter that brings the link up.
        let set = &rndis.transport_mut().sent[2];
        assert_eq!(
            wire::u32_at(set, 12).unwrap(),
            wire::OID_GEN_CURRENT_PACKET_FILTER
        );
        assert_eq!(wire::u32_at(set, 28).unwrap(), wire::DEFAULT_PACKET_FILTER);
    }

    #[test]
    fn falls_back_to_current_address_when_permanent_fails() {
        let mac = [1, 2, 3, 4, 5, 6];
        let mut device = MockDevice::new(mac);
        device.fail_permanent_address = true;
        let mut rndis = Rndis::new(device);

        assert_eq!(rndis.bring_up().unwrap().host_mac, mac);
    }

    #[test]
    fn an_indicate_status_between_command_and_completion_is_handled() {
        let mut device = MockDevice::new([1, 2, 3, 4, 5, 6]);
        device
            .preloaded
            .push_back(indicate(wire::STATUS_MEDIA_CONNECT));
        let mut rndis = Rndis::new(device);

        rndis.bring_up().unwrap();
        assert_eq!(rndis.link_state(), LinkState::Up);
    }

    #[test]
    fn a_device_keepalive_is_answered_during_a_transaction() {
        let mut device = MockDevice::new([1, 2, 3, 4, 5, 6]);
        let mut ping = Vec::new();
        for v in [wire::MSG_KEEPALIVE, 12, 0x4242] {
            ping.extend_from_slice(&v.to_le_bytes());
        }
        device.preloaded.push_back(ping);
        let mut rndis = Rndis::new(device);

        rndis.initialize().unwrap();

        let answered = rndis.transport_mut().sent.iter().any(|m| {
            wire::u32_at(m, 0).unwrap() == wire::MSG_KEEPALIVE_C
                && wire::u32_at(m, 8).unwrap() == 0x4242
        });
        assert!(answered, "device keepalive must be acknowledged");
    }

    #[test]
    fn media_disconnect_is_reported() {
        let mut device = MockDevice::new([1, 2, 3, 4, 5, 6]);
        device
            .preloaded
            .push_back(indicate(wire::STATUS_MEDIA_DISCONNECT));
        let mut rndis = Rndis::new(device);

        assert_eq!(rndis.poll_unsolicited().unwrap(), LinkState::Down);
    }

    #[test]
    fn init_failure_status_is_surfaced() {
        struct Failing;
        impl ControlTransport for Failing {
            fn send(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn receive(&mut self) -> Result<Vec<u8>> {
                let mut r = Vec::new();
                for v in [
                    wire::MSG_INIT_C,
                    52,
                    1,
                    0xC000_0001,
                    1,
                    0,
                    0,
                    0,
                    1,
                    16384,
                    0,
                    0,
                    0,
                ] {
                    r.extend_from_slice(&v.to_le_bytes());
                }
                Ok(r)
            }
            fn await_response(&mut self, _: Duration) -> Result<()> {
                Ok(())
            }
        }

        let err = Rndis::new(Failing).initialize().unwrap_err();
        assert!(matches!(err, Error::Status(0xC000_0001)));
    }

    #[test]
    fn a_silent_device_times_out_rather_than_hanging() {
        struct Silent;
        impl ControlTransport for Silent {
            fn send(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }
            fn receive(&mut self) -> Result<Vec<u8>> {
                Ok(Vec::new())
            }
            fn await_response(&mut self, timeout: Duration) -> Result<()> {
                std::thread::sleep(timeout.min(Duration::from_millis(10)));
                Ok(())
            }
        }

        let err = Rndis::new(Silent).initialize().unwrap_err();
        assert!(matches!(err, Error::NoResponse));
    }
}
