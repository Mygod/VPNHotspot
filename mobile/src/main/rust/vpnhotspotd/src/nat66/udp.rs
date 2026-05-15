use std::collections::{hash_map::Entry, HashMap};
use std::future::pending;
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{SocketAddr, SocketAddrV6, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::{Duration, Instant};

use libc::{
    c_void, iovec, msghdr, recvmsg, sockaddr_in6, socklen_t, CMSG_DATA, CMSG_FIRSTHDR, CMSG_NXTHDR,
    IPPROTO_IPV6, IPV6_RECVORIGDSTADDR, MSG_DONTWAIT,
};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::dns::{resolve_or_error, DNS_PORT};
use crate::nat66::icmp;
use crate::report;
use crate::socket::socket_addr_v6_from_raw;
use crate::upstream::connect_udp;
use vpnhotspotd::shared::model::{select_network, SessionConfig};

const UDP_ASSOC_IDLE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AssociationKey {
    client: SocketAddrV6,
    destination: SocketAddrV6,
}

struct UdpAssociation {
    id: u64,
    datagrams: mpsc::UnboundedSender<Vec<u8>>,
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
    downstream: String,
    socket: TokioUdpSocket,
    datagrams: mpsc::UnboundedReceiver<Vec<u8>>,
    reply_sockets: Arc<ReplySocketPool>,
    reply_key: ReplySocketKey,
    reply_socket: Option<Arc<TokioUdpSocket>>,
    icmp_errors: bool,
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

#[derive(Default)]
struct ReplySocketState {
    retained_dns: Option<ReplySocketKey>,
    sockets: HashMap<ReplySocketKey, ReplySocketEntry>,
}

#[derive(Default)]
struct ReplySocketPool {
    state: StdMutex<ReplySocketState>,
}

impl ReplySocketPool {
    fn reserve_user(&self, key: ReplySocketKey) -> io::Result<()> {
        self.with_state(|state| match state.sockets.entry(key) {
            Entry::Occupied(mut entry) => {
                entry.get_mut().users += 1;
            }
            Entry::Vacant(entry) => {
                entry.insert(ReplySocketEntry {
                    socket: None,
                    users: 1,
                });
            }
        })
    }

    fn acquire_reserved(&self, key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.with_state(|state| {
            state
                .sockets
                .get(&key)
                .and_then(|entry| entry.socket.clone())
        })? {
            return Ok(socket);
        }
        let socket = create_reply_socket(key)?;
        self.with_state(|state| match state.sockets.entry(key) {
            Entry::Occupied(mut entry) => {
                if let Some(socket) = &entry.get().socket {
                    socket.clone()
                } else {
                    entry.get_mut().socket = Some(socket.clone());
                    socket
                }
            }
            Entry::Vacant(_) => socket,
        })
    }

    fn retain_dns(&self, key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.with_state(|state| {
            if state.retained_dns != Some(key) {
                if let Some(previous) = state.retained_dns.replace(key) {
                    if state
                        .sockets
                        .get(&previous)
                        .is_some_and(|entry| entry.users == 0)
                    {
                        state.sockets.remove(&previous);
                    }
                }
            }
            state
                .sockets
                .get(&key)
                .and_then(|entry| entry.socket.clone())
        })? {
            return Ok(socket);
        }
        let socket = create_reply_socket(key)?;
        self.with_state(|state| {
            if state.retained_dns != Some(key) {
                if let Some(previous) = state.retained_dns.replace(key) {
                    if state
                        .sockets
                        .get(&previous)
                        .is_some_and(|entry| entry.users == 0)
                    {
                        state.sockets.remove(&previous);
                    }
                }
            }
            match state.sockets.entry(key) {
                Entry::Occupied(mut entry) => {
                    if let Some(socket) = &entry.get().socket {
                        socket.clone()
                    } else {
                        entry.get_mut().socket = Some(socket.clone());
                        socket
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(ReplySocketEntry {
                        socket: Some(socket.clone()),
                        users: 0,
                    });
                    socket
                }
            }
        })
    }

    fn release_user(&self, key: ReplySocketKey) -> io::Result<()> {
        self.with_state(|state| -> io::Result<()> {
            let entry = state
                .sockets
                .get_mut(&key)
                .ok_or_else(|| io::Error::other("reply socket reservation missing"))?;
            if entry.users == 0 {
                return Err(io::Error::other("reply socket users underflow"));
            }
            entry.users -= 1;
            let remove = entry.users == 0 && state.retained_dns != Some(key);
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
        self.with_state(|state| {
            let retained = state.retained_dns == Some(key);
            match state.sockets.entry(key) {
                Entry::Occupied(mut entry) => {
                    if entry
                        .get()
                        .socket
                        .as_ref()
                        .is_some_and(|socket| Arc::ptr_eq(socket, previous))
                    {
                        entry.get_mut().socket = Some(socket.clone());
                    }
                    entry.get().socket.clone().unwrap_or(socket)
                }
                Entry::Vacant(entry) if retained => {
                    entry.insert(ReplySocketEntry {
                        socket: Some(socket.clone()),
                        users: 0,
                    });
                    socket
                }
                Entry::Vacant(_) => socket,
            }
        })
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

pub(crate) fn spawn_loop(
    listener: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = AsyncFd::new(listener)?;
    spawn(async move {
        let listener_fd = listener.get_ref().as_raw_fd();
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
                let active = now.duration_since(association.last_active) < UDP_ASSOC_IDLE;
                if !active {
                    association.stop.cancel();
                }
                active
            });
            let next_expiry = associations
                .values()
                .map(|association| association.last_active + UDP_ASSOC_IDLE)
                .min();

            select! {
                _ = stop.cancelled() => break,
                _ = async {
                    if let Some(deadline) = next_expiry {
                        sleep_until(TokioInstant::from_std(deadline)).await;
                    } else {
                        pending::<()>().await;
                    }
                } => {}
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
                            Ok((size, client, destination)) => {
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
                                if destination.ip().is_multicast()
                                    || destination.ip().is_unicast_link_local()
                                    || destination.ip().is_loopback()
                                    || destination.ip().is_unspecified()
                                {
                                    continue;
                                }
                                let network = match select_network(&snapshot, *destination.ip()) {
                                    Some(network) => network,
                                    None => continue,
                                };
                                let key = AssociationKey { client, destination };
                                if let Some(datagrams) = associations.get_mut(&key).map(|association| {
                                    association.last_active = activity;
                                    association.datagrams.clone()
                                }) {
                                    if datagrams.send(buffer[..size].to_vec()).is_err() {
                                        if let Some(association) = associations.remove(&key) {
                                            association.stop.cancel();
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
                                let icmp_errors = match icmp::enable_udp_error_queue(&upstream) {
                                    Ok(()) => true,
                                    Err(e) => {
                                        report::io_with_details(
                                            "nat66.udp_error_queue",
                                            e,
                                            [
                                                ("client", client.to_string()),
                                                ("destination", destination.to_string()),
                                            ],
                                        );
                                        false
                                    }
                                };
                                let reply_key = ReplySocketKey {
                                    source: destination,
                                    mark: snapshot.reply_mark,
                                };
                                if let Err(e) = reply_sockets.reserve_user(reply_key) {
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
                                let association_stop = stop.child_token();
                                let association_id = next_association_id;
                                next_association_id = next_association_id.wrapping_add(1);
                                let (datagram_tx, datagram_rx) = mpsc::unbounded_channel();
                                spawn(AssociationTask {
                                    key,
                                    id: association_id,
                                    downstream: snapshot.downstream.clone(),
                                    socket: upstream,
                                    datagrams: datagram_rx,
                                    reply_sockets: reply_sockets.clone(),
                                    reply_key,
                                    reply_socket: None,
                                    icmp_errors,
                                    stop: association_stop.clone(),
                                    association_event_tx: association_event_tx.clone(),
                                }.run());
                                associations.insert(
                                    key,
                                    UdpAssociation {
                                        id: association_id,
                                        datagrams: datagram_tx.clone(),
                                        last_active: activity,
                                        stop: association_stop,
                                    },
                                );
                                if datagram_tx.send(buffer[..size].to_vec()).is_err() {
                                    if let Some(association) = associations.remove(&key) {
                                        association.stop.cancel();
                                    }
                                }
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
                datagram = self.datagrams.recv() => match datagram {
                    Some(datagram) => {
                        if let Err(e) = self.socket.send(&datagram).await {
                            if !self.drain_error_queue().await {
                                report::io_with_details(
                                    "nat66.udp_send",
                                    e,
                                    [
                                        ("client", self.key.client.to_string()),
                                        ("destination", self.key.destination.to_string()),
                                    ],
                                );
                                break;
                            }
                        } else {
                            self.report_active();
                        }
                    }
                    None => break,
                },
                result = self.socket.recv(&mut buffer) => match result {
                    Ok(size) => {
                        if let Err(e) = send_response(
                            &self.reply_sockets,
                            self.reply_key,
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
                    Err(e) => {
                        if !self.drain_error_queue().await {
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
        }
        let cancelled = self.stop.is_cancelled();
        self.stop.cancel();
        if let Err(e) = self.reply_sockets.release_user(self.reply_key) {
            report::io("nat66.udp_reply_release", e);
        }
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

    async fn drain_error_queue(&self) -> bool {
        if !self.icmp_errors {
            return false;
        }
        let context = icmp::UdpErrorContext {
            downstream: self.downstream.clone(),
            reply_mark: self.reply_key.mark,
            client: self.key.client,
            destination: self.key.destination,
        };
        match icmp::drain_udp_error_queue(&self.socket, &context).await {
            Ok(count) => {
                if count > 0 {
                    self.report_active();
                    true
                } else {
                    false
                }
            }
            Err(e) => {
                report::io_with_details(
                    "nat66.udp_error_queue_recv",
                    e,
                    [
                        ("client", self.key.client.to_string()),
                        ("destination", self.key.destination.to_string()),
                    ],
                );
                false
            }
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

fn recv_packet(fd: i32, buffer: &mut [u8]) -> io::Result<(usize, SocketAddrV6, SocketAddrV6)> {
    let mut source: sockaddr_in6 = unsafe { zeroed() };
    let source_len = size_of::<sockaddr_in6>() as socklen_t;
    let mut control = [0u8; 128];
    let mut iov = iovec {
        iov_base: buffer.as_mut_ptr() as *mut c_void,
        iov_len: buffer.len(),
    };
    let mut message = msghdr {
        msg_name: &mut source as *mut _ as *mut c_void,
        msg_namelen: source_len,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr() as *mut c_void,
        msg_controllen: control.len(),
        msg_flags: 0,
    };
    let size = unsafe { recvmsg(fd, &mut message, MSG_DONTWAIT) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let source = socket_addr_v6_from_raw(source);
    let mut destination = None;
    let mut current = unsafe { CMSG_FIRSTHDR(&message) };
    while !current.is_null() {
        unsafe {
            if (*current).cmsg_level == IPPROTO_IPV6 && (*current).cmsg_type == IPV6_RECVORIGDSTADDR
            {
                let raw = CMSG_DATA(current) as *const sockaddr_in6;
                destination = Some(socket_addr_v6_from_raw(*raw));
                break;
            }
            current = CMSG_NXTHDR(&message, current);
        }
    }
    let destination = destination.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing original destination")
    })?;
    Ok((size as usize, source, destination))
}
