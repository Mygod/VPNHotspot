use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};

use etherparse::icmpv6::TYPE_ECHO_REQUEST as ICMPV6_ECHO_REQUEST;
use etherparse::{IcmpEchoHeader, Icmpv6Type, IpNumber, Ipv6Header, UdpHeader};

const ICMPV6_HEADER_LEN: usize = 8;
const ICMPV6_MINIMUM_MTU: usize = 1280;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UdpQuoteMetadata {
    pub source: SocketAddrV6,
    pub destination: SocketAddrV6,
    pub hop_limit: u8,
    pub length: u16,
    pub checksum: u16,
}

pub fn build_echo_request_zero_checksum(id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(ICMPV6_HEADER_LEN + payload.len());
    packet.extend([ICMPV6_ECHO_REQUEST, 0, 0, 0]);
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

pub fn build_echo_request_with_checksum(
    source: Ipv6Addr,
    destination: Ipv6Addr,
    id: u16,
    seq: u16,
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    build_echo_packet_with_checksum(
        Icmpv6Type::EchoRequest(IcmpEchoHeader { id, seq }),
        source,
        destination,
        payload,
    )
}

pub fn build_echo_reply_with_checksum(
    source: Ipv6Addr,
    destination: Ipv6Addr,
    id: u16,
    seq: u16,
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    build_echo_packet_with_checksum(
        Icmpv6Type::EchoReply(IcmpEchoHeader { id, seq }),
        source,
        destination,
        payload,
    )
}

fn build_echo_packet_with_checksum(
    icmp_type: Icmpv6Type,
    source: Ipv6Addr,
    destination: Ipv6Addr,
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    let header = icmp_type
        .to_header(source.octets(), destination.octets(), payload)
        .map_err(io::Error::other)?;
    let mut packet = Vec::with_capacity(header.header_len() + payload.len());
    packet.extend_from_slice(&header.to_bytes());
    packet.extend_from_slice(payload);
    Ok(packet)
}

pub fn build_icmp_error_packet(
    type_u8: u8,
    code_u8: u8,
    bytes5to8: [u8; 4],
    source: Ipv6Addr,
    destination: Ipv6Addr,
    quote: &[u8],
) -> io::Result<Vec<u8>> {
    let quote = cap_error_quote(quote);
    let header = Icmpv6Type::Unknown {
        type_u8,
        code_u8,
        bytes5to8,
    }
    .to_header(source.octets(), destination.octets(), quote)
    .map_err(io::Error::other)?;
    let mut packet = Vec::with_capacity(header.header_len() + quote.len());
    packet.extend_from_slice(&header.to_bytes());
    packet.extend_from_slice(quote);
    Ok(packet)
}

fn cap_error_quote(quote: &[u8]) -> &[u8] {
    let max_quote = ICMPV6_MINIMUM_MTU - Ipv6Header::LEN - ICMPV6_HEADER_LEN;
    &quote[..quote.len().min(max_quote)]
}

pub fn build_icmp_quote(
    source: Ipv6Addr,
    destination: Ipv6Addr,
    hop_limit: u8,
    icmp_payload: &[u8],
) -> io::Result<Vec<u8>> {
    let icmp_payload = cap_invoking_icmp_payload(icmp_payload);
    let ip = ipv6_header(
        source,
        destination,
        IpNumber::IPV6_ICMP,
        hop_limit,
        icmp_payload.len(),
    )?;
    let mut quote = Vec::with_capacity(Ipv6Header::LEN + icmp_payload.len());
    quote.extend_from_slice(&ip.to_bytes());
    quote.extend_from_slice(icmp_payload);
    Ok(quote)
}

pub fn build_udp_quote_with_hop_limit(
    client: SocketAddrV6,
    destination: SocketAddrV6,
    hop_limit: u8,
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    let max_payload =
        ICMPV6_MINIMUM_MTU - Ipv6Header::LEN - ICMPV6_HEADER_LEN - Ipv6Header::LEN - UdpHeader::LEN;
    let quoted_payload = &payload[..payload.len().min(max_payload)];
    let ip = ipv6_header(
        *client.ip(),
        *destination.ip(),
        IpNumber::UDP,
        hop_limit,
        UdpHeader::LEN + payload.len(),
    )?;
    let udp = UdpHeader::with_ipv6_checksum(client.port(), destination.port(), &ip, payload)
        .map_err(io::Error::other)?;
    let mut quote = Vec::with_capacity(Ipv6Header::LEN + UdpHeader::LEN + quoted_payload.len());
    quote.extend_from_slice(&ip.to_bytes());
    quote.extend_from_slice(&udp.to_bytes());
    quote.extend_from_slice(quoted_payload);
    Ok(quote)
}

pub fn build_translated_udp_quote(
    original: UdpQuoteMetadata,
    client: SocketAddrV6,
    destination: SocketAddrV6,
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    let length = usize::from(original.length);
    if length < UdpHeader::LEN || length < UdpHeader::LEN + payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid udp quote length",
        ));
    }
    let max_payload =
        ICMPV6_MINIMUM_MTU - Ipv6Header::LEN - ICMPV6_HEADER_LEN - Ipv6Header::LEN - UdpHeader::LEN;
    let quoted_payload = &payload[..payload.len().min(max_payload)];
    let ip = ipv6_header(
        *client.ip(),
        *destination.ip(),
        IpNumber::UDP,
        original.hop_limit,
        length,
    )?;
    let udp = UdpHeader {
        source_port: client.port(),
        destination_port: destination.port(),
        length: original.length,
        checksum: translated_udp_checksum(original, client, destination)?,
    };
    let mut quote = Vec::with_capacity(Ipv6Header::LEN + UdpHeader::LEN + quoted_payload.len());
    quote.extend_from_slice(&ip.to_bytes());
    quote.extend_from_slice(&udp.to_bytes());
    quote.extend_from_slice(quoted_payload);
    Ok(quote)
}

fn cap_invoking_icmp_payload(payload: &[u8]) -> &[u8] {
    let max_payload = ICMPV6_MINIMUM_MTU - Ipv6Header::LEN - ICMPV6_HEADER_LEN - Ipv6Header::LEN;
    &payload[..payload.len().min(max_payload)]
}

fn translated_udp_checksum(
    original: UdpQuoteMetadata,
    client: SocketAddrV6,
    destination: SocketAddrV6,
) -> io::Result<u16> {
    if original.checksum == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "missing udp checksum",
        ));
    }
    let mut sum = u32::from(!original.checksum);
    update_checksum_words(
        &mut sum,
        original.source.ip().octets(),
        client.ip().octets(),
    );
    update_checksum_words(
        &mut sum,
        original.destination.ip().octets(),
        destination.ip().octets(),
    );
    update_checksum_word(&mut sum, original.source.port(), client.port());
    update_checksum_word(&mut sum, original.destination.port(), destination.port());
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    let checksum = !(sum as u16);
    Ok(if checksum == 0 { 0xffff } else { checksum })
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

fn ipv6_header(
    source: Ipv6Addr,
    destination: Ipv6Addr,
    next_header: IpNumber,
    hop_limit: u8,
    payload_len: usize,
) -> io::Result<Ipv6Header> {
    let mut header = Ipv6Header {
        next_header,
        hop_limit,
        source: source.octets(),
        destination: destination.octets(),
        ..Default::default()
    };
    header
        .set_payload_length(payload_len)
        .map_err(io::Error::other)?;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use etherparse::Icmpv6Header;

    #[test]
    fn echo_request_packet_can_be_parsed_from_selected_library_path() {
        let source = "fd00::2".parse().unwrap();
        let destination = "2001:db8::1".parse().unwrap();
        let packet =
            build_echo_request_with_checksum(source, destination, 11, 22, b"probe").unwrap();
        let (header, payload) = Icmpv6Header::from_slice(&packet).unwrap();
        assert!(header.checksum != 0);
        assert_eq!(payload, b"probe");
        assert_eq!(
            header.icmp_type,
            Icmpv6Type::EchoRequest(IcmpEchoHeader { id: 11, seq: 22 })
        );
    }

    #[test]
    fn echo_packet_restores_identifier_with_checksum() {
        let source = "2001:db8::1".parse().unwrap();
        let destination = "fd00::2".parse().unwrap();
        let packet = build_echo_reply_with_checksum(source, destination, 7, 9, b"abc").unwrap();
        let (header, payload) = Icmpv6Header::from_slice(&packet).unwrap();
        assert!(header.checksum != 0);
        assert_eq!(payload, b"abc");
        assert_eq!(
            header.icmp_type,
            Icmpv6Type::EchoReply(IcmpEchoHeader { id: 7, seq: 9 })
        );
    }

    #[test]
    fn icmp_error_packet_preserves_type_code_and_metadata() {
        let source = "2001:db8::1".parse().unwrap();
        let destination = "fd00::2".parse().unwrap();
        for (type_u8, code_u8, bytes5to8) in [
            (1, 4, [0; 4]),
            (2, 0, 1280u32.to_be_bytes()),
            (3, 0, [0; 4]),
            (4, 1, 7u32.to_be_bytes()),
        ] {
            let packet =
                build_icmp_error_packet(type_u8, code_u8, bytes5to8, source, destination, b"quote")
                    .unwrap();
            let (header, payload) = Icmpv6Header::from_slice(&packet).unwrap();
            assert!(header.checksum != 0);
            assert_eq!(payload, b"quote");
            assert_eq!(header.icmp_type.type_u8(), type_u8);
            assert_eq!(header.icmp_type.code_u8(), code_u8);
            assert_eq!(&packet[4..8], &bytes5to8);
        }
    }

    #[test]
    fn icmp_quote_uses_original_echo_addresses_and_hop_limit() {
        let source = "fd00::2".parse().unwrap();
        let destination = "2001:db8::1".parse().unwrap();
        let request = build_echo_request_zero_checksum(11, 22, b"probe");
        let quote = build_icmp_quote(source, destination, 1, &request).unwrap();
        let (ip, rest) = Ipv6Header::from_slice(&quote).unwrap();
        assert_eq!(ip.source_addr(), source);
        assert_eq!(ip.destination_addr(), destination);
        assert_eq!(ip.next_header, IpNumber::IPV6_ICMP);
        assert_eq!(ip.hop_limit, 1);
        let (header, payload) = Icmpv6Header::from_slice(rest).unwrap();
        assert_eq!(payload, b"probe");
        assert_eq!(
            header.icmp_type,
            Icmpv6Type::EchoRequest(IcmpEchoHeader { id: 11, seq: 22 })
        );
    }

    #[test]
    fn udp_quote_uses_client_view_addresses_and_ports() {
        let client = "[fd00::2]:1234".parse().unwrap();
        let destination = "[2001:db8::1]:443".parse().unwrap();
        let quote = build_udp_quote_with_hop_limit(client, destination, 64, b"payload").unwrap();
        let (ip, rest) = Ipv6Header::from_slice(&quote).unwrap();
        assert_eq!(ip.source_addr(), *client.ip());
        assert_eq!(ip.destination_addr(), *destination.ip());
        assert_eq!(ip.next_header, IpNumber::UDP);
        assert_eq!(ip.hop_limit, 64);
        let (udp, payload) = UdpHeader::from_slice(rest).unwrap();
        assert_eq!(udp.source_port, client.port());
        assert_eq!(udp.destination_port, destination.port());
        assert_eq!(
            usize::from(ip.payload_length),
            UdpHeader::LEN + payload.len()
        );
        assert_eq!(usize::from(udp.length), UdpHeader::LEN + payload.len());
        assert_eq!(payload, b"payload");
        assert_ne!(udp.checksum, 0);
    }

    #[test]
    fn udp_quote_caps_payload_to_ipv6_minimum_mtu() {
        let client = "[fd00::2]:1234".parse().unwrap();
        let destination = "[2001:db8::1]:443".parse().unwrap();
        let payload = vec![0u8; 4096];
        let quote = build_udp_quote_with_hop_limit(client, destination, 1, &payload).unwrap();
        assert_eq!(
            quote.len(),
            ICMPV6_MINIMUM_MTU - Ipv6Header::LEN - ICMPV6_HEADER_LEN
        );
        let (ip, rest) = Ipv6Header::from_slice(&quote).unwrap();
        assert_eq!(ip.hop_limit, 1);
        assert_eq!(
            usize::from(ip.payload_length),
            UdpHeader::LEN + payload.len()
        );
        let (udp, quoted_payload) = UdpHeader::from_slice(rest).unwrap();
        assert_eq!(usize::from(udp.length), UdpHeader::LEN + payload.len());
        assert_eq!(
            quoted_payload.len(),
            quote.len() - Ipv6Header::LEN - UdpHeader::LEN
        );
        let expected =
            UdpHeader::with_ipv6_checksum(client.port(), destination.port(), &ip, &payload)
                .unwrap();
        assert_eq!(udp.checksum, expected.checksum);
        assert_ne!(udp.checksum, 0);
    }

    #[test]
    fn translated_udp_quote_preserves_parsed_length_and_adjusts_checksum() {
        let upstream = "[2001:db8:1::10]:50000".parse().unwrap();
        let client = "[fd00::2]:1234".parse().unwrap();
        let destination = "[2001:db8::1]:443".parse().unwrap();
        let full_payload = vec![7u8; 4096];
        let upstream_quote =
            build_udp_quote_with_hop_limit(upstream, destination, 2, &full_payload).unwrap();
        let (upstream_ip, upstream_rest) = Ipv6Header::from_slice(&upstream_quote).unwrap();
        let (upstream_udp, upstream_payload) = UdpHeader::from_slice(upstream_rest).unwrap();
        let translated = build_translated_udp_quote(
            UdpQuoteMetadata {
                source: upstream,
                destination,
                hop_limit: upstream_ip.hop_limit,
                length: upstream_udp.length,
                checksum: upstream_udp.checksum,
            },
            client,
            destination,
            &upstream_payload[..32],
        )
        .unwrap();
        let expected =
            build_udp_quote_with_hop_limit(client, destination, 2, &full_payload).unwrap();
        let (expected_ip, expected_rest) = Ipv6Header::from_slice(&expected).unwrap();
        let (expected_udp, _) = UdpHeader::from_slice(expected_rest).unwrap();
        let (ip, rest) = Ipv6Header::from_slice(&translated).unwrap();
        let (udp, payload) = UdpHeader::from_slice(rest).unwrap();
        assert_eq!(ip.source_addr(), *client.ip());
        assert_eq!(ip.destination_addr(), *destination.ip());
        assert_eq!(ip.hop_limit, 2);
        assert_eq!(ip.payload_length, expected_ip.payload_length);
        assert_eq!(udp.length, expected_udp.length);
        assert_eq!(udp.checksum, expected_udp.checksum);
        assert_eq!(payload, &upstream_payload[..32]);
    }

    #[test]
    fn generated_error_quote_is_capped_to_ipv6_minimum_mtu() {
        let quote = vec![0u8; 4096];
        let packet = build_icmp_error_packet(
            3,
            0,
            [0; 4],
            "fd00::1".parse().unwrap(),
            "fd00::2".parse().unwrap(),
            &quote,
        )
        .unwrap();
        assert_eq!(packet.len(), ICMPV6_MINIMUM_MTU - Ipv6Header::LEN);
    }

    #[test]
    fn selected_library_rejects_malformed_packets() {
        assert!(Icmpv6Header::from_slice(&[ICMPV6_ECHO_REQUEST, 0, 0]).is_err());
        assert!(Ipv6Header::from_slice(&[0; Ipv6Header::LEN - 1]).is_err());
    }
}
