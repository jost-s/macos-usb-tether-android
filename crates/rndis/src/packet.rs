//! PACKET_MSG framing: Ethernet frames in and out of bulk transfers.
//!
//! Bounds checks mirror `rndis_rx_fixup` in Linux `drivers/net/usb/rndis_host.c`.
//! A bulk transfer may carry several packets back to back.

use crate::error::{Error, Result};
use crate::wire::{u32_at, DATA_HEADER_LEN, MSG_PACKET, OFFSET_BASE};

/// Wrap an Ethernet frame in a PACKET_MSG.
pub fn encode(frame: &[u8]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(DATA_HEADER_LEN + frame.len());
    let fields: [u32; 11] = [
        MSG_PACKET,
        (DATA_HEADER_LEN + frame.len()) as u32,
        (DATA_HEADER_LEN - OFFSET_BASE) as u32, // data offset, relative to byte 8
        frame.len() as u32,
        0, // OOB data offset
        0, // OOB data length
        0, // number of OOB records
        0, // per-packet info offset
        0, // per-packet info length
        0, // device VC handle
        0, // reserved
    ];
    for f in fields {
        msg.extend_from_slice(&f.to_le_bytes());
    }
    msg.extend_from_slice(frame);
    msg
}

/// Iterator over the Ethernet frames in one bulk transfer.
pub struct Frames<'a> {
    buf: &'a [u8],
}

/// Split a received bulk transfer into Ethernet frames.
pub fn decode(buf: &[u8]) -> Frames<'_> {
    Frames { buf }
}

impl<'a> Iterator for Frames<'a> {
    type Item = Result<&'a [u8]>;

    fn next(&mut self) -> Option<Self::Item> {
        // Trailing padding shorter than a header is normal; stop quietly.
        if self.buf.len() < DATA_HEADER_LEN {
            self.buf = &[];
            return None;
        }

        let result = self.next_frame();
        if result.is_err() {
            // A malformed header makes every following offset meaningless.
            self.buf = &[];
        }
        Some(result)
    }
}

impl<'a> Frames<'a> {
    fn next_frame(&mut self) -> Result<&'a [u8]> {
        let buf = self.buf;
        if u32_at(buf, 0)? != MSG_PACKET {
            return Err(Error::UnexpectedMessage(u32_at(buf, 0)?));
        }

        let msg_len = u32_at(buf, 4)? as usize;
        let data_offset = u32_at(buf, 8)? as usize;
        let data_len = u32_at(buf, 12)? as usize;

        // `msg_len >= DATA_HEADER_LEN` is ours, not the kernel's: it guarantees
        // the iterator advances and cannot spin on a zero-length message.
        if msg_len < DATA_HEADER_LEN || msg_len > buf.len() {
            return Err(Error::Malformed("packet length outside transfer"));
        }
        let end = OFFSET_BASE
            .checked_add(data_offset)
            .and_then(|s| s.checked_add(data_len))
            .ok_or(Error::Malformed("packet data offset overflow"))?;
        if end > msg_len {
            return Err(Error::Malformed("packet data outside message"));
        }

        let frame = &buf[OFFSET_BASE + data_offset..end];
        self.buf = &buf[msg_len..];
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| i as u8).collect()
    }

    fn collect(buf: &[u8]) -> Result<Vec<Vec<u8>>> {
        decode(buf).map(|r| r.map(<[u8]>::to_vec)).collect()
    }

    #[test]
    fn round_trips_a_single_frame() {
        let f = frame(64);
        let frames = collect(&encode(&f)).unwrap();
        assert_eq!(frames, vec![f]);
    }

    #[test]
    fn splits_aggregated_frames_from_one_transfer() {
        let (a, b, c) = (frame(64), frame(100), frame(1514));
        let mut buf = encode(&a);
        buf.extend_from_slice(&encode(&b));
        buf.extend_from_slice(&encode(&c));

        assert_eq!(collect(&buf).unwrap(), vec![a, b, c]);
    }

    #[test]
    fn ignores_trailing_padding_shorter_than_a_header() {
        let f = frame(64);
        let mut buf = encode(&f);
        buf.extend_from_slice(&[0; 16]);
        assert_eq!(collect(&buf).unwrap(), vec![f]);
    }

    #[test]
    fn empty_transfer_yields_nothing() {
        assert!(collect(&[]).unwrap().is_empty());
    }

    #[test]
    fn rejects_data_length_running_past_the_message() {
        let mut buf = encode(&frame(64));
        buf[12..16].copy_from_slice(&9999u32.to_le_bytes());
        assert!(collect(&buf).is_err());
    }

    #[test]
    fn rejects_data_offset_running_past_the_message() {
        let mut buf = encode(&frame(64));
        buf[8..12].copy_from_slice(&9999u32.to_le_bytes());
        assert!(collect(&buf).is_err());
    }

    #[test]
    fn rejects_message_length_beyond_the_transfer() {
        let mut buf = encode(&frame(64));
        let too_long = buf.len() as u32 + 1;
        buf[4..8].copy_from_slice(&too_long.to_le_bytes());
        assert!(collect(&buf).is_err());
    }

    #[test]
    fn rejects_message_length_shorter_than_the_header() {
        let mut buf = encode(&frame(64));
        buf[4..8].copy_from_slice(&8u32.to_le_bytes());
        assert!(collect(&buf).is_err());
    }

    #[test]
    fn rejects_a_non_packet_message() {
        let mut buf = encode(&frame(64));
        buf[0..4].copy_from_slice(&0x8000_0002u32.to_le_bytes());
        assert!(collect(&buf).is_err());
    }

    #[test]
    fn stops_after_a_malformed_packet_instead_of_looping() {
        let mut buf = encode(&frame(64));
        buf[4..8].copy_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&encode(&frame(64)));

        // One error, then the iterator ends.
        let results: Vec<_> = decode(&buf).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[test]
    fn accepts_a_larger_than_minimum_data_offset() {
        // Some gadgets insert per-packet info between header and payload.
        let f = frame(64);
        let pad = 8;
        let mut buf = encode(&f);
        buf.splice(DATA_HEADER_LEN..DATA_HEADER_LEN, std::iter::repeat_n(0u8, pad));
        let new_len = (buf.len()) as u32;
        buf[4..8].copy_from_slice(&new_len.to_le_bytes());
        buf[8..12].copy_from_slice(&((DATA_HEADER_LEN - OFFSET_BASE + pad) as u32).to_le_bytes());

        assert_eq!(collect(&buf).unwrap(), vec![f]);
    }
}
