use std::net::{Ipv6Addr, SocketAddrV6};

use etherparse::{Icmpv6Header, Icmpv6Type, IpNumber, Ipv6Header, UdpHeader};

use super::raw_socket::{
    ErrorQueueMessage, Ipv6RecvError, EMSGSIZE, SO_EE_ORIGIN_ICMP6, SO_EE_ORIGIN_LOCAL,
};
use vpnhotspotd::shared::icmp_nat::{
    downstream_icmp_error_source, icmpv6_error_bytes, EchoEntry, ICMPV6_PACKET_TOO_BIG,
};
use vpnhotspotd::shared::icmp_wire::UdpQuoteMetadata;

pub(super) struct EchoErrorProbe<'a> {
    pub(super) destination: Ipv6Addr,
    pub(super) id: u16,
    pub(super) seq: u16,
    pub(super) hop_limit: Option<u8>,
    pub(super) payload: &'a [u8],
}

pub(super) struct UdpErrorProbe<'a> {
    pub(super) quote: UdpQuoteMetadata,
    pub(super) payload: &'a [u8],
}

pub(super) struct EchoErrorResponse {
    pub(super) source: Ipv6Addr,
    pub(super) icmp_type: u8,
    pub(super) code: u8,
    pub(super) bytes5to8: [u8; 4],
    pub(super) destination: Ipv6Addr,
    pub(super) hop_limit: u8,
}

pub(super) enum QueuedEchoError {
    Upstream {
        offender: SocketAddrV6,
        icmp_type: u8,
        code: u8,
        bytes5to8: [u8; 4],
    },
    LocalPacketTooBig {
        mtu: u32,
    },
}

pub(super) fn classify_error_queue_echo(
    message: &ErrorQueueMessage,
) -> Option<(EchoErrorProbe<'_>, QueuedEchoError)> {
    let probe = error_queue_echo_probe(message)?;
    let error = match message.error.origin {
        SO_EE_ORIGIN_ICMP6 => {
            let (icmp_type, bytes5to8) = error_type_bytes(&message.error)?;
            QueuedEchoError::Upstream {
                offender: message.offender?,
                icmp_type,
                code: message.error.code,
                bytes5to8,
            }
        }
        SO_EE_ORIGIN_LOCAL if message.error.errno == EMSGSIZE => {
            let mtu = message.error.info;
            if mtu == 0 {
                return None;
            }
            QueuedEchoError::LocalPacketTooBig { mtu }
        }
        _ => return None,
    };
    Some((probe, error))
}

pub(super) fn echo_error_response(
    error: QueuedEchoError,
    entry: &EchoEntry,
    probe: &EchoErrorProbe<'_>,
) -> EchoErrorResponse {
    match error {
        QueuedEchoError::Upstream {
            offender,
            icmp_type,
            code,
            bytes5to8,
        } => EchoErrorResponse {
            source: downstream_icmp_error_source(*offender.ip(), entry.gateway),
            icmp_type,
            code,
            bytes5to8,
            destination: probe.destination,
            hop_limit: probe.hop_limit.unwrap_or(entry.upstream_hop_limit),
        },
        QueuedEchoError::LocalPacketTooBig { mtu } => EchoErrorResponse {
            source: entry.gateway,
            icmp_type: ICMPV6_PACKET_TOO_BIG,
            code: 0,
            bytes5to8: mtu.to_be_bytes(),
            destination: probe.destination,
            hop_limit: entry.downstream_hop_limit,
        },
    }
}

fn error_queue_echo_probe(message: &ErrorQueueMessage) -> Option<EchoErrorProbe<'_>> {
    if let Some(probe) = quoted_echo_probe(&message.payload) {
        return Some(probe);
    }
    let destination = *message.destination?.ip();
    let (icmp, payload) = Icmpv6Header::from_slice(&message.payload).ok()?;
    let Icmpv6Type::EchoRequest(echo) = icmp.icmp_type else {
        return None;
    };
    Some(EchoErrorProbe {
        destination,
        id: echo.id,
        seq: echo.seq,
        hop_limit: None,
        payload,
    })
}

pub(super) fn quoted_echo_probe(payload: &[u8]) -> Option<EchoErrorProbe<'_>> {
    let (ip, rest) = Ipv6Header::from_slice(payload).ok()?;
    if ip.next_header != IpNumber::IPV6_ICMP {
        return None;
    }
    let (icmp, payload) = Icmpv6Header::from_slice(rest).ok()?;
    let Icmpv6Type::EchoRequest(echo) = icmp.icmp_type else {
        return None;
    };
    Some(EchoErrorProbe {
        destination: ip.destination_addr(),
        id: echo.id,
        seq: echo.seq,
        hop_limit: Some(ip.hop_limit),
        payload,
    })
}

pub(super) fn quoted_udp_probe(payload: &[u8]) -> Option<UdpErrorProbe<'_>> {
    let (ip, rest) = Ipv6Header::from_slice(payload).ok()?;
    if ip.next_header != IpNumber::UDP {
        return None;
    }
    let (udp, payload) = UdpHeader::from_slice(rest).ok()?;
    if ip.payload_length != udp.length
        || udp.length < UdpHeader::LEN_U16
        || usize::from(udp.length) < UdpHeader::LEN + payload.len()
    {
        return None;
    }
    Some(UdpErrorProbe {
        quote: UdpQuoteMetadata {
            source: normalize_udp_error_addr(SocketAddrV6::new(
                ip.source_addr(),
                udp.source_port,
                0,
                0,
            )),
            destination: normalize_udp_error_addr(SocketAddrV6::new(
                ip.destination_addr(),
                udp.destination_port,
                0,
                0,
            )),
            hop_limit: ip.hop_limit,
            length: udp.length,
            checksum: udp.checksum,
        },
        payload,
    })
}

pub(super) fn packet_icmp_error_metadata(
    packet: &[u8],
    header: &Icmpv6Header,
) -> Option<(u8, u8, [u8; 4])> {
    let type_u8 = header.icmp_type.type_u8();
    if icmpv6_error_bytes(type_u8, 0).is_none() || packet.len() < 8 {
        return None;
    }
    Some((
        type_u8,
        header.icmp_type.code_u8(),
        packet[4..8].try_into().ok()?,
    ))
}

pub(super) fn normalize_udp_error_addr(address: SocketAddrV6) -> SocketAddrV6 {
    SocketAddrV6::new(*address.ip(), address.port(), 0, 0)
}

fn error_type_bytes(error: &Ipv6RecvError) -> Option<(u8, [u8; 4])> {
    icmpv6_error_bytes(error.icmp_type, error.info)
}
