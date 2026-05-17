use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, SocketAddrV6, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use super::{sleep_until_deadline, IDLE_TIMEOUT};
use crate::dns::{resolve_or_error, DNS_PORT};
use crate::nat66::icmp;
use crate::report;
use crate::upstream::connect_udp;
use vpnhotspotd::shared::icmp_nat::{is_special_destination, nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::model::{select_network, SessionConfig};

mod icmp_error;
mod reply_socket;
mod socket_io;

use icmp_error::send_packet_too_big as send_udp_packet_too_big;
use reply_socket::{
    report_send_response_error, send_response, ReplySocketKey, ReplySocketPool,
    ReplySocketReservation,
};
use socket_io::{
    enable_recv_hop_limit, forward_udp_datagram, is_udp_icmp_error, recv_packet, UdpForwardResult,
};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AssociationKey {
    client: SocketAddrV6,
    destination: SocketAddrV6,
}

struct UdpAssociation {
    id: u64,
    socket: Arc<TokioUdpSocket>,
    icmp_errors: Option<icmp::UdpErrorRegistration>,
    last_active: Instant,
    stop: CancellationToken,
}

enum UdpAssociationEvent {
    Active(AssociationKey, u64),
    Closed(AssociationKey, u64),
}

struct AssociationTask {
    key: AssociationKey,
    id: u64,
    socket: Arc<TokioUdpSocket>,
    reply_reservation: ReplySocketReservation,
    reply_socket: Option<Arc<TokioUdpSocket>>,
    icmp_errors_registered: bool,
    stop: CancellationToken,
    association_event_tx: mpsc::UnboundedSender<UdpAssociationEvent>,
}

pub(crate) fn spawn_loop(
    listener: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    icmp_dispatcher: icmp::Dispatcher,
) -> io::Result<()> {
    let listener = AsyncFd::new(listener)?;
    spawn(async move {
        let listener_fd = listener.get_ref().as_raw_fd();
        if let Err(e) = enable_recv_hop_limit(listener.get_ref()) {
            report::io("nat66.udp_hop_limit_setup", e);
        }
        let reply_sockets = Arc::new(ReplySocketPool::default());
        let mut associations = HashMap::<AssociationKey, UdpAssociation>::new();
        let (association_event_tx, mut association_event_rx) = mpsc::unbounded_channel();
        let mut next_association_id = 0u64;
        let mut buffer = [0u8; 65535];
        let handle_association_event =
            |associations: &mut HashMap<AssociationKey, UdpAssociation>, event| match event {
                UdpAssociationEvent::Active(key, id) => {
                    if let Some(association) = associations.get_mut(&key) {
                        if association.id == id {
                            association.last_active = Instant::now();
                        }
                    }
                }
                UdpAssociationEvent::Closed(key, id) => {
                    let remove = associations
                        .get(&key)
                        .is_some_and(|association| association.id == id);
                    if remove {
                        if let Some(association) = associations.remove(&key) {
                            association.stop.cancel();
                        }
                    }
                }
            };
        loop {
            while let Ok(event) = association_event_rx.try_recv() {
                handle_association_event(&mut associations, event);
            }
            let now = Instant::now();
            associations.retain(|_, association| {
                let active = now.duration_since(association.last_active) < IDLE_TIMEOUT;
                if !active {
                    association.stop.cancel();
                }
                active
            });
            let next_expiry = associations
                .values()
                .map(|association| association.last_active + IDLE_TIMEOUT)
                .min();

            select! {
                _ = stop.cancelled() => break,
                _ = sleep_until_deadline(next_expiry) => {}
                event = association_event_rx.recv() => match event {
                    Some(event) => handle_association_event(&mut associations, event),
                    None => break,
                },
                ready = listener.readable() => {
                    let mut ready = match ready {
                        Ok(ready) => ready,
                        Err(e) => {
                            report::io("nat66.udp_readable", e);
                            break;
                        }
                    };
                    loop {
                        match recv_packet(listener_fd, &mut buffer) {
                            Ok((size, client, destination, hop_limit)) => {
                                let activity = Instant::now();
                                let snapshot = config.lock().await.clone();
                                let Some(ipv6_nat) = snapshot.ipv6_nat.as_ref() else {
                                    continue;
                                };
                                if *destination.ip() == ipv6_nat.gateway.address() && destination.port() == DNS_PORT {
                                    let query = buffer[..size].to_vec();
                                    let reply_key = ReplySocketKey {
                                        source: destination,
                                        mark: snapshot.reply_mark,
                                    };
                                    let reply_socket = match reply_sockets.retain_dns(reply_key) {
                                        Ok(socket) => socket,
                                        Err(e) => {
                                            report::io_with_details(
                                                "nat66.dns_udp_reply_socket",
                                                e,
                                                [
                                                    ("client", client.to_string()),
                                                    ("destination", destination.to_string()),
                                                ],
                                            );
                                            continue;
                                        }
                                    };
                                    let reply_sockets = reply_sockets.clone();
                                    let query_stop = stop.child_token();
                                    spawn(async move {
                                        let mut reply_socket = Some(reply_socket);
                                        select! {
                                            _ = query_stop.cancelled() => {}
                                            response = resolve_or_error(&snapshot, &query) => {
                                                if let Some(response) = response {
                                                    if let Err(e) = send_response(
                                                        &reply_sockets,
                                                        reply_key,
                                                        &mut reply_socket,
                                                        client,
                                                        &response,
                                                    ).await {
                                                        report_send_response_error(
                                                            "nat66.dns_udp_response",
                                                            e,
                                                            client,
                                                            destination,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    });
                                    continue;
                                }
                                if is_special_destination(*destination.ip()) {
                                    continue;
                                }
                                let downstream_hop_limit = match hop_limit {
                                    Some(hop_limit) => hop_limit,
                                    None => {
                                        report::message(
                                            "nat66.udp_hop_limit_missing",
                                            "missing downstream hop limit",
                                            "InvalidData",
                                        );
                                        continue;
                                    }
                                };
                                let upstream_hop_limit = match nat66_hop_limit(Some(downstream_hop_limit)) {
                                    Nat66HopLimit::Expired => {
                                        if let Err(e) = icmp::send_udp_time_exceeded(
                                            &snapshot.downstream,
                                            snapshot.reply_mark,
                                            ipv6_nat.gateway.address(),
                                            client,
                                            destination,
                                            downstream_hop_limit,
                                            &buffer[..size],
                                        ).await {
                                            report::io_with_details(
                                                "nat66.udp_time_exceeded",
                                                e,
                                                [
                                                    ("client", client.to_string()),
                                                    ("destination", destination.to_string()),
                                                ],
                                            );
                                        }
                                        continue;
                                    }
                                    Nat66HopLimit::Forward(hop_limit) => hop_limit,
                                    Nat66HopLimit::Missing => continue,
                                };
                                let network = match select_network(&snapshot, *destination.ip()) {
                                    Some(network) => network,
                                    None => continue,
                                };
                                let key = AssociationKey { client, destination };
                                if let Some((socket, icmp_errors_registered)) = associations
                                    .get(&key)
                                    .map(|association| {
                                        (
                                            association.socket.clone(),
                                            association.icmp_errors.is_some(),
                                        )
                                    })
                                {
                                    match forward_udp_datagram(
                                        &socket,
                                        upstream_hop_limit,
                                        &buffer[..size],
                                        client,
                                        destination,
                                        icmp_errors_registered,
                                    ) {
                                        UdpForwardResult::Sent => {
                                            if let Some(association) = associations.get_mut(&key) {
                                                association.last_active = activity;
                                            }
                                        }
                                        UdpForwardResult::Dropped => {}
                                        UdpForwardResult::PacketTooBig(mtu) => {
                                            send_udp_packet_too_big(
                                                &snapshot,
                                                ipv6_nat.gateway.address(),
                                                client,
                                                destination,
                                                mtu,
                                                downstream_hop_limit,
                                                &buffer[..size],
                                            )
                                            .await;
                                        }
                                        UdpForwardResult::Failed => {
                                            if let Some(association) = associations.remove(&key) {
                                                association.stop.cancel();
                                            }
                                        }
                                    }
                                    continue;
                                }
                                let upstream = match connect_udp(network, destination).await {
                                    Ok(socket) => socket,
                                    Err(e) => {
                                        report::io_with_details(
                                            "nat66.udp_connect",
                                            e,
                                            [
                                                ("client", client.to_string()),
                                                ("destination", destination.to_string()),
                                            ],
                                        );
                                        continue;
                                    }
                                };
                                let reply_key = ReplySocketKey {
                                    source: destination,
                                    mark: snapshot.reply_mark,
                                };
                                let reply_reservation = match reply_sockets.reserve_user(reply_key) {
                                    Ok(reservation) => reservation,
                                    Err(e) => {
                                        report::io_with_details(
                                            "nat66.udp_reply_reserve",
                                            e,
                                            [
                                                ("client", client.to_string()),
                                                ("destination", destination.to_string()),
                                            ],
                                        );
                                        continue;
                                    }
                                };
                                let icmp_errors = match upstream.local_addr() {
                                    Ok(SocketAddr::V6(source)) => {
                                        match icmp_dispatcher.register_udp_error(
                                            network,
                                            source,
                                            destination,
                                            icmp::UdpErrorContext {
                                                downstream: snapshot.downstream.clone(),
                                                reply_mark: snapshot.reply_mark,
                                                gateway: ipv6_nat.gateway.address(),
                                                client,
                                                destination,
                                            },
                                        ) {
                                            Ok(registration) => Some(registration),
                                            Err(e) => {
                                                report::io_with_details(
                                                    "nat66.udp_icmp_register",
                                                    e,
                                                    [
                                                        ("client", client.to_string()),
                                                        ("destination", destination.to_string()),
                                                    ],
                                                );
                                                None
                                            }
                                        }
                                    }
                                    Ok(SocketAddr::V4(_)) => {
                                        report::message(
                                            "nat66.udp_icmp_register",
                                            "unexpected IPv4 upstream socket address",
                                            "InvalidData",
                                        );
                                        None
                                    }
                                    Err(e) => {
                                        report::io_with_details(
                                            "nat66.udp_icmp_register",
                                            e,
                                            [
                                                ("client", client.to_string()),
                                                ("destination", destination.to_string()),
                                            ],
                                        );
                                        None
                                    }
                                };
                                let icmp_errors_registered = icmp_errors.is_some();
                                let upstream = Arc::new(upstream);
                                match forward_udp_datagram(
                                    &upstream,
                                    upstream_hop_limit,
                                    &buffer[..size],
                                    client,
                                    destination,
                                    icmp_errors_registered,
                                ) {
                                    UdpForwardResult::Sent => {}
                                    UdpForwardResult::Dropped | UdpForwardResult::Failed => continue,
                                    UdpForwardResult::PacketTooBig(mtu) => {
                                        send_udp_packet_too_big(
                                            &snapshot,
                                            ipv6_nat.gateway.address(),
                                            client,
                                            destination,
                                            mtu,
                                            downstream_hop_limit,
                                            &buffer[..size],
                                        )
                                        .await;
                                        continue;
                                    }
                                }
                                let association_stop = stop.child_token();
                                let association_id = next_association_id;
                                next_association_id = next_association_id.wrapping_add(1);
                                spawn(AssociationTask {
                                    key,
                                    id: association_id,
                                    socket: upstream.clone(),
                                    reply_reservation,
                                    reply_socket: None,
                                    icmp_errors_registered,
                                    stop: association_stop.clone(),
                                    association_event_tx: association_event_tx.clone(),
                                }.run());
                                associations.insert(
                                    key,
                                    UdpAssociation {
                                        id: association_id,
                                        socket: upstream,
                                        icmp_errors,
                                        last_active: activity,
                                        stop: association_stop,
                                    },
                                );
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                ready.clear_ready();
                                break;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                report::io("nat66.udp_recv", e);
                                break;
                            }
                        }
                    }
                }
            }
        }
        for association in associations.values() {
            association.stop.cancel();
        }
    });
    Ok(())
}

impl AssociationTask {
    async fn run(mut self) {
        let mut buffer = [0u8; 65535];
        loop {
            select! {
                _ = self.stop.cancelled() => break,
                result = self.socket.recv(&mut buffer) => match result {
                    Ok(size) => {
                        if let Err(e) = send_response(
                            &self.reply_reservation.pool,
                            self.reply_reservation.key,
                            &mut self.reply_socket,
                            self.key.client,
                            &buffer[..size],
                        ).await {
                            report_send_response_error(
                                "nat66.udp_response",
                                e,
                                self.key.client,
                                self.key.destination,
                            );
                            break;
                        }
                        self.report_active();
                    }
                    Err(e) if self.icmp_errors_registered && is_udp_icmp_error(&e) => continue,
                    Err(e) => {
                        report::io_with_details(
                            "nat66.udp_upstream_recv",
                            e,
                            [
                                ("client", self.key.client.to_string()),
                                ("destination", self.key.destination.to_string()),
                            ],
                        );
                        break;
                    }
                }
            }
        }
        let cancelled = self.stop.is_cancelled();
        self.stop.cancel();
        if self
            .association_event_tx
            .send(UdpAssociationEvent::Closed(self.key, self.id))
            .is_err()
            && !cancelled
        {
            report::message(
                "nat66.udp_association_closed",
                "association event receiver closed",
                "ChannelClosed",
            );
        }
    }

    fn report_active(&self) {
        if self
            .association_event_tx
            .send(UdpAssociationEvent::Active(self.key, self.id))
            .is_err()
            && !self.stop.is_cancelled()
        {
            report::message(
                "nat66.udp_association_active",
                "association event receiver closed",
                "ChannelClosed",
            );
        }
    }
}
