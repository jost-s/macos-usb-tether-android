//! Ethernet framing and the synthesized host MAC.

use std::fmt;

use crate::error::{Error, Result};

pub const HEADER_LEN: usize = 14;
/// Ethernet's minimum frame size excluding FCS.
pub const MIN_FRAME_LEN: usize = 60;

pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86DD;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const BROADCAST: MacAddr = MacAddr([0xFF; 6]);
    pub const ZERO: MacAddr = MacAddr([0; 6]);

    pub fn from_slice(b: &[u8]) -> Result<Self> {
        let mut m = [0u8; 6];
        m.copy_from_slice(b.get(..6).ok_or(Error::Truncated)?);
        Ok(MacAddr(m))
    }

    pub fn is_broadcast(&self) -> bool {
        self.0 == [0xFF; 6]
    }

    /// True for broadcast too, per the 802.3 group bit.
    pub fn is_multicast(&self) -> bool {
        self.0[0] & 0x01 != 0
    }

    pub fn as_bytes(&self) -> &[u8; 6] {
        &self.0
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5]
        )
    }
}

impl fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Frame<'a> {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn parse(buf: &[u8]) -> Result<Frame<'_>> {
    if buf.len() < HEADER_LEN {
        return Err(Error::Truncated);
    }
    Ok(Frame {
        dst: MacAddr::from_slice(&buf[0..6])?,
        src: MacAddr::from_slice(&buf[6..12])?,
        ethertype: u16::from_be_bytes([buf[12], buf[13]]),
        payload: &buf[HEADER_LEN..],
    })
}

/// Build a frame, zero-padded to the 60-byte minimum.
pub fn build(dst: MacAddr, src: MacAddr, ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity((HEADER_LEN + payload.len()).max(MIN_FRAME_LEN));
    f.extend_from_slice(&dst.0);
    f.extend_from_slice(&src.0);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f.resize(f.len().max(MIN_FRAME_LEN), 0);
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_frame() {
        let dst = MacAddr([1, 2, 3, 4, 5, 6]);
        let src = MacAddr([0xAA; 6]);
        let payload = vec![9u8; 100];

        let built = build(dst, src, ETHERTYPE_IPV4, &payload);
        let f = parse(&built).unwrap();
        assert_eq!(f.dst, dst);
        assert_eq!(f.src, src);
        assert_eq!(f.ethertype, ETHERTYPE_IPV4);
        assert_eq!(&f.payload[..100], &payload[..]);
    }

    #[test]
    fn pads_short_frames_to_the_ethernet_minimum() {
        let built = build(MacAddr::BROADCAST, MacAddr::ZERO, ETHERTYPE_ARP, &[1, 2, 3]);
        assert_eq!(built.len(), MIN_FRAME_LEN);
    }

    #[test]
    fn rejects_a_frame_shorter_than_its_header() {
        assert!(matches!(parse(&[0u8; 13]), Err(Error::Truncated)));
    }

    #[test]
    fn broadcast_counts_as_multicast() {
        assert!(MacAddr::BROADCAST.is_multicast());
        assert!(MacAddr::BROADCAST.is_broadcast());
        assert!(!MacAddr([0x02, 0, 0, 0, 0, 1]).is_multicast());
    }
}
