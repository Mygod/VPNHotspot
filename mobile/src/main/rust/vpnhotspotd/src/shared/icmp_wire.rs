use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};

use etherparse::{IcmpEchoHeader, Icmpv6Type, IpNumber, Ipv6Header, UdpHeader};

pub const ICMPV6_HEADER_LEN: usize = 8;
pub const ICMPV6_ECHO_REQUEST: u8 = 128;
pub const ICMPV6_ECHO_REPLY: u8 = 129;
const ICMPV6_MINIMUM_MTU: usize = 1280;

pub fn build_echo_packet_zero_checksum(type_u8: u8, id: u16, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(ICMPV6_HEADER_LEN + payload.len());
    packet.extend([type_u8, 0, 0, 0]);
    packet.extend_from_slice(&id.to_be_bytes());
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(payload);
    packet
}

pub fn build_echo_packet_with_checksum(
    type_u8: u8,
    source: Ipv6Addr,
    destination: Ipv6Addr,
    id: u16,
    seq: u16,
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    let echo = IcmpEchoHeader { id, seq };
    let icmp_type = match type_u8 {
        ICMPV6_ECHO_REPLY => Icmpv6Type::EchoReply(echo),
        ICMPV6_ECHO_REQUEST => Icmpv6Type::EchoRequest(echo),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported echo type",
            ))
        }
    };
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

pub fn cap_error_quote(quote: &[u8]) -> &[u8] {
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

pub fn build_udp_quote(
    client: SocketAddrV6,
    destination: SocketAddrV6,
    payload: &[u8],
) -> io::Result<Vec<u8>> {
    build_udp_quote_with_hop_limit(client, destination, 64, payload)
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

fn cap_invoking_icmp_payload(payload: &[u8]) -> &[u8] {
    let max_payload = ICMPV6_MINIMUM_MTU - Ipv6Header::LEN - ICMPV6_HEADER_LEN - Ipv6Header::LEN;
    &payload[..payload.len().min(max_payload)]
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
        let packet = build_echo_packet_with_checksum(
            ICMPV6_ECHO_REQUEST,
            source,
            destination,
            11,
            22,
            b"probe",
        )
        .unwrap();
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
        let packet =
            build_echo_packet_with_checksum(ICMPV6_ECHO_REPLY, source, destination, 7, 9, b"abc")
                .unwrap();
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
        let request = build_echo_packet_zero_checksum(ICMPV6_ECHO_REQUEST, 11, 22, b"probe");
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
        let quote = build_udp_quote(client, destination, b"payload").unwrap();
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
