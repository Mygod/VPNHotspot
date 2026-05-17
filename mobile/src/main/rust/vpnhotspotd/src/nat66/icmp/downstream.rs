use std::collections::HashMap;
use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use etherparse::{Icmpv6Header, Icmpv6Type, IpNumber, Ipv6Slice};
use nfq::{Queue, Verdict};
use tokio::io::unix::AsyncFd;

use super::raw_socket::{send_downstream_icmp, send_upstream_echo, set_upstream_echo_hop_limit};
use super::state::EchoState;
use super::IcmpSession;
use crate::report;
use vpnhotspotd::shared::icmp_nat::{
    classify_nat66_destination, nat66_hop_limit, EchoAllocation, Nat66Destination, Nat66HopLimit,
    ICMPV6_PACKET_TOO_BIG, ICMPV6_TIME_EXCEEDED,
};
use vpnhotspotd::shared::icmp_wire::{
    build_echo_request_zero_checksum, build_icmp_error_packet, build_icmp_quote,
    build_udp_quote_with_hop_limit,
};
use vpnhotspotd::shared::model::{select_network, SessionConfig};

struct DownstreamIcmpPacket {
    input_interface: u32,
    source: SocketAddrV6,
    destination: Ipv6Addr,
    hop_limit: u8,
    payload: Vec<u8>,
}

enum QueuedEchoAction {
    Accept,
    Drop,
    Handle,
}

pub(super) async fn run_queue(
    mut queue: AsyncFd<Queue>,
    registrations: Arc<StdMutex<HashMap<u32, Weak<IcmpSession>>>>,
    state: Arc<EchoState>,
) {
    loop {
        let mut ready = match queue.readable_mut().await {
            Ok(ready) => ready,
            Err(e) => {
                report::io("nat66.icmp_queue_readable", e);
                break;
            }
        };
        loop {
            let message = match ready.try_io(|queue| queue.get_mut().recv()) {
                Ok(Ok(message)) => message,
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => {
                    report::io("nat66.icmp_queue_recv", e);
                    break;
                }
                Err(_) => break,
            };
            let Some(packet) = downstream_packet(&message) else {
                if let Err(e) = drop_queued_packet(ready.get_mut().get_mut(), message) {
                    report::io("nat66.icmp_queue_verdict", e);
                    break;
                }
                continue;
            };
            let session = match registrations.lock() {
                Ok(mut registrations) => {
                    match registrations
                        .get(&packet.input_interface)
                        .and_then(Weak::upgrade)
                    {
                        Some(session) => Some(session),
                        None => {
                            registrations.remove(&packet.input_interface);
                            None
                        }
                    }
                }
                Err(_) => {
                    report::message(
                        "nat66.icmp_registrations",
                        "icmp registrations state poisoned",
                        "PoisonError",
                    );
                    None
                }
            };
            let Some(session) = session else {
                if let Err(e) = accept_queued_packet(ready.get_mut().get_mut(), message) {
                    report::io("nat66.icmp_queue_verdict", e);
                    break;
                }
                continue;
            };
            drop(ready);
            let snapshot = session.config.lock().await.clone();
            match queued_echo_action(&packet, &snapshot) {
                QueuedEchoAction::Accept => {
                    if let Err(e) = accept_queued_packet(queue.get_mut(), message) {
                        report::io("nat66.icmp_queue_verdict", e);
                        break;
                    }
                }
                QueuedEchoAction::Drop => {
                    if let Err(e) = drop_queued_packet(queue.get_mut(), message) {
                        report::io("nat66.icmp_queue_verdict", e);
                        break;
                    }
                }
                QueuedEchoAction::Handle => {
                    if let Err(e) = drop_queued_packet(queue.get_mut(), message) {
                        report::io("nat66.icmp_queue_verdict", e);
                        break;
                    }
                    handle_downstream_echo(packet, &snapshot, session.session_key, state.clone())
                        .await;
                }
            }
            break;
        }
    }
}

fn queued_echo_action(packet: &DownstreamIcmpPacket, config: &SessionConfig) -> QueuedEchoAction {
    let Ok((header, _)) = Icmpv6Header::from_slice(&packet.payload) else {
        return QueuedEchoAction::Drop;
    };
    if !matches!(header.icmp_type, Icmpv6Type::EchoRequest(_)) {
        return QueuedEchoAction::Drop;
    }
    let Some(ipv6_nat) = config.ipv6_nat.as_ref() else {
        return QueuedEchoAction::Accept;
    };
    match classify_nat66_destination(packet.destination, ipv6_nat.gateway.address()) {
        Nat66Destination::Gateway | Nat66Destination::Special => QueuedEchoAction::Accept,
        Nat66Destination::Routable => QueuedEchoAction::Handle,
    }
}

fn accept_queued_packet(queue: &mut Queue, mut message: nfq::Message) -> io::Result<()> {
    message.set_verdict(Verdict::Accept);
    queue.verdict(message)
}

fn drop_queued_packet(queue: &mut Queue, mut message: nfq::Message) -> io::Result<()> {
    message.set_verdict(Verdict::Drop);
    queue.verdict(message)
}

fn downstream_packet(message: &nfq::Message) -> Option<DownstreamIcmpPacket> {
    let ipv6 = Ipv6Slice::from_slice(message.get_payload()).ok()?;
    let payload = ipv6.payload();
    if payload.fragmented || payload.ip_number != IpNumber::IPV6_ICMP {
        return None;
    }
    let header = ipv6.header();
    let source_ip = header.source_addr();
    let input_interface = message.get_indev();
    Some(DownstreamIcmpPacket {
        input_interface,
        source: SocketAddrV6::new(
            source_ip,
            0,
            0,
            if source_ip.is_unicast_link_local() {
                input_interface
            } else {
                0
            },
        ),
        destination: header.destination_addr(),
        hop_limit: header.hop_limit(),
        payload: payload.payload.to_vec(),
    })
}

pub(crate) async fn send_udp_time_exceeded(
    downstream: &str,
    reply_mark: u32,
    gateway: Ipv6Addr,
    client: SocketAddrV6,
    destination: SocketAddrV6,
    hop_limit: u8,
    payload: &[u8],
) -> io::Result<()> {
    let quote = build_udp_quote_with_hop_limit(client, destination, hop_limit, payload)?;
    let packet = build_icmp_error_packet(
        ICMPV6_TIME_EXCEEDED,
        0,
        [0; 4],
        gateway,
        *client.ip(),
        &quote,
    )?;
    send_downstream_icmp(downstream, reply_mark, gateway, client, &packet).await
}

pub(crate) async fn send_udp_packet_too_big(
    context: &super::UdpErrorContext,
    mtu: u32,
    hop_limit: u8,
    payload: &[u8],
) -> io::Result<()> {
    let quote =
        build_udp_quote_with_hop_limit(context.client, context.destination, hop_limit, payload)?;
    let packet = build_icmp_error_packet(
        ICMPV6_PACKET_TOO_BIG,
        0,
        mtu.to_be_bytes(),
        context.gateway,
        *context.client.ip(),
        &quote,
    )?;
    send_downstream_icmp(
        &context.downstream,
        context.reply_mark,
        context.gateway,
        context.client,
        &packet,
    )
    .await
}

async fn handle_downstream_echo(
    packet: DownstreamIcmpPacket,
    config: &SessionConfig,
    session_key: u64,
    state: Arc<EchoState>,
) {
    let Ok((header, payload)) = Icmpv6Header::from_slice(&packet.payload) else {
        return;
    };
    let Icmpv6Type::EchoRequest(echo) = header.icmp_type else {
        return;
    };
    let Some(ipv6_nat) = config.ipv6_nat.as_ref() else {
        return;
    };
    match classify_nat66_destination(packet.destination, ipv6_nat.gateway.address()) {
        Nat66Destination::Gateway | Nat66Destination::Special => return,
        Nat66Destination::Routable => {}
    }
    let upstream_hop_limit = match nat66_hop_limit(Some(packet.hop_limit)) {
        Nat66HopLimit::Expired => {
            let quote = match build_icmp_quote(
                *packet.source.ip(),
                packet.destination,
                packet.hop_limit,
                &packet.payload,
            ) {
                Ok(quote) => quote,
                Err(e) => {
                    report::io_with_details(
                        "nat66.icmp_time_exceeded_quote",
                        e,
                        [
                            ("client", packet.source.to_string()),
                            ("destination", packet.destination.to_string()),
                        ],
                    );
                    return;
                }
            };
            let response = match build_icmp_error_packet(
                ICMPV6_TIME_EXCEEDED,
                0,
                [0; 4],
                ipv6_nat.gateway.address(),
                *packet.source.ip(),
                &quote,
            ) {
                Ok(response) => response,
                Err(e) => {
                    report::io("nat66.icmp_time_exceeded_packet", e);
                    return;
                }
            };
            if let Err(e) = send_downstream_icmp(
                &config.downstream,
                config.reply_mark,
                ipv6_nat.gateway.address(),
                packet.source,
                &response,
            )
            .await
            {
                report::stdout!(
                    "icmp time exceeded dropped: source={} client={} destination={}: {}",
                    ipv6_nat.gateway.address(),
                    packet.source,
                    packet.destination,
                    e
                );
            }
            return;
        }
        Nat66HopLimit::Forward(hop_limit) => hop_limit,
        Nat66HopLimit::Missing => return,
    };
    let Some(network) = select_network(config, packet.destination) else {
        return;
    };
    let (id, seq, socket) = match EchoState::allocate_echo(
        &state,
        EchoAllocation {
            session_key,
            downstream: config.downstream.clone(),
            reply_mark: config.reply_mark,
            network,
            destination: packet.destination,
            client: packet.source,
            original_id: echo.id,
            original_seq: echo.seq,
            downstream_hop_limit: packet.hop_limit,
            upstream_hop_limit,
            gateway: ipv6_nat.gateway.address(),
        },
    ) {
        Ok(allocation) => allocation,
        Err(e) => {
            report::io("nat66.icmp_echo_map", e);
            return;
        }
    };
    let request = build_echo_request_zero_checksum(id, seq, payload);
    if let Err(e) = set_upstream_echo_hop_limit(&socket.socket, upstream_hop_limit) {
        report::io_with_details(
            "nat66.icmp_hop_limit",
            e,
            [
                ("client", packet.source.to_string()),
                ("destination", packet.destination.to_string()),
                ("network", network.to_string()),
            ],
        );
        if let Err(e) = state.remove_allocation(network, packet.destination, id, seq) {
            report::io("nat66.icmp_echo_remove_failed_send", e);
        }
        return;
    }
    let destination = SocketAddrV6::new(packet.destination, 0, 0, 0);
    let mut retried_after_error_queue = false;
    loop {
        match send_upstream_echo(&socket.socket, destination, &request).await {
            Ok(()) => break,
            Err(e) => {
                let checked_error_queue = if socket.error_queue {
                    match super::upstream::drain_echo_error_queue(&socket.socket, network, &state)
                        .await
                    {
                        Ok(_) => true,
                        Err(error) => {
                            report::io_with_details(
                                "nat66.icmp_echo_error_queue_recv",
                                error,
                                [("network", network.to_string())],
                            );
                            false
                        }
                    }
                } else {
                    false
                };
                match state.has_allocation(network, packet.destination, id, seq) {
                    Ok(false) => break,
                    Err(error) => {
                        report::io("nat66.icmp_echo_lookup_failed_send", error);
                    }
                    Ok(true) => {
                        if !retried_after_error_queue && checked_error_queue {
                            retried_after_error_queue = true;
                            continue;
                        }
                    }
                }
                report::io_with_details(
                    "nat66.icmp_upstream_send",
                    e,
                    [
                        ("client", packet.source.to_string()),
                        ("destination", packet.destination.to_string()),
                        ("network", network.to_string()),
                    ],
                );
                if let Err(e) = state.remove_allocation(network, packet.destination, id, seq) {
                    report::io("nat66.icmp_echo_remove_failed_send", e);
                }
                break;
            }
        }
    }
}
