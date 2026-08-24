use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};

use etherparse::{
    Icmpv6Header, Icmpv6Type, IpNumber, Ipv6ExtensionSlice, Ipv6ExtensionsSlice, Ipv6Header,
    UdpHeader,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotedEchoPacket<'a> {
    packet: &'a [u8],
    upper_offset: usize,
    source: Ipv6Addr,
    pub destination: Ipv6Addr,
    pub hop_limit: u8,
    pub id: u16,
    pub seq: u16,
    pub payload: &'a [u8],
}

impl<'a> QuotedEchoPacket<'a> {
    pub fn parse(packet: &'a [u8]) -> Option<Self> {
        let parsed = ParsedIpv6Quote::parse(packet, IpNumber::IPV6_ICMP)?;
        let (icmp, payload) = Icmpv6Header::from_slice(parsed.upper_payload).ok()?;
        let Icmpv6Type::EchoRequest(echo) = icmp.icmp_type else {
            return None;
        };
        Some(Self {
            packet,
            upper_offset: parsed.upper_offset,
            source: parsed.source,
            destination: parsed.destination,
            hop_limit: parsed.hop_limit,
            id: echo.id,
            seq: echo.seq,
            payload,
        })
    }

    pub fn translate(&self, source: Ipv6Addr, destination: Ipv6Addr, id: u16, seq: u16) -> Vec<u8> {
        let mut packet = self.packet.to_vec();
        let checksum_offset = self.upper_offset + 2;
        let mut sum = u32::from(!u16::from_be_bytes([
            packet[checksum_offset],
            packet[checksum_offset + 1],
        ]));
        update_checksum_words(&mut sum, self.source.octets(), source.octets());
        update_checksum_words(&mut sum, self.destination.octets(), destination.octets());
        update_checksum_word(&mut sum, self.id, id);
        update_checksum_word(&mut sum, self.seq, seq);
        packet[8..24].copy_from_slice(&source.octets());
        packet[24..40].copy_from_slice(&destination.octets());
        packet[self.upper_offset + 4..self.upper_offset + 6].copy_from_slice(&id.to_be_bytes());
        packet[self.upper_offset + 6..self.upper_offset + 8].copy_from_slice(&seq.to_be_bytes());
        packet[checksum_offset..checksum_offset + 2]
            .copy_from_slice(&finish_checksum(sum).to_be_bytes());
        packet
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuotedUdpPacket<'a> {
    packet: &'a [u8],
    upper_offset: usize,
    pub source: SocketAddrV6,
    pub destination: SocketAddrV6,
    checksum: u16,
}

impl<'a> QuotedUdpPacket<'a> {
    pub fn parse(packet: &'a [u8]) -> Option<Self> {
        let parsed = ParsedIpv6Quote::parse(packet, IpNumber::UDP)?;
        let (udp, _) = UdpHeader::from_slice(parsed.upper_payload).ok()?;
        if udp.length < UdpHeader::LEN_U16 {
            return None;
        }
        let upper_payload_len = parsed
            .declared_payload_len
            .checked_sub(parsed.extension_len)?;
        if if parsed.first_fragment {
            usize::from(udp.length) < upper_payload_len
        } else {
            usize::from(udp.length) != upper_payload_len
        } {
            return None;
        }
        Some(Self {
            packet,
            upper_offset: parsed.upper_offset,
            source: SocketAddrV6::new(parsed.source, udp.source_port, 0, 0),
            destination: SocketAddrV6::new(parsed.destination, udp.destination_port, 0, 0),
            checksum: udp.checksum,
        })
    }

    pub fn translate(
        &self,
        source: SocketAddrV6,
        destination: SocketAddrV6,
    ) -> io::Result<Vec<u8>> {
        if self.checksum == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "missing udp checksum",
            ));
        }
        let mut sum = u32::from(!self.checksum);
        update_checksum_words(&mut sum, self.source.ip().octets(), source.ip().octets());
        update_checksum_words(
            &mut sum,
            self.destination.ip().octets(),
            destination.ip().octets(),
        );
        update_checksum_word(&mut sum, self.source.port(), source.port());
        update_checksum_word(&mut sum, self.destination.port(), destination.port());
        let checksum = finish_checksum(sum);
        let mut packet = self.packet.to_vec();
        packet[8..24].copy_from_slice(&source.ip().octets());
        packet[24..40].copy_from_slice(&destination.ip().octets());
        packet[self.upper_offset..self.upper_offset + 2]
            .copy_from_slice(&source.port().to_be_bytes());
        packet[self.upper_offset + 2..self.upper_offset + 4]
            .copy_from_slice(&destination.port().to_be_bytes());
        packet[self.upper_offset + 6..self.upper_offset + 8]
            .copy_from_slice(&(if checksum == 0 { 0xffff } else { checksum }).to_be_bytes());
        Ok(packet)
    }
}

struct ParsedIpv6Quote<'a> {
    upper_payload: &'a [u8],
    upper_offset: usize,
    extension_len: usize,
    declared_payload_len: usize,
    first_fragment: bool,
    source: Ipv6Addr,
    destination: Ipv6Addr,
    hop_limit: u8,
}

impl<'a> ParsedIpv6Quote<'a> {
    fn parse(packet: &'a [u8], expected: IpNumber) -> Option<Self> {
        let (ip, payload) = Ipv6Header::from_slice(packet).ok()?;
        let declared_payload_len = usize::from(ip.payload_length);
        if declared_payload_len == 0 || payload.len() > declared_payload_len {
            return None;
        }
        let (extensions, next_header, upper_payload) =
            Ipv6ExtensionsSlice::from_slice(ip.next_header, payload).ok()?;
        if next_header != expected {
            return None;
        }
        let mut first_fragment = false;
        let mut saw_fragment = false;
        for extension in extensions.clone() {
            if let Ipv6ExtensionSlice::Fragment(fragment) = extension {
                if saw_fragment || fragment.fragment_offset().value() != 0 {
                    return None;
                }
                saw_fragment = true;
                first_fragment = fragment.more_fragments();
            }
        }
        let extension_len = extensions.slice().len();
        if declared_payload_len < extension_len + 8 || upper_payload.len() < 8 {
            return None;
        }
        Some(Self {
            upper_payload,
            upper_offset: Ipv6Header::LEN + extension_len,
            extension_len,
            declared_payload_len,
            first_fragment,
            source: ip.source_addr(),
            destination: ip.destination_addr(),
            hop_limit: ip.hop_limit,
        })
    }
}

fn update_checksum_words(sum: &mut u32, old: [u8; 16], new: [u8; 16]) {
    for (old, new) in old.chunks_exact(2).zip(new.chunks_exact(2)) {
        update_checksum_word(
            sum,
            u16::from_be_bytes([old[0], old[1]]),
            u16::from_be_bytes([new[0], new[1]]),
        );
    }
}

fn update_checksum_word(sum: &mut u32, old: u16, new: u16) {
    *sum += u32::from(!old) + u32::from(new);
    *sum = (*sum & 0xffff) + (*sum >> 16);
}

fn finish_checksum(mut sum: u32) -> u16 {
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use etherparse::{IcmpEchoHeader, Ipv6FragmentHeader, Ipv6RawExtHeader};

    fn ipv6_packet(
        source: Ipv6Addr,
        destination: Ipv6Addr,
        next_header: IpNumber,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut ip = Ipv6Header {
            next_header,
            hop_limit: 17,
            source: source.octets(),
            destination: destination.octets(),
            ..Default::default()
        };
        ip.set_payload_length(payload.len()).unwrap();
        let mut packet = Vec::with_capacity(Ipv6Header::LEN + payload.len());
        packet.extend_from_slice(&ip.to_bytes());
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn echo_quote_walks_extensions_and_preserves_them_when_translated() {
        let upstream: Ipv6Addr = "2001:db8:1::10".parse().unwrap();
        let client: Ipv6Addr = "fd00::2".parse().unwrap();
        let destination: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let icmp = Icmpv6Type::EchoRequest(IcmpEchoHeader { id: 11, seq: 22 })
            .to_header(upstream.octets(), destination.octets(), b"payload")
            .unwrap();
        let extension =
            Ipv6RawExtHeader::new_raw(IpNumber::IPV6_FRAGMENTATION_HEADER, &[0; 6]).unwrap();
        let fragment = Ipv6FragmentHeader::new(
            IpNumber::IPV6_ICMP,
            0.try_into().unwrap(),
            true,
            0x1234_5678,
        );
        let mut payload = Vec::new();
        payload.extend_from_slice(&extension.to_bytes());
        payload.extend_from_slice(&fragment.to_bytes());
        payload.extend_from_slice(&icmp.to_bytes());
        payload.extend_from_slice(b"payload");
        let packet = ipv6_packet(
            upstream,
            destination,
            IpNumber::IPV6_DESTINATION_OPTIONS,
            &payload,
        );

        let quote = QuotedEchoPacket::parse(&packet).unwrap();
        assert_eq!(quote.destination, destination);
        assert_eq!(quote.hop_limit, 17);
        assert_eq!(quote.id, 11);
        assert_eq!(quote.seq, 22);
        assert_eq!(quote.payload, b"payload");

        let translated = quote.translate(client, destination, 33, 44);
        assert_eq!(&translated[40..56], &packet[40..56]);
        let reparsed = QuotedEchoPacket::parse(&translated).unwrap();
        assert_eq!(reparsed.id, 33);
        assert_eq!(reparsed.seq, 44);
        let (_, rest) = Ipv6Header::from_slice(&translated).unwrap();
        let (_, _, upper) =
            Ipv6ExtensionsSlice::from_slice(IpNumber::IPV6_DESTINATION_OPTIONS, rest).unwrap();
        let (translated_icmp, translated_payload) = Icmpv6Header::from_slice(upper).unwrap();
        assert_eq!(translated_payload, b"payload");
        assert_eq!(
            translated_icmp.checksum,
            Icmpv6Type::EchoRequest(IcmpEchoHeader { id: 33, seq: 44 })
                .to_header(client.octets(), destination.octets(), b"payload")
                .unwrap()
                .checksum
        );
    }

    #[test]
    fn udp_quote_accepts_first_fragment_and_preserves_fragment_header() {
        let upstream: SocketAddrV6 = "[2001:db8:1::10]:50000".parse().unwrap();
        let client: SocketAddrV6 = "[fd00::2]:1234".parse().unwrap();
        let destination: SocketAddrV6 = "[2001:db8::1]:443".parse().unwrap();
        let full_payload = [7u8; 24];
        let mut checksum_ip = Ipv6Header {
            source: upstream.ip().octets(),
            destination: destination.ip().octets(),
            ..Default::default()
        };
        checksum_ip
            .set_payload_length(UdpHeader::LEN + full_payload.len())
            .unwrap();
        let udp = UdpHeader::with_ipv6_checksum(
            upstream.port(),
            destination.port(),
            &checksum_ip,
            &full_payload,
        )
        .unwrap();
        let fragment =
            Ipv6FragmentHeader::new(IpNumber::UDP, 0.try_into().unwrap(), true, 0x1234_5678);
        let mut payload = Vec::new();
        payload.extend_from_slice(&fragment.to_bytes());
        payload.extend_from_slice(&udp.to_bytes());
        payload.extend_from_slice(&full_payload[..8]);
        let packet = ipv6_packet(
            *upstream.ip(),
            *destination.ip(),
            IpNumber::IPV6_FRAGMENTATION_HEADER,
            &payload,
        );

        let quote = QuotedUdpPacket::parse(&packet).unwrap();
        assert_eq!(quote.source, upstream);
        assert_eq!(quote.destination, destination);
        let translated = quote.translate(client, destination).unwrap();
        assert_eq!(&translated[40..48], &packet[40..48]);
        assert_eq!(&translated[56..], &packet[56..]);
        let reparsed = QuotedUdpPacket::parse(&translated).unwrap();
        assert_eq!(reparsed.source, client);
        assert_eq!(reparsed.destination, destination);

        checksum_ip.source = client.ip().octets();
        let expected = UdpHeader::with_ipv6_checksum(
            client.port(),
            destination.port(),
            &checksum_ip,
            &full_payload,
        )
        .unwrap();
        assert_eq!(
            u16::from_be_bytes([translated[54], translated[55]]),
            expected.checksum
        );
    }

    #[test]
    fn transport_quote_rejects_non_initial_fragment() {
        let source: Ipv6Addr = "2001:db8:1::10".parse().unwrap();
        let destination: Ipv6Addr = "2001:db8::1".parse().unwrap();
        let fragment = Ipv6FragmentHeader::new(
            IpNumber::IPV6_ICMP,
            1.try_into().unwrap(),
            false,
            0x1234_5678,
        );
        let mut payload = Vec::new();
        payload.extend_from_slice(&fragment.to_bytes());
        payload.extend_from_slice(&[128, 0, 0, 0, 0, 1, 0, 2]);
        let packet = ipv6_packet(
            source,
            destination,
            IpNumber::IPV6_FRAGMENTATION_HEADER,
            &payload,
        );
        assert!(QuotedEchoPacket::parse(&packet).is_none());
    }

    #[test]
    fn transport_quote_rejects_truncated_extension_header() {
        let packet = ipv6_packet(
            "2001:db8:1::10".parse().unwrap(),
            "2001:db8::1".parse().unwrap(),
            IpNumber::IPV6_DESTINATION_OPTIONS,
            &[IpNumber::UDP.0, 1, 0, 0, 0, 0, 0, 0],
        );
        assert!(QuotedUdpPacket::parse(&packet).is_none());
    }
}
