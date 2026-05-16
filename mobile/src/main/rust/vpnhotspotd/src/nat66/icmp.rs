use std::collections::HashMap;
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, Weak};
use std::time::Instant;

use etherparse::{Icmpv6Header, Icmpv6Type, IpNumber, Ipv6Header, Ipv6Slice};
use libc::{
    c_int, c_void, iovec, msghdr, recvmsg, sa_family_t, sockaddr_in6, socklen_t, CMSG_DATA,
    CMSG_FIRSTHDR, CMSG_NXTHDR, IPPROTO_IPV6, MSG_DONTWAIT,
};
use nfq::{Queue, Verdict};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::socket::{await_writable, set_int_sockopt, set_nonblocking, socket_addr_v6_from_raw};
use crate::upstream::set_socket_network;
use crate::{netlink, report};
use vpnhotspotd::shared::icmp_nat::{
    classify_nat66_destination, downstream_icmp_error_source, icmpv6_error_bytes, nat66_hop_limit,
    EchoAllocation, EchoEntry, EchoMap, Nat66Destination, Nat66HopLimit, ICMPV6_TIME_EXCEEDED,
};
use vpnhotspotd::shared::icmp_wire::{
    build_echo_packet_with_checksum, build_echo_packet_zero_checksum, build_icmp_error_packet,
    build_icmp_quote, build_udp_quote, build_udp_quote_with_hop_limit, ICMPV6_ECHO_REPLY,
    ICMPV6_ECHO_REQUEST,
};
use vpnhotspotd::shared::model::{select_network, Network, SessionConfig, DAEMON_ICMP_NFQUEUE_NUM};

const IPV6_RECVERR_OPT: c_int = 25;
const MSG_ERRQUEUE_OPT: c_int = 0x2000;
const SO_EE_ORIGIN_ICMP6: u8 = 3;
const NONLOCAL_PROBE_SOURCE: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);

#[repr(C)]
#[derive(Clone, Copy)]
struct SockExtendedErr {
    ee_errno: u32,
    ee_origin: u8,
    ee_type: u8,
    ee_code: u8,
    ee_pad: u8,
    ee_info: u32,
    ee_data: u32,
}

#[derive(Default)]
struct EchoState {
    inner: StdMutex<EchoStateInner>,
}

#[derive(Default)]
struct EchoStateInner {
    map: EchoMap,
    upstream: HashMap<Network, UpstreamSocket>,
}

#[derive(Clone)]
struct UpstreamSocket {
    socket: Arc<AsyncFd<Socket>>,
    stop: CancellationToken,
    error_queue: bool,
}

enum UpstreamPrune {
    Removed,
    StillActive,
}

#[derive(Clone)]
pub(crate) struct Dispatcher {
    inner: Arc<DispatcherInner>,
}

struct DispatcherInner {
    state: Arc<EchoState>,
    registrations: Arc<StdMutex<HashMap<u32, Weak<IcmpSession>>>>,
    next_session_key: AtomicU64,
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl Drop for DispatcherInner {
    fn drop(&mut self) {
        if let Err(e) = self.state.cancel_all_upstream() {
            report::io("nat66.icmp_upstream_cancel", e);
        }
        let Ok(mut task) = self.task.lock() else {
            return;
        };
        if let Some(task) = task.take() {
            task.abort();
        }
    }
}

pub(crate) struct Registration {
    input_interface: u32,
    session: Arc<IcmpSession>,
    registrations: Arc<StdMutex<HashMap<u32, Weak<IcmpSession>>>>,
}

struct IcmpSession {
    session_key: u64,
    state: Arc<EchoState>,
    config: Arc<Mutex<SessionConfig>>,
}

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

struct ReceivedIcmpPacket<'a> {
    source: SocketAddrV6,
    payload: &'a [u8],
}

struct ErrorQueueMessage {
    error: SockExtendedErr,
    offender: Option<SocketAddrV6>,
    destination: Option<SocketAddrV6>,
    payload: Vec<u8>,
}

struct EchoErrorProbe<'a> {
    destination: Ipv6Addr,
    id: u16,
    seq: u16,
    hop_limit: Option<u8>,
    payload: &'a [u8],
}

pub(crate) struct UdpErrorContext {
    pub(crate) downstream: String,
    pub(crate) reply_mark: u32,
    pub(crate) gateway: Ipv6Addr,
    pub(crate) client: SocketAddrV6,
    pub(crate) destination: SocketAddrV6,
}

impl EchoState {
    fn allocate_echo(
        state: &Arc<Self>,
        allocation: EchoAllocation,
    ) -> io::Result<(u16, u16, UpstreamSocket)> {
        let network = allocation.network;
        let destination = allocation.destination;
        let (id, seq, socket) = {
            let mut inner = state.lock_inner()?;
            let (id, seq) = inner
                .map
                .allocate(Instant::now(), super::IDLE_TIMEOUT, allocation)?;
            let socket = inner.upstream.get(&network).cloned();
            (id, seq, socket)
        };
        if let Some(socket) = socket {
            return Ok((id, seq, socket));
        }
        match Self::ensure_upstream_socket(state, network) {
            Ok(socket) => Ok((id, seq, socket)),
            Err(e) => {
                if let Err(remove_error) = state.remove_allocation(network, destination, id, seq) {
                    report::io("nat66.icmp_echo_remove_failed_send", remove_error);
                }
                Err(e)
            }
        }
    }

    fn restore(
        &self,
        network: Network,
        source: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> io::Result<Option<EchoEntry>> {
        let (entry, upstream) = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            let entry = inner
                .map
                .restore(now, super::IDLE_TIMEOUT, network, source, id, seq);
            let upstream = Self::remove_idle_upstream_locked(&mut inner, network, now);
            (entry, upstream)
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Ok(entry)
    }

    fn remove_allocation(
        &self,
        network: Network,
        destination: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> io::Result<()> {
        let upstream = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            inner
                .map
                .remove(now, super::IDLE_TIMEOUT, network, destination, id, seq);
            Self::remove_idle_upstream_locked(&mut inner, network, now)
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Ok(())
    }

    fn has_allocation(
        &self,
        network: Network,
        destination: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> io::Result<bool> {
        let mut inner = self.lock_inner()?;
        Ok(inner.map.contains(
            Instant::now(),
            super::IDLE_TIMEOUT,
            network,
            destination,
            id,
            seq,
        ))
    }

    fn remove_session(&self, session_key: u64) -> io::Result<()> {
        let upstreams = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            inner
                .map
                .remove_session(now, super::IDLE_TIMEOUT, session_key);
            Self::remove_idle_upstreams_locked(&mut inner, now)
        };
        for upstream in upstreams {
            upstream.stop.cancel();
        }
        Ok(())
    }

    fn upstream_idle_deadline(&self, network: Network) -> io::Result<Option<Instant>> {
        let mut inner = self.lock_inner()?;
        Ok(inner
            .map
            .network_idle_deadline(Instant::now(), network, super::IDLE_TIMEOUT))
    }

    fn prune_idle_upstream(&self, network: Network) -> io::Result<UpstreamPrune> {
        let (result, upstream) = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            if inner
                .map
                .has_network_entries(now, super::IDLE_TIMEOUT, network)
            {
                (UpstreamPrune::StillActive, None)
            } else {
                (UpstreamPrune::Removed, inner.upstream.remove(&network))
            }
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Ok(result)
    }

    fn ensure_upstream_socket(state: &Arc<Self>, network: Network) -> io::Result<UpstreamSocket> {
        if let Some(upstream) = state.lock_inner()?.upstream.get(&network).cloned() {
            return Ok(upstream);
        }
        let socket = Arc::new(AsyncFd::new(create_upstream_socket(network)?)?);
        let stop = CancellationToken::new();
        let error_queue = match enable_ipv6_error_queue(socket.get_ref().as_raw_fd()) {
            Ok(()) => true,
            Err(e) => {
                report::io_with_details(
                    "nat66.icmp_echo_error_queue",
                    e,
                    [("network", network.to_string())],
                );
                false
            }
        };
        let upstream = UpstreamSocket {
            socket: socket.clone(),
            stop: stop.clone(),
            error_queue,
        };
        let mut start = false;
        let active = {
            let mut inner = state.lock_inner()?;
            if let Some(upstream) = inner.upstream.get(&network) {
                upstream.clone()
            } else {
                if !inner
                    .map
                    .has_network_entries(Instant::now(), super::IDLE_TIMEOUT, network)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "echo allocation removed",
                    ));
                }
                start = true;
                inner.upstream.insert(network, upstream.clone());
                upstream
            }
        };
        if start {
            spawn_upstream_loop(
                network,
                active.socket.clone(),
                active.error_queue,
                state.clone(),
                stop,
            );
        }
        Ok(active)
    }

    fn remove_idle_upstream_locked(
        inner: &mut EchoStateInner,
        network: Network,
        now: Instant,
    ) -> Option<UpstreamSocket> {
        if inner
            .map
            .has_network_entries(now, super::IDLE_TIMEOUT, network)
        {
            None
        } else {
            inner.upstream.remove(&network)
        }
    }

    fn remove_idle_upstreams_locked(
        inner: &mut EchoStateInner,
        now: Instant,
    ) -> Vec<UpstreamSocket> {
        let networks: Vec<_> = inner.upstream.keys().copied().collect();
        networks
            .into_iter()
            .filter_map(|network| Self::remove_idle_upstream_locked(inner, network, now))
            .collect()
    }

    fn cancel_all_upstream(&self) -> io::Result<()> {
        for upstream in self
            .lock_inner()?
            .upstream
            .drain()
            .map(|(_, upstream)| upstream)
        {
            upstream.stop.cancel();
        }
        Ok(())
    }

    fn remove_upstream_socket(
        &self,
        network: Network,
        socket: &Arc<AsyncFd<Socket>>,
    ) -> io::Result<()> {
        let mut inner = self.lock_inner()?;
        if inner
            .upstream
            .get(&network)
            .is_some_and(|upstream| Arc::ptr_eq(&upstream.socket, socket))
        {
            inner.upstream.remove(&network);
        }
        Ok(())
    }

    fn lock_inner(&self) -> io::Result<MutexGuard<'_, EchoStateInner>> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("echo state poisoned"))
    }
}

impl Dispatcher {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(DispatcherInner {
                state: Arc::new(EchoState::default()),
                registrations: Arc::new(StdMutex::new(HashMap::new())),
                next_session_key: AtomicU64::new(1),
                task: StdMutex::new(None),
            }),
        }
    }

    pub(crate) async fn register(
        &self,
        initial: &SessionConfig,
        config: Arc<Mutex<SessionConfig>>,
        netlink: &netlink::Handle,
    ) -> io::Result<Registration> {
        if initial.ipv6_nat.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing ipv6 NAT config",
            ));
        }
        let input_interface = netlink::link_index(netlink, &initial.downstream).await?;
        drop(create_downstream_send_socket(
            &initial.downstream,
            initial.reply_mark,
            NONLOCAL_PROBE_SOURCE,
        )?);
        self.ensure_started()?;
        let session_key = self.inner.next_session_key.fetch_add(1, Ordering::Relaxed);
        let session = Arc::new(IcmpSession {
            session_key,
            state: self.inner.state.clone(),
            config,
        });
        self.inner
            .registrations
            .lock()
            .map_err(|_| io::Error::other("icmp registrations state poisoned"))?
            .insert(input_interface, Arc::downgrade(&session));
        Ok(Registration {
            input_interface,
            session,
            registrations: self.inner.registrations.clone(),
        })
    }

    fn ensure_started(&self) -> io::Result<()> {
        let mut task = self
            .inner
            .task
            .lock()
            .map_err(|_| io::Error::other("icmp queue task state poisoned"))?;
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let queue = AsyncFd::new(create_downstream_queue()?)?;
        *task = Some(spawn(run_downstream_queue(
            queue,
            self.inner.registrations.clone(),
            self.inner.state.clone(),
        )));
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        match self.registrations.lock() {
            Ok(mut registrations) => {
                let remove_registration = match registrations.get(&self.input_interface) {
                    Some(session) => match session.upgrade() {
                        Some(session) => Arc::ptr_eq(&session, &self.session),
                        None => true,
                    },
                    None => false,
                };
                if remove_registration {
                    registrations.remove(&self.input_interface);
                }
            }
            Err(_) => report::message(
                "nat66.icmp_remove_registration",
                "icmp registrations state poisoned",
                "PoisonError",
            ),
        }
    }
}

impl Drop for IcmpSession {
    fn drop(&mut self) {
        if let Err(e) = self.state.remove_session(self.session_key) {
            report::io("nat66.icmp_remove_session", e);
        }
    }
}

async fn run_downstream_queue(
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

pub(crate) fn enable_udp_error_queue(socket: &TokioUdpSocket) -> io::Result<()> {
    enable_ipv6_error_queue(socket.as_raw_fd())
}

pub(crate) async fn drain_udp_error_queue(
    socket: &TokioUdpSocket,
    context: &UdpErrorContext,
) -> io::Result<usize> {
    let mut translated = 0;
    loop {
        let message = match recv_error_queue(socket.as_raw_fd()) {
            Ok(message) => message,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(translated),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if message.error.ee_origin != SO_EE_ORIGIN_ICMP6 {
            continue;
        }
        let Some((icmp_type, bytes5to8)) = error_type_bytes(&message.error) else {
            continue;
        };
        let Some(offender) = message.offender else {
            report::stdout!(
                "udp icmp error dropped: missing offender client={} destination={}",
                context.client,
                context.destination
            );
            continue;
        };
        let source = downstream_icmp_error_source(*offender.ip(), context.gateway);
        let quote = match build_udp_quote(context.client, context.destination, &message.payload) {
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
                continue;
            }
        };
        let packet = match build_icmp_error_packet(
            icmp_type,
            message.error.ee_code,
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
                continue;
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
            continue;
        }
        translated += 1;
    }
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
    let request = build_echo_packet_zero_checksum(ICMPV6_ECHO_REQUEST, id, seq, payload);
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
                    match drain_echo_error_queue(
                        socket.socket.get_ref().as_raw_fd(),
                        network,
                        &state,
                    )
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

fn spawn_upstream_loop(
    network: Network,
    socket: Arc<AsyncFd<Socket>>,
    error_queue: bool,
    state: Arc<EchoState>,
    stop: CancellationToken,
) {
    spawn(async move {
        let mut buffer = vec![0u8; 65535];
        loop {
            let deadline = match state.upstream_idle_deadline(network) {
                Ok(Some(deadline)) => deadline,
                Ok(None) => match state.prune_idle_upstream(network) {
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
                _ = super::sleep_until_deadline(Some(deadline)) => {
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
                    match drain_echo_error_queue(socket.get_ref().as_raw_fd(), network, &state)
                        .await
                    {
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
                match recv_raw_icmp_packet(socket.get_ref().as_raw_fd(), &mut buffer) {
                    Ok(Some(packet)) => handle_upstream_echo_reply(network, &state, packet).await,
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

async fn handle_upstream_echo_reply(
    network: Network,
    state: &EchoState,
    packet: ReceivedIcmpPacket<'_>,
) {
    let Ok((header, payload)) = Icmpv6Header::from_slice(packet.payload) else {
        return;
    };
    let Icmpv6Type::EchoReply(echo) = header.icmp_type else {
        return;
    };
    let entry = match state.restore(network, *packet.source.ip(), echo.id, echo.seq) {
        Ok(Some(entry)) => entry,
        Ok(None) => return,
        Err(e) => {
            report::io("nat66.icmp_echo_restore", e);
            return;
        }
    };
    let reply = match build_echo_packet_with_checksum(
        ICMPV6_ECHO_REPLY,
        *packet.source.ip(),
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
        *packet.source.ip(),
        entry.client,
        &reply,
    )
    .await
    {
        report::stdout!(
            "icmp echo reply dropped: source={} client={}: {}",
            packet.source.ip(),
            entry.client,
            e
        );
    }
}

async fn drain_echo_error_queue(
    fd: RawFd,
    network: Network,
    state: &EchoState,
) -> io::Result<usize> {
    let mut drained = 0;
    loop {
        let message = match recv_error_queue(fd) {
            Ok(message) => message,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(drained),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        drained += 1;
        if message.error.ee_origin != SO_EE_ORIGIN_ICMP6 {
            continue;
        }
        let Some((icmp_type, bytes5to8)) = error_type_bytes(&message.error) else {
            continue;
        };
        let Some(offender) = message.offender else {
            continue;
        };
        let Some(probe) = error_queue_echo_probe(&message) else {
            continue;
        };
        let Some(entry) = state.restore(network, probe.destination, probe.id, probe.seq)? else {
            continue;
        };
        let source = downstream_icmp_error_source(*offender.ip(), entry.gateway);
        let request = match build_echo_packet_with_checksum(
            ICMPV6_ECHO_REQUEST,
            *entry.client.ip(),
            probe.destination,
            entry.original_id,
            entry.original_seq,
            probe.payload,
        ) {
            Ok(request) => request,
            Err(e) => {
                report::io_with_details(
                    "nat66.icmp_echo_error_quote_request",
                    e,
                    [
                        ("client", entry.client.to_string()),
                        ("destination", probe.destination.to_string()),
                    ],
                );
                continue;
            }
        };
        let quote = match build_icmp_quote(
            *entry.client.ip(),
            probe.destination,
            probe.hop_limit.unwrap_or(entry.upstream_hop_limit),
            &request,
        ) {
            Ok(quote) => quote,
            Err(e) => {
                report::io_with_details(
                    "nat66.icmp_echo_error_quote",
                    e,
                    [
                        ("client", entry.client.to_string()),
                        ("destination", probe.destination.to_string()),
                    ],
                );
                continue;
            }
        };
        let packet = match build_icmp_error_packet(
            icmp_type,
            message.error.ee_code,
            bytes5to8,
            source,
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
                        ("destination", probe.destination.to_string()),
                    ],
                );
                continue;
            }
        };
        if let Err(e) = send_downstream_icmp(
            &entry.downstream,
            entry.reply_mark,
            source,
            entry.client,
            &packet,
        )
        .await
        {
            report::stdout!(
                "icmp echo error dropped: source={} client={}: {}",
                source,
                entry.client,
                e
            );
            continue;
        }
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

fn quoted_echo_probe(payload: &[u8]) -> Option<EchoErrorProbe<'_>> {
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

fn recv_raw_icmp_packet<'a>(
    fd: RawFd,
    buffer: &'a mut [u8],
) -> io::Result<Option<ReceivedIcmpPacket<'a>>> {
    let mut source: sockaddr_in6 = unsafe { zeroed() };
    let source_len = size_of::<sockaddr_in6>() as socklen_t;
    let mut iov = iovec {
        iov_base: buffer.as_mut_ptr() as *mut c_void,
        iov_len: buffer.len(),
    };
    let mut message = msghdr {
        msg_name: &mut source as *mut _ as *mut c_void,
        msg_namelen: source_len,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: std::ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    let size = unsafe { recvmsg(fd, &mut message, MSG_DONTWAIT) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    if size == 0 || source.sin6_family != libc::AF_INET6 as sa_family_t {
        return Ok(None);
    }
    Ok(Some(ReceivedIcmpPacket {
        source: socket_addr_v6_from_raw(source),
        payload: &buffer[..size as usize],
    }))
}

fn recv_error_queue(fd: RawFd) -> io::Result<ErrorQueueMessage> {
    let mut name: sockaddr_in6 = unsafe { zeroed() };
    let name_len = size_of::<sockaddr_in6>() as socklen_t;
    let mut buffer = vec![0u8; 2048];
    let mut control = [0u8; 512];
    let mut iov = iovec {
        iov_base: buffer.as_mut_ptr() as *mut c_void,
        iov_len: buffer.len(),
    };
    let mut message = msghdr {
        msg_name: &mut name as *mut _ as *mut c_void,
        msg_namelen: name_len,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr() as *mut c_void,
        msg_controllen: control.len(),
        msg_flags: 0,
    };
    let size = unsafe { recvmsg(fd, &mut message, MSG_DONTWAIT | MSG_ERRQUEUE_OPT) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let destination = if name.sin6_family == libc::AF_INET6 as sa_family_t {
        Some(socket_addr_v6_from_raw(name))
    } else {
        None
    };
    let mut error = None;
    let mut offender = None;
    let mut current = unsafe { CMSG_FIRSTHDR(&message) };
    while !current.is_null() {
        unsafe {
            if (*current).cmsg_level == IPPROTO_IPV6 && (*current).cmsg_type == IPV6_RECVERR_OPT {
                let data = CMSG_DATA(current) as *const u8;
                error = Some((data as *const SockExtendedErr).read_unaligned());
                let data_len = (*current).cmsg_len as usize - (data as usize - current as usize);
                if data_len >= size_of::<SockExtendedErr>() + size_of::<sockaddr_in6>() {
                    let raw = (data.add(size_of::<SockExtendedErr>()) as *const sockaddr_in6)
                        .read_unaligned();
                    if raw.sin6_family == libc::AF_INET6 as sa_family_t {
                        offender = Some(socket_addr_v6_from_raw(raw));
                    }
                }
            }
            current = CMSG_NXTHDR(&message, current);
        }
    }
    let error =
        error.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 error"))?;
    buffer.truncate(size as usize);
    Ok(ErrorQueueMessage {
        error,
        offender,
        destination,
        payload: buffer,
    })
}

fn create_downstream_queue() -> io::Result<Queue> {
    let mut queue = Queue::open()?;
    queue.bind(DAEMON_ICMP_NFQUEUE_NUM)?;
    queue.set_nonblocking(true);
    set_nonblocking(queue.as_raw_fd())?;
    Ok(queue)
}

fn create_downstream_send_socket(
    interface: &str,
    mark: u32,
    source: Ipv6Addr,
) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    socket.bind_device(Some(interface.as_bytes()))?;
    socket.set_mark(mark)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(SocketAddrV6::new(source, 0, 0, 0)))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn create_upstream_socket(network: Network) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn enable_ipv6_error_queue(fd: RawFd) -> io::Result<()> {
    set_int_sockopt(fd, IPPROTO_IPV6, IPV6_RECVERR_OPT, 1)
}

fn set_upstream_echo_hop_limit(socket: &AsyncFd<Socket>, hop_limit: u8) -> io::Result<()> {
    set_int_sockopt(
        socket.get_ref().as_raw_fd(),
        IPPROTO_IPV6,
        libc::IPV6_UNICAST_HOPS,
        c_int::from(hop_limit),
    )
}

async fn send_upstream_echo(
    socket: &AsyncFd<Socket>,
    destination: SocketAddrV6,
    packet: &[u8],
) -> io::Result<()> {
    sendto_all_async(socket, packet, SockAddr::from(destination)).await
}

async fn send_downstream_icmp(
    interface: &str,
    mark: u32,
    source: Ipv6Addr,
    target: SocketAddrV6,
    packet: &[u8],
) -> io::Result<()> {
    let socket = create_downstream_send_socket(interface, mark, source)?;
    sendto_all(
        &socket,
        packet,
        SockAddr::from(SocketAddrV6::new(
            *target.ip(),
            0,
            target.flowinfo(),
            target.scope_id(),
        )),
    )
    .await
}

async fn sendto_all(socket: &Socket, mut packet: &[u8], address: SockAddr) -> io::Result<()> {
    while !packet.is_empty() {
        let written = match socket.send_to(packet, &address) {
            Ok(written) => written,
            Err(error) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    await_writable(socket.as_fd()).await?;
                    continue;
                }
                return Err(error);
            }
        };
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "socket write failed",
            ));
        }
        packet = &packet[written..];
    }
    Ok(())
}

async fn sendto_all_async(
    socket: &AsyncFd<Socket>,
    mut packet: &[u8],
    address: SockAddr,
) -> io::Result<()> {
    while !packet.is_empty() {
        let mut ready = socket.writable().await?;
        let written = match socket.get_ref().send_to(packet, &address) {
            Ok(written) => written,
            Err(error) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    ready.clear_ready();
                    continue;
                }
                return Err(error);
            }
        };
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "socket write failed",
            ));
        }
        packet = &packet[written..];
    }
    Ok(())
}

fn error_type_bytes(error: &SockExtendedErr) -> Option<(u8, [u8; 4])> {
    icmpv6_error_bytes(error.ee_type, error.ee_info)
}
