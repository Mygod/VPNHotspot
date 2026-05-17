use std::io;
use std::net::SocketAddrV6;
use std::sync::Arc;

use etherparse::{Icmpv6Header, Icmpv6Type};
use socket2::Socket;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use super::super::sleep_until_deadline;
use super::probe::{
    classify_error_queue_echo, echo_error_response, packet_icmp_error_metadata, quoted_echo_probe,
    quoted_udp_probe, EchoErrorProbe, EchoErrorResponse, UdpErrorProbe,
};
use super::raw_socket::{
    recv_error_queue, recv_raw_icmp_packet, send_downstream_icmp, ReceivedIcmpPacket,
};
use super::state::{EchoState, UpstreamActivity, UpstreamPrune};
use crate::report;
use vpnhotspotd::shared::icmp_nat::{downstream_icmp_error_source, EchoEntry};
use vpnhotspotd::shared::icmp_wire::{
    build_echo_reply_with_checksum, build_echo_request_with_checksum, build_icmp_error_packet,
    build_icmp_quote, build_translated_udp_quote,
};
use vpnhotspotd::shared::model::Network;

pub(super) fn spawn_loop(
    network: Network,
    socket: Arc<AsyncFd<Socket>>,
    changed: Arc<Notify>,
    error_queue: bool,
    state: Arc<EchoState>,
    stop: CancellationToken,
) {
    spawn(async move {
        let mut buffer = vec![0u8; 65535];
        loop {
            let deadline = match state.upstream_activity(network) {
                Ok(UpstreamActivity::Active(deadline)) => deadline,
                Ok(UpstreamActivity::Idle) => match state.prune_idle_upstream(network) {
                    Ok(UpstreamPrune::Removed) => break,
                    Ok(UpstreamPrune::StillActive) => continue,
                    Err(e) => {
                        report::io("nat66.icmp_upstream_timeout", e);
                        break;
                    }
                },
                Err(e) => {
                    report::io("nat66.icmp_upstream_timeout", e);
                    break;
                }
            };
            let mut ready = select! {
                _ = stop.cancelled() => break,
                _ = changed.notified() => continue,
                _ = sleep_until_deadline(deadline) => {
                    match state.prune_idle_upstream(network) {
                        Ok(UpstreamPrune::Removed) => break,
                        Ok(UpstreamPrune::StillActive) => continue,
                        Err(e) => {
                            report::io("nat66.icmp_upstream_timeout", e);
                            break;
                        }
                    }
                }
                ready = socket.readable() => match ready {
                    Ok(ready) => ready,
                    Err(e) => {
                        report::io("nat66.icmp_upstream_readable", e);
                        break;
                    }
                },
            };
            loop {
                if stop.is_cancelled() {
                    break;
                }
                if error_queue {
                    match drain_echo_error_queue(&socket, network, &state).await {
                        Ok(_) => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => report::io_with_details(
                            "nat66.icmp_echo_error_queue_recv",
                            e,
                            [("network", network.to_string())],
                        ),
                    }
                }
                match recv_raw_icmp_packet(&socket, &mut buffer) {
                    Ok(Some(packet)) => handle_upstream_icmp_packet(network, &state, packet).await,
                    Ok(None) => continue,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        ready.clear_ready();
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        report::io_with_details(
                            "nat66.icmp_upstream_recv",
                            e,
                            [("network", network.to_string())],
                        );
                        break;
                    }
                }
            }
        }
        if let Err(e) = state.remove_upstream_socket(network, &socket) {
            report::io("nat66.icmp_upstream_remove", e);
        }
    });
}

async fn handle_upstream_icmp_packet(
    network: Network,
    state: &EchoState,
    packet: ReceivedIcmpPacket<'_>,
) {
    let Ok((header, payload)) = Icmpv6Header::from_slice(packet.payload) else {
        return;
    };
    match header.icmp_type {
        Icmpv6Type::EchoReply(echo) => {
            handle_upstream_echo_reply(network, state, packet.source, echo.id, echo.seq, payload)
                .await;
        }
        _ => {
            if let Some((icmp_type, code, bytes5to8)) =
                packet_icmp_error_metadata(packet.payload, &header)
            {
                handle_upstream_icmp_error(
                    network,
                    state,
                    packet.source,
                    icmp_type,
                    code,
                    bytes5to8,
                    payload,
                )
                .await;
            }
        }
    }
}

async fn handle_upstream_echo_reply(
    network: Network,
    state: &EchoState,
    source: SocketAddrV6,
    id: u16,
    seq: u16,
    payload: &[u8],
) {
    let entry = match state.restore(network, *source.ip(), id, seq) {
        Ok(Some(entry)) => entry,
        Ok(None) => return,
        Err(e) => {
            report::io("nat66.icmp_echo_restore", e);
            return;
        }
    };
    let reply = match build_echo_reply_with_checksum(
        *source.ip(),
        *entry.client.ip(),
        entry.original_id,
        entry.original_seq,
        payload,
    ) {
        Ok(reply) => reply,
        Err(e) => {
            report::io("nat66.icmp_echo_reply_packet", e);
            return;
        }
    };
    if let Err(e) = send_downstream_icmp(
        &entry.downstream,
        entry.reply_mark,
        *source.ip(),
        entry.client,
        &reply,
    )
    .await
    {
        report::stdout!(
            "icmp echo reply dropped: source={} client={}: {}",
            source.ip(),
            entry.client,
            e
        );
    }
}

async fn handle_upstream_icmp_error(
    network: Network,
    state: &EchoState,
    offender: SocketAddrV6,
    icmp_type: u8,
    code: u8,
    bytes5to8: [u8; 4],
    quote: &[u8],
) {
    if let Some(probe) = quoted_echo_probe(quote) {
        handle_upstream_echo_error(network, state, offender, icmp_type, code, bytes5to8, probe)
            .await;
        return;
    }
    if let Some(probe) = quoted_udp_probe(quote) {
        handle_upstream_udp_error(network, state, offender, icmp_type, code, bytes5to8, probe)
            .await;
    }
}

async fn handle_upstream_echo_error(
    network: Network,
    state: &EchoState,
    offender: SocketAddrV6,
    icmp_type: u8,
    code: u8,
    bytes5to8: [u8; 4],
    probe: EchoErrorProbe<'_>,
) {
    let entry = match state.restore(network, probe.destination, probe.id, probe.seq) {
        Ok(Some(entry)) => entry,
        Ok(None) => return,
        Err(e) => {
            report::io("nat66.icmp_echo_restore", e);
            return;
        }
    };
    let source = downstream_icmp_error_source(*offender.ip(), entry.gateway);
    let hop_limit = probe.hop_limit.unwrap_or(entry.upstream_hop_limit);
    send_echo_error(
        entry,
        EchoErrorResponse {
            source,
            icmp_type,
            code,
            bytes5to8,
            destination: probe.destination,
            hop_limit,
        },
        probe.payload,
    )
    .await
}

async fn handle_upstream_udp_error(
    network: Network,
    state: &EchoState,
    offender: SocketAddrV6,
    icmp_type: u8,
    code: u8,
    bytes5to8: [u8; 4],
    probe: UdpErrorProbe<'_>,
) {
    let context = match state.lookup_udp_error(network, probe.quote.source, probe.quote.destination)
    {
        Ok(Some(context)) => context,
        Ok(None) => return,
        Err(e) => {
            report::io("nat66.icmp_udp_lookup", e);
            return;
        }
    };
    let source = downstream_icmp_error_source(*offender.ip(), context.gateway);
    let quote = match build_translated_udp_quote(
        probe.quote,
        context.client,
        context.destination,
        probe.payload,
    ) {
        Ok(quote) => quote,
        Err(e) => {
            report::io_with_details(
                "nat66.icmp_udp_quote",
                e,
                [
                    ("client", context.client.to_string()),
                    ("destination", context.destination.to_string()),
                ],
            );
            return;
        }
    };
    let packet = match build_icmp_error_packet(
        icmp_type,
        code,
        bytes5to8,
        source,
        *context.client.ip(),
        &quote,
    ) {
        Ok(packet) => packet,
        Err(e) => {
            report::io_with_details(
                "nat66.icmp_udp_error_packet",
                e,
                [
                    ("client", context.client.to_string()),
                    ("destination", context.destination.to_string()),
                ],
            );
            return;
        }
    };
    if let Err(e) = send_downstream_icmp(
        &context.downstream,
        context.reply_mark,
        source,
        context.client,
        &packet,
    )
    .await
    {
        report::stdout!(
            "udp icmp error dropped: source={} client={} destination={}: {}",
            source,
            context.client,
            context.destination,
            e
        );
    }
}

pub(super) async fn drain_echo_error_queue(
    socket: &AsyncFd<Socket>,
    network: Network,
    state: &EchoState,
) -> io::Result<usize> {
    let mut drained = 0;
    loop {
        let message = match recv_error_queue(socket) {
            Ok(message) => message,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(drained),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        drained += 1;
        let Some((probe, error)) = classify_error_queue_echo(&message) else {
            continue;
        };
        let Some(entry) = state.restore(network, probe.destination, probe.id, probe.seq)? else {
            continue;
        };
        let response = echo_error_response(error, &entry, &probe);
        send_echo_error(entry, response, probe.payload).await
    }
}

async fn send_echo_error(entry: EchoEntry, response: EchoErrorResponse, payload: &[u8]) {
    let request = match build_echo_request_with_checksum(
        *entry.client.ip(),
        response.destination,
        entry.original_id,
        entry.original_seq,
        payload,
    ) {
        Ok(request) => request,
        Err(e) => {
            report::io_with_details(
                "nat66.icmp_echo_error_quote_request",
                e,
                [
                    ("client", entry.client.to_string()),
                    ("destination", response.destination.to_string()),
                ],
            );
            return;
        }
    };
    let quote = match build_icmp_quote(
        *entry.client.ip(),
        response.destination,
        response.hop_limit,
        &request,
    ) {
        Ok(quote) => quote,
        Err(e) => {
            report::io_with_details(
                "nat66.icmp_echo_error_quote",
                e,
                [
                    ("client", entry.client.to_string()),
                    ("destination", response.destination.to_string()),
                ],
            );
            return;
        }
    };
    let packet = match build_icmp_error_packet(
        response.icmp_type,
        response.code,
        response.bytes5to8,
        response.source,
        *entry.client.ip(),
        &quote,
    ) {
        Ok(packet) => packet,
        Err(e) => {
            report::io_with_details(
                "nat66.icmp_echo_error_packet",
                e,
                [
                    ("client", entry.client.to_string()),
                    ("destination", response.destination.to_string()),
                ],
            );
            return;
        }
    };
    if let Err(e) = send_downstream_icmp(
        &entry.downstream,
        entry.reply_mark,
        response.source,
        entry.client,
        &packet,
    )
    .await
    {
        report::stdout!(
            "icmp echo error dropped: source={} client={}: {}",
            response.source,
            entry.client,
            e
        );
    }
}
