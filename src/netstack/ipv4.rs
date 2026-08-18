//! Just enough IPv4/UDP to carry DHCP ourselves — the kernel never sees this
//! link, so nothing else can build these packets for us.

use std::net::Ipv4Addr;

use crate::netstack::error::{Error, Result};

pub const PROTO_UDP: u8 = 17;
const IPV4_HEADER_LEN: usize = 20;
const UDP_HEADER_LEN: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct UdpDatagram<'a> {
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

/// One's-complement sum used by both the IPv4 and UDP checksums.
fn checksum(parts: &[&[u8]]) -> u16 {
    let mut sum: u32 = 0;
    let mut carry_byte: Option<u8> = None;

    for part in parts {
        let mut bytes = *part;
        // A part with an odd length pairs its last byte with the next part's
        // first byte, exactly as if the parts were concatenated.
        if let Some(high) = carry_byte.take() {
            let low = bytes.first().copied().unwrap_or(0);
            sum += u32::from(u16::from_be_bytes([high, low]));
            bytes = bytes.get(1..).unwrap_or(&[]);
        }
        let mut chunks = bytes.chunks_exact(2);
        for c in chunks.by_ref() {
            sum += u32::from(u16::from_be_bytes([c[0], c[1]]));
        }
        if let [last] = chunks.remainder() {
            carry_byte = Some(*last);
        }
    }
    if let Some(high) = carry_byte {
        sum += u32::from(u16::from_be_bytes([high, 0]));
    }

    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Build a complete IPv4+UDP packet. `ip_id` should vary between packets.
pub fn build_udp(d: &UdpDatagram, ip_id: u16) -> Vec<u8> {
    let udp_len = UDP_HEADER_LEN + d.payload.len();
    let total_len = IPV4_HEADER_LEN + udp_len;

    let mut p = Vec::with_capacity(total_len);
    p.push(0x45); // version 4, 5-word header
    p.push(0); // DSCP/ECN
    p.extend_from_slice(&(total_len as u16).to_be_bytes());
    p.extend_from_slice(&ip_id.to_be_bytes());
    p.extend_from_slice(&0u16.to_be_bytes()); // flags/fragment offset
    p.push(64); // TTL
    p.push(PROTO_UDP);
    p.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    p.extend_from_slice(&d.src_ip.octets());
    p.extend_from_slice(&d.dst_ip.octets());

    let ip_checksum = checksum(&[&p[..IPV4_HEADER_LEN]]);
    p[10..12].copy_from_slice(&ip_checksum.to_be_bytes());

    p.extend_from_slice(&d.src_port.to_be_bytes());
    p.extend_from_slice(&d.dst_port.to_be_bytes());
    p.extend_from_slice(&(udp_len as u16).to_be_bytes());
    p.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    p.extend_from_slice(d.payload);

    // UDP checksum covers a pseudo-header of the IP addresses, protocol and length.
    let pseudo = [
        d.src_ip.octets()[0],
        d.src_ip.octets()[1],
        d.src_ip.octets()[2],
        d.src_ip.octets()[3],
        d.dst_ip.octets()[0],
        d.dst_ip.octets()[1],
        d.dst_ip.octets()[2],
        d.dst_ip.octets()[3],
        0,
        PROTO_UDP,
        (udp_len >> 8) as u8,
        udp_len as u8,
    ];
    let udp_checksum = checksum(&[&pseudo, &p[IPV4_HEADER_LEN..]]);
    // All-zeroes means "no checksum"; RFC 768 sends the equivalent 0xFFFF instead.
    let udp_checksum = if udp_checksum == 0 {
        0xFFFF
    } else {
        udp_checksum
    };
    p[IPV4_HEADER_LEN + 6..IPV4_HEADER_LEN + 8].copy_from_slice(&udp_checksum.to_be_bytes());

    p
}

/// Extract a UDP datagram, tolerating IPv4 options. Fragments are rejected:
/// DHCP never uses them and reassembly is out of scope.
pub fn parse_udp(buf: &[u8]) -> Result<UdpDatagram<'_>> {
    if buf.len() < IPV4_HEADER_LEN {
        return Err(Error::Truncated);
    }
    if buf[0] >> 4 != 4 {
        return Err(Error::Malformed("not IPv4"));
    }
    let header_len = (buf[0] & 0x0F) as usize * 4;
    if header_len < IPV4_HEADER_LEN || header_len > buf.len() {
        return Err(Error::Malformed("bad IPv4 header length"));
    }
    if buf[9] != PROTO_UDP {
        return Err(Error::Malformed("not UDP"));
    }
    // More-fragments flag or a non-zero fragment offset.
    if buf[6] & 0x20 != 0 || u16::from_be_bytes([buf[6] & 0x1F, buf[7]]) != 0 {
        return Err(Error::Malformed("fragmented"));
    }

    let total_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    // Trailing bytes are normal (Ethernet padding); a short buffer is not.
    if total_len < header_len || total_len > buf.len() {
        return Err(Error::Truncated);
    }

    let udp = &buf[header_len..total_len];
    if udp.len() < UDP_HEADER_LEN {
        return Err(Error::Truncated);
    }
    let udp_len = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if udp_len < UDP_HEADER_LEN || udp_len > udp.len() {
        return Err(Error::Truncated);
    }

    Ok(UdpDatagram {
        src_ip: Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]),
        dst_ip: Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]),
        src_port: u16::from_be_bytes([udp[0], udp[1]]),
        dst_port: u16::from_be_bytes([udp[2], udp[3]]),
        payload: &udp[UDP_HEADER_LEN..udp_len],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(payload: &[u8]) -> UdpDatagram<'_> {
        UdpDatagram {
            src_ip: Ipv4Addr::UNSPECIFIED,
            dst_ip: Ipv4Addr::BROADCAST,
            src_port: 68,
            dst_port: 67,
            payload,
        }
    }

    #[test]
    fn round_trips_a_datagram() {
        let payload = vec![7u8; 300];
        let packet = build_udp(&datagram(&payload), 0x1234);
        let d = parse_udp(&packet).unwrap();

        assert_eq!(d.src_ip, Ipv4Addr::UNSPECIFIED);
        assert_eq!(d.dst_ip, Ipv4Addr::BROADCAST);
        assert_eq!((d.src_port, d.dst_port), (68, 67));
        assert_eq!(d.payload, &payload[..]);
    }

    #[test]
    fn built_packets_have_valid_checksums() {
        // Summing a correct header including its checksum yields zero.
        let packet = build_udp(&datagram(&[1, 2, 3]), 1);
        assert_eq!(checksum(&[&packet[..20]]), 0);
    }

    #[test]
    fn odd_length_payload_checksums_the_same_as_a_flat_buffer() {
        let packet = build_udp(&datagram(&[1, 2, 3, 4, 5]), 1);
        let udp = &packet[20..];
        let pseudo = [
            0,
            0,
            0,
            0,
            255,
            255,
            255,
            255,
            0,
            PROTO_UDP,
            0,
            udp.len() as u8,
        ];
        let split = checksum(&[&pseudo, udp]);

        let mut flat = pseudo.to_vec();
        flat.extend_from_slice(udp);
        assert_eq!(split, checksum(&[&flat]));
    }

    #[test]
    fn parses_a_packet_carrying_ipv4_options() {
        let mut packet = build_udp(&datagram(&[9, 9]), 1);
        // Splice a 4-byte no-op option in and fix up the header length/total.
        packet.splice(20..20, [1, 1, 1, 1]);
        packet[0] = 0x46;
        let total = packet.len() as u16;
        packet[2..4].copy_from_slice(&total.to_be_bytes());

        assert_eq!(parse_udp(&packet).unwrap().payload, &[9, 9]);
    }

    #[test]
    fn ignores_trailing_ethernet_padding() {
        let mut packet = build_udp(&datagram(&[1, 2, 3]), 1);
        packet.extend_from_slice(&[0; 20]);
        assert_eq!(parse_udp(&packet).unwrap().payload, &[1, 2, 3]);
    }

    #[test]
    fn rejects_fragments() {
        let mut packet = build_udp(&datagram(&[1]), 1);
        packet[6] = 0x20; // more fragments
        assert!(matches!(parse_udp(&packet), Err(Error::Malformed(_))));

        let mut packet = build_udp(&datagram(&[1]), 1);
        packet[7] = 1; // non-zero fragment offset
        assert!(matches!(parse_udp(&packet), Err(Error::Malformed(_))));
    }

    #[test]
    fn rejects_a_length_field_longer_than_the_buffer() {
        let mut packet = build_udp(&datagram(&[1, 2, 3]), 1);
        let lie = (packet.len() as u16) + 100;
        packet[2..4].copy_from_slice(&lie.to_be_bytes());
        assert!(matches!(parse_udp(&packet), Err(Error::Truncated)));
    }

    #[test]
    fn rejects_a_udp_length_longer_than_the_ip_payload() {
        let mut packet = build_udp(&datagram(&[1, 2, 3]), 1);
        packet[24..26].copy_from_slice(&999u16.to_be_bytes());
        assert!(parse_udp(&packet).is_err());
    }

    #[test]
    fn rejects_a_header_length_below_the_minimum() {
        let mut packet = build_udp(&datagram(&[1]), 1);
        packet[0] = 0x44;
        assert!(matches!(parse_udp(&packet), Err(Error::Malformed(_))));
    }

    #[test]
    fn rejects_non_udp_and_non_ipv4() {
        let mut packet = build_udp(&datagram(&[1]), 1);
        packet[9] = 6;
        assert!(matches!(parse_udp(&packet), Err(Error::Malformed(_))));

        let mut packet = build_udp(&datagram(&[1]), 1);
        packet[0] = 0x65;
        assert!(matches!(parse_udp(&packet), Err(Error::Malformed(_))));
    }
}
