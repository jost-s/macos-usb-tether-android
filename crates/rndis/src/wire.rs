//! RNDIS constants and message encoding/decoding.
//!
//! Values and struct layouts follow Linux `include/linux/rndis.h` and
//! `include/linux/usb/rndis_host.h`. Every field is little-endian u32.

use crate::error::{Error, Result};

pub const MSG_COMPLETION: u32 = 0x8000_0000;
pub const MSG_PACKET: u32 = 0x0000_0001;
pub const MSG_INIT: u32 = 0x0000_0002;
pub const MSG_INIT_C: u32 = MSG_INIT | MSG_COMPLETION;
pub const MSG_HALT: u32 = 0x0000_0003;
pub const MSG_QUERY: u32 = 0x0000_0004;
pub const MSG_QUERY_C: u32 = MSG_QUERY | MSG_COMPLETION;
pub const MSG_SET: u32 = 0x0000_0005;
pub const MSG_SET_C: u32 = MSG_SET | MSG_COMPLETION;
pub const MSG_RESET: u32 = 0x0000_0006;
pub const MSG_RESET_C: u32 = MSG_RESET | MSG_COMPLETION;
pub const MSG_INDICATE: u32 = 0x0000_0007;
pub const MSG_KEEPALIVE: u32 = 0x0000_0008;
pub const MSG_KEEPALIVE_C: u32 = MSG_KEEPALIVE | MSG_COMPLETION;

pub const STATUS_SUCCESS: u32 = 0x0000_0000;
pub const STATUS_MEDIA_CONNECT: u32 = 0x4001_000B;
pub const STATUS_MEDIA_DISCONNECT: u32 = 0x4001_000C;

pub const OID_GEN_MAXIMUM_FRAME_SIZE: u32 = 0x0001_0106;
pub const OID_GEN_LINK_SPEED: u32 = 0x0001_0107;
pub const OID_GEN_CURRENT_PACKET_FILTER: u32 = 0x0001_010E;
pub const OID_GEN_PHYSICAL_MEDIUM: u32 = 0x0001_0202;
pub const OID_802_3_PERMANENT_ADDRESS: u32 = 0x0101_0101;
pub const OID_802_3_CURRENT_ADDRESS: u32 = 0x0101_0102;

pub const PACKET_TYPE_DIRECTED: u32 = 0x0000_0001;
pub const PACKET_TYPE_MULTICAST: u32 = 0x0000_0002;
pub const PACKET_TYPE_ALL_MULTICAST: u32 = 0x0000_0004;
pub const PACKET_TYPE_BROADCAST: u32 = 0x0000_0008;
pub const PACKET_TYPE_PROMISCUOUS: u32 = 0x0000_0020;

/// Filter that brings the link up. Matches HoRNDIS; broader than Linux's
/// default, which some Android builds need to forward our synthesized MAC.
pub const DEFAULT_PACKET_FILTER: u32 = PACKET_TYPE_DIRECTED
    | PACKET_TYPE_MULTICAST
    | PACKET_TYPE_ALL_MULTICAST
    | PACKET_TYPE_BROADCAST
    | PACKET_TYPE_PROMISCUOUS;

/// `struct rndis_data_hdr` — the PACKET_MSG header.
pub const DATA_HEADER_LEN: usize = 44;

/// Offsets inside control messages are relative to the end of `msg_len`.
pub const OFFSET_BASE: usize = 8;

/// Largest control message we will send or accept, as Linux's
/// `CONTROL_BUFFER_SIZE`.
pub const MAX_CONTROL_MSG: usize = 1024;

/// First word of the interrupt-endpoint notification meaning a control
/// response can be fetched with GET_ENCAPSULATED_RESPONSE.
pub const RESPONSE_AVAILABLE: u32 = 0x0000_0001;

/// `USB_CDC_SEND_ENCAPSULATED_COMMAND`
pub const REQ_SEND_ENCAPSULATED: u8 = 0x00;
/// `USB_CDC_GET_ENCAPSULATED_RESPONSE`
pub const REQ_GET_ENCAPSULATED: u8 = 0x01;

pub fn u32_at(buf: &[u8], offset: usize) -> Result<u32> {
    buf.get(offset..offset + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(Error::Truncated)
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// `REMOTE_NDIS_INITIALIZE_MSG`
pub fn encode_init(request_id: u32, max_transfer_size: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(24);
    put_u32(&mut m, MSG_INIT);
    put_u32(&mut m, 24);
    put_u32(&mut m, request_id);
    put_u32(&mut m, 1); // major version
    put_u32(&mut m, 0); // minor version
    put_u32(&mut m, max_transfer_size);
    m
}

/// `REMOTE_NDIS_QUERY_MSG` with an empty information buffer.
pub fn encode_query(request_id: u32, oid: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(28);
    put_u32(&mut m, MSG_QUERY);
    put_u32(&mut m, 28);
    put_u32(&mut m, request_id);
    put_u32(&mut m, oid);
    put_u32(&mut m, 0); // information buffer length
    put_u32(&mut m, 0); // information buffer offset
    put_u32(&mut m, 0); // device VC handle
    m
}

/// `REMOTE_NDIS_SET_MSG`
pub fn encode_set(request_id: u32, oid: u32, data: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(28 + data.len());
    put_u32(&mut m, MSG_SET);
    put_u32(&mut m, (28 + data.len()) as u32);
    put_u32(&mut m, request_id);
    put_u32(&mut m, oid);
    put_u32(&mut m, data.len() as u32);
    put_u32(&mut m, (28 - OFFSET_BASE) as u32);
    put_u32(&mut m, 0); // device VC handle
    m.extend_from_slice(data);
    m
}

/// `REMOTE_NDIS_KEEPALIVE_MSG`
pub fn encode_keepalive(request_id: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(12);
    put_u32(&mut m, MSG_KEEPALIVE);
    put_u32(&mut m, 12);
    put_u32(&mut m, request_id);
    m
}

/// `REMOTE_NDIS_HALT_MSG`
pub fn encode_halt(request_id: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(12);
    put_u32(&mut m, MSG_HALT);
    put_u32(&mut m, 12);
    put_u32(&mut m, request_id);
    m
}

/// `REMOTE_NDIS_KEEPALIVE_CMPLT`, sent when the *device* pings us.
pub fn encode_keepalive_complete(request_id: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(16);
    put_u32(&mut m, MSG_KEEPALIVE_C);
    put_u32(&mut m, 16);
    put_u32(&mut m, request_id);
    put_u32(&mut m, STATUS_SUCCESS);
    m
}

/// What the device told us in `REMOTE_NDIS_INITIALIZE_CMPLT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InitComplete {
    pub request_id: u32,
    pub status: u32,
    pub major_version: u32,
    pub minor_version: u32,
    pub medium: u32,
    pub max_packets_per_transfer: u32,
    pub max_transfer_size: u32,
    /// Already expanded from the exponent the device sends.
    pub packet_alignment: u32,
}

pub fn decode_init_complete(buf: &[u8]) -> Result<InitComplete> {
    if u32_at(buf, 0)? != MSG_INIT_C {
        return Err(Error::UnexpectedMessage(u32_at(buf, 0)?));
    }
    // 13 u32 fields; some devices send a shorter message, so require only the
    // fields we read.
    let alignment_exponent = u32_at(buf, 40)?;
    Ok(InitComplete {
        request_id: u32_at(buf, 8)?,
        status: u32_at(buf, 12)?,
        major_version: u32_at(buf, 16)?,
        minor_version: u32_at(buf, 20)?,
        medium: u32_at(buf, 28)?,
        max_packets_per_transfer: u32_at(buf, 32)?,
        max_transfer_size: u32_at(buf, 36)?,
        // Guard the shift: a hostile or broken exponent must not panic.
        packet_alignment: if alignment_exponent < 8 {
            1 << alignment_exponent
        } else {
            1
        },
    })
}

/// Information buffer of a `REMOTE_NDIS_QUERY_CMPLT`, bounds-checked against
/// the received message.
pub fn decode_query_complete(buf: &[u8]) -> Result<(u32, u32, &[u8])> {
    if u32_at(buf, 0)? != MSG_QUERY_C {
        return Err(Error::UnexpectedMessage(u32_at(buf, 0)?));
    }
    let request_id = u32_at(buf, 8)?;
    let status = u32_at(buf, 12)?;
    let len = u32_at(buf, 16)? as usize;
    let offset = u32_at(buf, 20)? as usize;

    if status != STATUS_SUCCESS {
        return Ok((request_id, status, &[]));
    }

    let start = OFFSET_BASE.checked_add(offset).ok_or(Error::Truncated)?;
    let end = start.checked_add(len).ok_or(Error::Truncated)?;
    let info = buf.get(start..end).ok_or(Error::Truncated)?;
    Ok((request_id, status, info))
}

/// Request id and status of any `*_CMPLT` message.
pub fn decode_completion(buf: &[u8]) -> Result<(u32, u32)> {
    Ok((u32_at(buf, 8)?, u32_at(buf, 12)?))
}

/// Status code of a `REMOTE_NDIS_INDICATE_STATUS_MSG`.
pub fn decode_indicate_status(buf: &[u8]) -> Result<u32> {
    u32_at(buf, 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_message_matches_the_wire_layout() {
        let m = encode_init(1, 0x4000);
        assert_eq!(m.len(), 24);
        assert_eq!(
            m,
            vec![
                0x02, 0, 0, 0, // MSG_INIT
                24, 0, 0, 0, // msg_len
                1, 0, 0, 0, // request id
                1, 0, 0, 0, // major
                0, 0, 0, 0, // minor
                0x00, 0x40, 0, 0, // max transfer size
            ]
        );
    }

    #[test]
    fn set_message_points_at_its_own_payload() {
        let m = encode_set(7, OID_GEN_CURRENT_PACKET_FILTER, &0x2Fu32.to_le_bytes());
        assert_eq!(u32_at(&m, 4).unwrap() as usize, m.len());
        let len = u32_at(&m, 16).unwrap() as usize;
        let offset = u32_at(&m, 20).unwrap() as usize;
        assert_eq!(&m[OFFSET_BASE + offset..OFFSET_BASE + offset + len], &[0x2F, 0, 0, 0]);
    }

    fn init_complete_bytes(alignment_exponent: u32) -> Vec<u8> {
        let mut m = Vec::new();
        for v in [
            MSG_INIT_C,
            52,
            1, // request id
            STATUS_SUCCESS,
            1,
            0,      // version
            0,      // device flags
            0,      // medium: 802.3
            8,      // max packets per transfer
            0x4000, // max transfer size
            alignment_exponent,
            0,
            0,
        ] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m
    }

    #[test]
    fn decodes_init_complete_and_expands_the_alignment_exponent() {
        let c = decode_init_complete(&init_complete_bytes(2)).unwrap();
        assert_eq!(c.status, STATUS_SUCCESS);
        assert_eq!(c.max_transfer_size, 0x4000);
        assert_eq!(c.max_packets_per_transfer, 8);
        assert_eq!(c.packet_alignment, 4);
    }

    #[test]
    fn absurd_alignment_exponent_does_not_shift_overflow() {
        let c = decode_init_complete(&init_complete_bytes(0xFFFF_FFFF)).unwrap();
        assert_eq!(c.packet_alignment, 1);
    }

    #[test]
    fn truncated_init_complete_is_rejected() {
        let short = init_complete_bytes(2)[..20].to_vec();
        assert!(matches!(decode_init_complete(&short), Err(Error::Truncated)));
    }

    #[test]
    fn decodes_a_mac_address_from_query_complete() {
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let mut m = Vec::new();
        for v in [MSG_QUERY_C, 30, 3, STATUS_SUCCESS, 6, 16] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        m.extend_from_slice(&mac);

        let (id, status, info) = decode_query_complete(&m).unwrap();
        assert_eq!((id, status), (3, STATUS_SUCCESS));
        assert_eq!(info, mac);
    }

    #[test]
    fn query_complete_information_buffer_cannot_escape_the_message() {
        // Offset/length pair points past the end of the received bytes.
        let mut m = Vec::new();
        for v in [MSG_QUERY_C, 24, 3, STATUS_SUCCESS, 6, 4096] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        assert!(matches!(decode_query_complete(&m), Err(Error::Truncated)));
    }

    #[test]
    fn query_complete_offset_length_overflow_is_rejected() {
        let mut m = Vec::new();
        for v in [MSG_QUERY_C, 24, 3, STATUS_SUCCESS, u32::MAX, u32::MAX] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        assert!(matches!(decode_query_complete(&m), Err(Error::Truncated)));
    }

    #[test]
    fn failed_query_yields_status_without_an_information_buffer() {
        let mut m = Vec::new();
        for v in [MSG_QUERY_C, 24, 3, 0xC000_0001, 0, 0] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        let (_, status, info) = decode_query_complete(&m).unwrap();
        assert_eq!(status, 0xC000_0001);
        assert!(info.is_empty());
    }
}
