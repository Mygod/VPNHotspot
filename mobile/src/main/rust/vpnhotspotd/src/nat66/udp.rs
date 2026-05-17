use std::collections::{hash_map::Entry, HashMap};
use std::io::{self, IoSliceMut};
use std::mem::size_of;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Instant;

use etherparse::{Ipv6Header, UdpHeader};
use libc::{c_int, c_void, socklen_t, IPPROTO_IPV6};
use nix::cmsg_space;
use nix::sys::socket::{
    recvmsg, send, setsockopt, sockopt, ControlMessageOwned, MsgFlags, SockaddrIn6,
};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
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

enum UdpForwardResult {
    Sent,
    Dropped,
    PacketTooBig(u32),
    Failed,
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ReplySocketKey {
    source: SocketAddrV6,
    mark: u32,
}

struct ReplySocketEntry {
    socket: Option<Arc<TokioUdpSocket>>,
    users: usize,
}

struct RetainedDnsSocket {
    key: ReplySocketKey,
    socket: Arc<TokioUdpSocket>,
}

struct ReplySocketReservation {
    pool: Arc<ReplySocketPool>,
    key: ReplySocketKey,
}

impl Drop for ReplySocketReservation {
    fn drop(&mut self) {
        if let Err(e) = self.pool.release_user(self.key) {
            report::io("nat66.udp_reply_release", e);
        }
    }
}

#[derive(Default)]
struct ReplySocketState {
    retained_dns: Option<RetainedDnsSocket>,
    sockets: HashMap<ReplySocketKey, ReplySocketEntry>,
}

#[derive(Default)]
struct ReplySocketPool {
    state: StdMutex<ReplySocketState>,
}

impl ReplySocketPool {
    fn reserve_user(self: &Arc<Self>, key: ReplySocketKey) -> io::Result<ReplySocketReservation> {
        self.with_state(|state| {
            let retained = state.retained_dns_socket(key);
            match state.sockets.entry(key) {
                Entry::Occupied(mut entry) => {
                    let entry = entry.get_mut();
                    entry.users += 1;
                    if entry.socket.is_none() {
                        entry.socket = retained;
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(ReplySocketEntry {
                        socket: retained,
                        users: 1,
                    });
                }
            }
        })?;
        Ok(ReplySocketReservation {
            pool: self.clone(),
            key,
        })
    }

    fn acquire_reserved(&self, key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.with_state(|state| state.reserved_socket(key))? {
            return Ok(socket);
        }
        let socket = create_reply_socket(key)?;
        self.with_state(|state| {
            if let Some(existing) = state.reserved_socket(key) {
                return existing;
            }
            if let Some(entry) = state.sockets.get_mut(&key) {
                entry.socket = Some(socket.clone());
            }
            socket
        })
    }

    fn retain_dns(&self, key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.with_state(|state| state.retain_existing_dns_socket(key))? {
            return Ok(socket);
        }
        let socket = create_reply_socket(key)?;
        self.with_state(|state| {
            if let Some(existing) = state.retain_existing_dns_socket(key) {
                return existing;
            }
            if let Some(entry) = state.sockets.get_mut(&key) {
                entry.socket = Some(socket.clone());
            }
            state.retain_dns_socket(key, socket.clone());
            socket
        })
    }

    fn release_user(&self, key: ReplySocketKey) -> io::Result<()> {
        self.with_state(|state| -> io::Result<()> {
            let remove = {
                let entry = state
                    .sockets
                    .get_mut(&key)
                    .ok_or_else(|| io::Error::other("reply socket reservation missing"))?;
                if entry.users == 0 {
                    return Err(io::Error::other("reply socket users underflow"));
                }
                entry.users -= 1;
                entry.users == 0
            };
            if remove {
                state.sockets.remove(&key);
            }
            Ok(())
        })?
    }

    fn replace_socket(
        &self,
        key: ReplySocketKey,
        previous: &Arc<TokioUdpSocket>,
    ) -> io::Result<Arc<TokioUdpSocket>> {
        let socket = create_reply_socket(key)?;
        self.with_state(|state| state.replace_socket(key, previous, socket))
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut ReplySocketState) -> T) -> io::Result<T> {
        let mut state = self.lock_state()?;
        Ok(f(&mut state))
    }

    fn lock_state(&self) -> io::Result<MutexGuard<'_, ReplySocketState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("reply socket state poisoned"))
    }
}

impl ReplySocketState {
    fn retained_dns_socket(&self, key: ReplySocketKey) -> Option<Arc<TokioUdpSocket>> {
        self.retained_dns
            .as_ref()
            .filter(|retained| retained.key == key)
            .map(|retained| retained.socket.clone())
    }

    fn retain_dns_socket(&mut self, key: ReplySocketKey, socket: Arc<TokioUdpSocket>) {
        self.retained_dns = Some(RetainedDnsSocket { key, socket });
    }

    fn reserved_socket(&mut self, key: ReplySocketKey) -> Option<Arc<TokioUdpSocket>> {
        if let Some(socket) = self
            .sockets
            .get(&key)
            .and_then(|entry| entry.socket.clone())
        {
            return Some(socket);
        }
        let socket = self.retained_dns_socket(key)?;
        if let Some(entry) = self.sockets.get_mut(&key) {
            entry.socket = Some(socket.clone());
        }
        Some(socket)
    }

    fn retain_existing_dns_socket(&mut self, key: ReplySocketKey) -> Option<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.retained_dns_socket(key) {
            return Some(socket);
        }
        let socket = self
            .sockets
            .get(&key)
            .and_then(|entry| entry.socket.clone())?;
        self.retain_dns_socket(key, socket.clone());
        Some(socket)
    }

    fn replace_socket(
        &mut self,
        key: ReplySocketKey,
        previous: &Arc<TokioUdpSocket>,
        socket: Arc<TokioUdpSocket>,
    ) -> Arc<TokioUdpSocket> {
        let mut current = None;
        let mut replaced = false;
        if let Some(entry) = self.sockets.get_mut(&key) {
            if entry
                .socket
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, previous))
            {
                entry.socket = Some(socket.clone());
                replaced = true;
            }
            current = entry.socket.clone();
        }
        if let Some(retained) = self
            .retained_dns
            .as_mut()
            .filter(|retained| retained.key == key)
        {
            if Arc::ptr_eq(&retained.socket, previous) {
                retained.socket = socket.clone();
                replaced = true;
            }
            if current.is_none() {
                current = Some(retained.socket.clone());
            }
        }
        if replaced {
            socket
        } else {
            current.unwrap_or(socket)
        }
    }
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

fn is_udp_icmp_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::EACCES
                | libc::ECONNREFUSED
                | libc::EHOSTUNREACH
                | libc::EMSGSIZE
                | libc::ENETUNREACH
                | libc::EPROTO
                | libc::ETIMEDOUT
        )
    )
}

async fn send_response(
    reply_sockets: &ReplySocketPool,
    key: ReplySocketKey,
    socket: &mut Option<Arc<TokioUdpSocket>>,
    target: SocketAddrV6,
    payload: &[u8],
) -> Result<(), SendResponseError> {
    let current = match socket {
        Some(socket) => socket.clone(),
        None => {
            let acquired = reply_sockets
                .acquire_reserved(key)
                .map_err(SendResponseError::Acquire)?;
            *socket = Some(acquired.clone());
            acquired
        }
    };
    if let Err(initial) = current.send_to(payload, SocketAddr::V6(target)).await {
        let retry = match reply_sockets.replace_socket(key, &current) {
            Ok(socket) => socket,
            Err(error) => return Err(SendResponseError::Replace { initial, error }),
        };
        *socket = Some(retry.clone());
        retry
            .send_to(payload, SocketAddr::V6(target))
            .await
            .map_err(|error| SendResponseError::Retry { initial, error })?;
    }
    Ok(())
}

enum SendResponseError {
    Acquire(io::Error),
    Replace {
        initial: io::Error,
        error: io::Error,
    },
    Retry {
        initial: io::Error,
        error: io::Error,
    },
}

impl SendResponseError {
    fn into_report_parts(self) -> (io::Error, Option<io::Error>, &'static str) {
        match self {
            Self::Acquire(error) => (error, None, "acquire_socket"),
            Self::Replace { initial, error } => (error, Some(initial), "replace_socket"),
            Self::Retry { initial, error } => (error, Some(initial), "retry_send"),
        }
    }
}

#[track_caller]
fn report_send_response_error(
    context: &'static str,
    error: SendResponseError,
    client: SocketAddrV6,
    destination: SocketAddrV6,
) {
    let (error, initial, stage) = error.into_report_parts();
    let mut details = vec![
        ("client", client.to_string()),
        ("destination", destination.to_string()),
        ("reply_socket_stage", stage.to_owned()),
    ];
    if let Some(initial) = initial {
        let initial_errno = initial.raw_os_error();
        details.extend([
            ("initial_send_kind", format!("{:?}", initial.kind())),
            ("initial_send_error", initial.to_string()),
        ]);
        if let Some(errno) = initial_errno {
            details.push(("initial_send_errno", errno.to_string()));
        }
    }
    report::io_with_details(context, error, details);
}

fn create_reply_socket(key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_mark(key.mark)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(key.source))?;
    socket.set_nonblocking(true)?;
    Ok(Arc::new(TokioUdpSocket::from_std(socket.into())?))
}

fn enable_recv_hop_limit(socket: &UdpSocket) -> io::Result<()> {
    setsockopt(socket, sockopt::Ipv6RecvHopLimit, &true).map_err(io::Error::from)
}

fn set_upstream_hop_limit(socket: &TokioUdpSocket, hop_limit: u8) -> io::Result<()> {
    setsockopt(socket, sockopt::Ipv6Ttl, &c_int::from(hop_limit)).map_err(io::Error::from)
}

async fn send_udp_packet_too_big(
    config: &SessionConfig,
    gateway: Ipv6Addr,
    client: SocketAddrV6,
    destination: SocketAddrV6,
    mtu: u32,
    hop_limit: u8,
    payload: &[u8],
) {
    let context = icmp::UdpErrorContext {
        downstream: config.downstream.clone(),
        reply_mark: config.reply_mark,
        gateway,
        client,
        destination,
    };
    if let Err(e) = icmp::send_udp_packet_too_big(&context, mtu, hop_limit, payload).await {
        report::io_with_details(
            "nat66.udp_packet_too_big",
            e,
            [
                ("client", client.to_string()),
                ("destination", destination.to_string()),
            ],
        );
    }
}

fn upstream_mtu(socket: &TokioUdpSocket) -> io::Result<u32> {
    let mut mtu = 0 as c_int;
    let mut len = size_of::<c_int>() as socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            IPPROTO_IPV6,
            libc::IPV6_MTU,
            &mut mtu as *mut _ as *mut c_void,
            &mut len,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if mtu <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid ipv6 mtu",
        ));
    }
    Ok(mtu as u32)
}

fn udp_ipv6_packet_exceeds_mtu(payload: &[u8], mtu: u32) -> bool {
    (Ipv6Header::LEN + UdpHeader::LEN + payload.len()) as u64 > u64::from(mtu)
}

fn forward_udp_datagram(
    socket: &TokioUdpSocket,
    hop_limit: u8,
    payload: &[u8],
    client: SocketAddrV6,
    destination: SocketAddrV6,
    icmp_errors_registered: bool,
) -> UdpForwardResult {
    if let Err(e) = set_upstream_hop_limit(socket, hop_limit) {
        report::io_with_details(
            "nat66.udp_hop_limit",
            e,
            [
                ("client", client.to_string()),
                ("destination", destination.to_string()),
            ],
        );
        return UdpForwardResult::Dropped;
    }
    loop {
        match send_udp_payload(socket, payload) {
            Ok(_) => return UdpForwardResult::Sent,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Keep the shared UDP owner moving; local datagram queue pressure drops only this packet.
                return UdpForwardResult::Dropped;
            }
            Err(e) if e.raw_os_error() == Some(libc::EMSGSIZE) => match upstream_mtu(socket) {
                Ok(mtu) if udp_ipv6_packet_exceeds_mtu(payload, mtu) => {
                    return UdpForwardResult::PacketTooBig(mtu);
                }
                Ok(_) if icmp_errors_registered => return UdpForwardResult::Dropped,
                Ok(_) => {
                    report::io_with_details(
                        "nat66.udp_send",
                        e,
                        [
                            ("client", client.to_string()),
                            ("destination", destination.to_string()),
                        ],
                    );
                    return UdpForwardResult::Failed;
                }
                Err(mtu_error) => {
                    report::io_with_details(
                        "nat66.udp_mtu",
                        mtu_error,
                        [
                            ("client", client.to_string()),
                            ("destination", destination.to_string()),
                        ],
                    );
                    report::io_with_details(
                        "nat66.udp_send",
                        e,
                        [
                            ("client", client.to_string()),
                            ("destination", destination.to_string()),
                        ],
                    );
                    return UdpForwardResult::Failed;
                }
            },
            Err(e) if icmp_errors_registered && is_udp_icmp_error(&e) => {
                return UdpForwardResult::Dropped;
            }
            Err(e) => {
                report::io_with_details(
                    "nat66.udp_send",
                    e,
                    [
                        ("client", client.to_string()),
                        ("destination", destination.to_string()),
                    ],
                );
                return UdpForwardResult::Failed;
            }
        }
    }
}

fn send_udp_payload(socket: &TokioUdpSocket, payload: &[u8]) -> io::Result<()> {
    let written =
        send(socket.as_raw_fd(), payload, MsgFlags::MSG_DONTWAIT).map_err(io::Error::from)?;
    if written == payload.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short udp datagram write",
        ))
    }
}

fn recv_packet(
    fd: i32,
    buffer: &mut [u8],
) -> io::Result<(usize, SocketAddrV6, SocketAddrV6, Option<u8>)> {
    let mut control = cmsg_space!(libc::sockaddr_in6, libc::c_int);
    let mut iov = [IoSliceMut::new(buffer)];
    let message = recvmsg::<SockaddrIn6>(fd, &mut iov, Some(&mut control), MsgFlags::MSG_DONTWAIT)
        .map_err(io::Error::from)?;
    let source = message
        .address
        .map(SocketAddrV6::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing source address"))?;
    let mut destination = None;
    let mut hop_limit = None;
    for cmsg in message.cmsgs().map_err(io::Error::from)? {
        match cmsg {
            ControlMessageOwned::Ipv6OrigDstAddr(raw) => {
                destination = Some(SocketAddrV6::from(SockaddrIn6::from(raw)));
            }
            ControlMessageOwned::Ipv6HopLimit(value)
                if (0..=i32::from(u8::MAX)).contains(&value) =>
            {
                hop_limit = Some(value as u8);
            }
            _ => {}
        }
    }
    let destination = destination.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing original destination")
    })?;
    Ok((message.bytes, source, destination, hop_limit))
}
