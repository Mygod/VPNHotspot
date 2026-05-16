use std::collections::HashMap;
use std::io::{self, IoSliceMut};
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, Weak};
use std::time::Instant;

use etherparse::{Icmpv6Header, Icmpv6Type, IpNumber, Ipv6Header, Ipv6Slice, UdpHeader};
use libc::c_int;
use nfq::{Queue, Verdict};
use nix::cmsg_space;
use nix::sys::socket::{recvmsg, setsockopt, sockopt, ControlMessageOwned, MsgFlags, SockaddrIn6};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::socket::{await_writable, set_nonblocking};
use crate::upstream::set_socket_network;
use crate::{netlink, report};
use vpnhotspotd::shared::icmp_nat::{
    classify_nat66_destination, downstream_icmp_error_source, icmpv6_error_bytes, nat66_hop_limit,
    EchoAllocation, EchoEntry, EchoMap, Nat66Destination, Nat66HopLimit, ICMPV6_PACKET_TOO_BIG,
    ICMPV6_TIME_EXCEEDED,
};
use vpnhotspotd::shared::icmp_wire::{
    build_echo_packet_with_checksum, build_echo_packet_zero_checksum, build_icmp_error_packet,
    build_icmp_quote, build_translated_udp_quote, build_udp_quote_with_hop_limit, UdpQuoteMetadata,
    ICMPV6_ECHO_REPLY, ICMPV6_ECHO_REQUEST,
};
use vpnhotspotd::shared::model::{select_network, Network, SessionConfig, DAEMON_ICMP_NFQUEUE_NUM};

const SO_EE_ORIGIN_LOCAL: u8 = 1;
const SO_EE_ORIGIN_ICMP6: u8 = 3;
const NONLOCAL_PROBE_SOURCE: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);

#[derive(Default)]
struct EchoState {
    inner: StdMutex<EchoStateInner>,
}

#[derive(Default)]
struct EchoStateInner {
    map: EchoMap,
    upstream: HashMap<Network, UpstreamSocket>,
    udp_errors: HashMap<UdpErrorKey, UdpErrorContext>,
}

#[derive(Clone)]
struct UpstreamSocket {
    socket: Arc<AsyncFd<Socket>>,
    stop: CancellationToken,
    changed: Arc<Notify>,
    error_queue: bool,
}

enum UpstreamPrune {
    Removed,
    StillActive,
}

enum UpstreamActivity {
    Idle,
    Active(Option<Instant>),
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

impl Drop for UdpErrorRegistration {
    fn drop(&mut self) {
        if let Err(e) = self.state.unregister_udp_error(self.key) {
            report::io("nat66.icmp_udp_unregister", e);
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
    error: libc::sock_extended_err,
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

struct UdpErrorProbe<'a> {
    quote: UdpQuoteMetadata,
    payload: &'a [u8],
}

struct EchoErrorResponse {
    source: Ipv6Addr,
    icmp_type: u8,
    code: u8,
    bytes5to8: [u8; 4],
    destination: Ipv6Addr,
    hop_limit: u8,
}

enum QueuedEchoError {
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

#[derive(Clone)]
pub(crate) struct UdpErrorContext {
    pub(crate) downstream: String,
    pub(crate) reply_mark: u32,
    pub(crate) gateway: Ipv6Addr,
    pub(crate) client: SocketAddrV6,
    pub(crate) destination: SocketAddrV6,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct UdpErrorKey {
    network: Network,
    upstream: SocketAddrV6,
    destination: SocketAddrV6,
}

pub(crate) struct UdpErrorRegistration {
    key: UdpErrorKey,
    state: Arc<EchoState>,
}

impl EchoState {
    fn allocate_echo(
        state: &Arc<Self>,
        allocation: EchoAllocation,
    ) -> io::Result<(u16, u16, UpstreamSocket)> {
        let network = allocation.network;
        let destination = allocation.destination;
        let (id, seq, socket, notify) = {
            let mut inner = state.lock_inner()?;
            let now = Instant::now();
            let old_deadline = inner
                .map
                .network_idle_deadline(now, network, super::IDLE_TIMEOUT);
            let (id, seq) = inner.map.allocate(now, super::IDLE_TIMEOUT, allocation)?;
            let socket = inner.upstream.get(&network).cloned();
            let notify = if old_deadline.is_none() {
                Self::upstream_changed_locked(&inner, network)
            } else {
                None
            };
            (id, seq, socket, notify)
        };
        Self::notify_upstream_changed(notify);
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

    fn register_udp_error(
        state: &Arc<Self>,
        network: Network,
        upstream: SocketAddrV6,
        destination: SocketAddrV6,
        context: UdpErrorContext,
    ) -> io::Result<UdpErrorRegistration> {
        let key = UdpErrorKey {
            network,
            upstream: normalize_udp_error_addr(upstream),
            destination: normalize_udp_error_addr(destination),
        };
        state.lock_inner()?.udp_errors.insert(key, context);
        if let Err(e) = Self::ensure_upstream_socket(state, network) {
            state.lock_inner()?.udp_errors.remove(&key);
            return Err(e);
        }
        Ok(UdpErrorRegistration {
            key,
            state: state.clone(),
        })
    }

    fn unregister_udp_error(&self, key: UdpErrorKey) -> io::Result<()> {
        let (upstream, notify) = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            let removed = inner.udp_errors.remove(&key).is_some();
            let upstream = Self::remove_idle_upstream_locked(&mut inner, key.network, now);
            let notify = if removed
                && upstream.is_none()
                && !Self::has_udp_error_entries_locked(&inner, key.network)
                && inner
                    .map
                    .network_idle_deadline(now, key.network, super::IDLE_TIMEOUT)
                    .is_some()
            {
                Self::upstream_changed_locked(&inner, key.network)
            } else {
                None
            };
            (upstream, notify)
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Self::notify_upstream_changed(notify);
        Ok(())
    }

    fn lookup_udp_error(&self, key: UdpErrorKey) -> io::Result<Option<UdpErrorContext>> {
        Ok(self.lock_inner()?.udp_errors.get(&key).cloned())
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

    fn upstream_activity(&self, network: Network) -> io::Result<UpstreamActivity> {
        let mut inner = self.lock_inner()?;
        let now = Instant::now();
        let echo_deadline = inner
            .map
            .network_idle_deadline(now, network, super::IDLE_TIMEOUT);
        if echo_deadline.is_some() || Self::has_udp_error_entries_locked(&inner, network) {
            Ok(UpstreamActivity::Active(echo_deadline))
        } else {
            Ok(UpstreamActivity::Idle)
        }
    }

    fn prune_idle_upstream(&self, network: Network) -> io::Result<UpstreamPrune> {
        let (result, upstream) = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            if Self::has_network_entries_locked(&mut inner, network, now) {
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
        let changed = Arc::new(Notify::new());
        let error_queue = match enable_ipv6_error_queue(socket.get_ref()) {
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
            changed: changed.clone(),
            error_queue,
        };
        let mut start = false;
        let active = {
            let mut inner = state.lock_inner()?;
            if let Some(upstream) = inner.upstream.get(&network) {
                upstream.clone()
            } else {
                if !Self::has_network_entries_locked(&mut inner, network, Instant::now()) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "upstream ICMP registration removed",
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
                active.changed.clone(),
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
        if Self::has_network_entries_locked(inner, network, now) {
            None
        } else {
            inner.upstream.remove(&network)
        }
    }

    fn has_network_entries_locked(
        inner: &mut EchoStateInner,
        network: Network,
        now: Instant,
    ) -> bool {
        inner
            .map
            .has_network_entries(now, super::IDLE_TIMEOUT, network)
            || Self::has_udp_error_entries_locked(inner, network)
    }

    fn has_udp_error_entries_locked(inner: &EchoStateInner, network: Network) -> bool {
        inner.udp_errors.keys().any(|key| key.network == network)
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

    fn upstream_changed_locked(inner: &EchoStateInner, network: Network) -> Option<Arc<Notify>> {
        inner
            .upstream
            .get(&network)
            .map(|upstream| upstream.changed.clone())
    }

    fn notify_upstream_changed(notify: Option<Arc<Notify>>) {
        if let Some(notify) = notify {
            notify.notify_one();
        }
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

    pub(crate) fn register_udp_error(
        &self,
        network: Network,
        upstream: SocketAddrV6,
        destination: SocketAddrV6,
        context: UdpErrorContext,
    ) -> io::Result<UdpErrorRegistration> {
        EchoState::register_udp_error(&self.inner.state, network, upstream, destination, context)
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
    context: &UdpErrorContext,
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
                _ = super::sleep_until_deadline(deadline) => {
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
    let reply = match build_echo_packet_with_checksum(
        ICMPV6_ECHO_REPLY,
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
    let key = UdpErrorKey {
        network,
        upstream: probe.quote.source,
        destination: probe.quote.destination,
    };
    let context = match state.lookup_udp_error(key) {
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

fn classify_error_queue_echo(
    message: &ErrorQueueMessage,
) -> Option<(EchoErrorProbe<'_>, QueuedEchoError)> {
    let probe = error_queue_echo_probe(message)?;
    let error = match message.error.ee_origin {
        SO_EE_ORIGIN_ICMP6 => {
            let (icmp_type, bytes5to8) = error_type_bytes(&message.error)?;
            QueuedEchoError::Upstream {
                offender: message.offender?,
                icmp_type,
                code: message.error.ee_code,
                bytes5to8,
            }
        }
        SO_EE_ORIGIN_LOCAL if message.error.ee_errno == libc::EMSGSIZE as u32 => {
            let mtu = message.error.ee_info;
            if mtu == 0 {
                return None;
            }
            QueuedEchoError::LocalPacketTooBig { mtu }
        }
        _ => return None,
    };
    Some((probe, error))
}

fn echo_error_response(
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

fn quoted_udp_probe(payload: &[u8]) -> Option<UdpErrorProbe<'_>> {
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

fn packet_icmp_error_metadata(packet: &[u8], header: &Icmpv6Header) -> Option<(u8, u8, [u8; 4])> {
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

fn normalize_udp_error_addr(address: SocketAddrV6) -> SocketAddrV6 {
    SocketAddrV6::new(*address.ip(), address.port(), 0, 0)
}

fn recv_raw_icmp_packet<'a>(
    fd: RawFd,
    buffer: &'a mut [u8],
) -> io::Result<Option<ReceivedIcmpPacket<'a>>> {
    let (size, source) = {
        let mut iov = [IoSliceMut::new(buffer)];
        let message = recvmsg::<SockaddrIn6>(fd, &mut iov, None, MsgFlags::MSG_DONTWAIT)
            .map_err(io::Error::from)?;
        if message.bytes == 0 {
            return Ok(None);
        }
        let Some(source) = message.address.map(SocketAddrV6::from) else {
            return Ok(None);
        };
        (message.bytes, source)
    };
    Ok(Some(ReceivedIcmpPacket {
        source,
        payload: &buffer[..size],
    }))
}

fn recv_error_queue(fd: RawFd) -> io::Result<ErrorQueueMessage> {
    let mut buffer = vec![0u8; 2048];
    let (size, destination, error, offender) = {
        let mut control = cmsg_space!(libc::sock_extended_err, libc::sockaddr_in6);
        let mut iov = [IoSliceMut::new(&mut buffer)];
        let message = recvmsg::<SockaddrIn6>(
            fd,
            &mut iov,
            Some(&mut control),
            MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_ERRQUEUE,
        )
        .map_err(io::Error::from)?;
        let destination = message.address.map(SocketAddrV6::from);
        let mut error = None;
        let mut offender = None;
        for cmsg in message.cmsgs().map_err(io::Error::from)? {
            if let ControlMessageOwned::Ipv6RecvErr(value, raw_offender) = cmsg {
                error = Some(value);
                offender = raw_offender
                    .filter(|raw| raw.sin6_family == libc::AF_INET6 as libc::sa_family_t)
                    .map(|raw| SocketAddrV6::from(SockaddrIn6::from(raw)));
            }
        }
        (message.bytes, destination, error, offender)
    };
    let error =
        error.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 error"))?;
    buffer.truncate(size);
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

fn enable_ipv6_error_queue(socket: &Socket) -> io::Result<()> {
    setsockopt(socket, sockopt::Ipv6RecvErr, &true).map_err(io::Error::from)
}

fn set_upstream_echo_hop_limit(socket: &AsyncFd<Socket>, hop_limit: u8) -> io::Result<()> {
    setsockopt(socket.get_ref(), sockopt::Ipv6Ttl, &c_int::from(hop_limit)).map_err(io::Error::from)
}

async fn send_echo_error(entry: EchoEntry, response: EchoErrorResponse, payload: &[u8]) {
    let request = match build_echo_packet_with_checksum(
        ICMPV6_ECHO_REQUEST,
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

fn error_type_bytes(error: &libc::sock_extended_err) -> Option<(u8, [u8; 4])> {
    icmpv6_error_bytes(error.ee_type, error.ee_info)
}
