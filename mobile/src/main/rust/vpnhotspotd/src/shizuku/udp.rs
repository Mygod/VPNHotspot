//! Per-client UDP mappings with bounded reply authorization and ICMP correlation.
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use socket2::{SockAddr, Socket};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::icmp_translate::{self, Correlation, Reported, Untranslatable};
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::send_history::{History, Resolution};
use vpnhotspotd::shared::udp_wire::Relayed;
use vpnhotspotd::shared::workers::{Ended, Terminal, Workers};

use vpnhotspotd::shared::admission::{logical_footprint, Admission, Class, Denied, Lease, Request};
use vpnhotspotd::shared::egress::RelayUpstream as Upstream;

use crate::report;
use crate::shizuku::egress::{self, Fragmentation};
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::reply::{
    receive, reply_channel, reply_channel_bytes, Event, Gate, Sizing, ERROR_OR_READABLE,
};
use crate::shizuku::send_failure::{self, Failure};
use crate::shizuku::tun_writer::Stamp;
use vpnhotspotd::shared::icmp_error::Reason;

// A bounded metadata history is enough to correlate errors without retaining client payloads.
const HISTORY_DEPTH: usize = 8;

// RFC 4787 requires at least two minutes; five minutes is the recommended default.
const MAPPING_TIMEOUT: Duration = Duration::from_secs(300);

// Limits the addresses allowed to send replies through one client mapping.
const REMOTES_PREPARED: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Provisional,
    Live,
    RolledBack,
}

struct Mapping {
    state: State,
    commit: Option<oneshot::Sender<()>>,
    socket: Arc<AsyncFd<Socket>>,
    remotes: HashMap<IpAddr, Instant>,
    deadline: Instant,
    lease: Lease,
    history: History,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Permit {
    Recorded,
    Free,
    Full,
}

impl Mapping {
    fn permits(&self, remote: IpAddr) -> Permit {
        if self.remotes.contains_key(&remote) {
            Permit::Recorded
        } else if self.remotes.len() < REMOTES_PREPARED {
            Permit::Free
        } else {
            Permit::Full
        }
    }

    fn footprint() -> Option<u64> {
        logical_footprint::<(IpAddr, Instant)>(REMOTES_PREPARED)?
            .checked_add(History::footprint(HISTORY_DEPTH)?)?
            .checked_add(egress::ErrorQueue::footprint())
    }
}

#[derive(Default)]
struct Counters {
    sent: u64,
    written: u64,
    denied: u64,
    no_upstream: u64,
    expired: u64,
    too_big: u64,
    blocked: u64,
    unreachable: u64,
    send_failed: u64,
    reported: u64,
    unreported: u64,
    df_failed: u64,
    open_failed: u64,
    foreign_interface: u64,
    unpermitted: u64,
    translated: u64,
    untranslated: u64,
    implausible: u64,
    ambiguous: u64,
    unsent: u64,
    stale: u64,
    swept: u64,
    short: u64,
    unprepared: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "sent {} written {} denied {} no-upstream {} expired {} too-big {} blocked {} \
             unreachable {} send-failed {} reported {} unreported {} df-failed {} open-failed {} \
             foreign-interface {} unpermitted {} translated {} untranslated {} implausible {} \
             ambiguous {} unsent {} stale {} swept {} short {} unprepared {}",
            self.sent,
            self.written,
            self.denied,
            self.no_upstream,
            self.expired,
            self.too_big,
            self.blocked,
            self.unreachable,
            self.send_failed,
            self.reported,
            self.unreported,
            self.df_failed,
            self.open_failed,
            self.foreign_interface,
            self.unpermitted,
            self.translated,
            self.untranslated,
            self.implausible,
            self.ambiguous,
            self.unsent,
            self.stale,
            self.swept,
            self.short,
            self.unprepared
        )
    }
}

pub(crate) struct Relay {
    stamp: Stamp,
    upstream: Option<Upstream>,
    mappings: Workers<SocketAddr, Mapping>,
    tables: Lease,
    errors: egress::ErrorQueue,
    events: mpsc::Sender<Event<SocketAddr>>,
    counters: Counters,
    reported_send_failure: bool,
}

struct FirstSend<'a> {
    upstream: Upstream,
    packet: &'a [u8],
    datagram: Relayed<'a>,
    hop_limit: u8,
    gateways: &'a Gateways,
    output: &'a mut Output,
}

impl Relay {
    pub(crate) fn new(
        admission: &mut Admission,
    ) -> Result<(Self, mpsc::Receiver<Event<SocketAddr>>), Denied> {
        let prepared = admission.general_record_ceiling() as usize;
        let bytes = Workers::<SocketAddr, Mapping>::footprint(prepared)
            .and_then(|table| table.checked_add(reply_channel_bytes::<SocketAddr>()?))
            .and_then(|bytes| bytes.checked_add(egress::ErrorQueue::footprint()))
            .ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Request::bytes(bytes, Class::General))?;
        let (events, receiver) = reply_channel::<SocketAddr>();
        Ok((
            Self {
                stamp: Stamp::default(),
                upstream: None,
                mappings: Workers::with_capacity("shizuku.udp_mapping", prepared),
                tables,
                errors: egress::ErrorQueue::new(),
                events,
                counters: Counters::default(),
                reported_send_failure: false,
            },
            receiver,
        ))
    }

    pub(crate) fn release(
        self,
        events: mpsc::Receiver<Event<SocketAddr>>,
        admission: &mut Admission,
    ) {
        drop(self.mappings);
        drop(self.events);
        drop(events);
        drop(self.errors);
        admission.release(self.tables);
    }

    pub(crate) async fn apply(
        &mut self,
        stamp: Stamp,
        upstream: Option<Upstream>,
        admission: &mut Admission,
    ) {
        let retiring = stamp != self.stamp;
        self.stamp = stamp;
        self.upstream = upstream;
        if retiring {
            // Return only after every old-generation receive task is joined and its descriptor is closed.
            self.shutdown(admission).await;
        }
    }

    pub(crate) async fn shutdown(&mut self, admission: &mut Admission) {
        self.mappings.cancel_all();
        while self.mappings.working() {
            let terminal = self.mappings.finished().await;
            self.close(terminal, admission);
        }
    }

    pub(crate) fn relay(
        &mut self,
        packet: &[u8],
        datagram: Relayed<'_>,
        gateways: &Gateways,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        let Some(upstream) = self.upstream else {
            self.counters.no_upstream += 1;
            return;
        };
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(datagram.hop_limit)) else {
            self.counters.expired += 1;
            return self.report(packet, Reason::Expired, gateways, output);
        };
        if !self.live(&datagram.source) {
            if self.mappings.contains(&datagram.source) {
                self.counters.unprepared += 1;
                return;
            }
            return self.open(
                FirstSend {
                    upstream,
                    packet,
                    datagram,
                    hop_limit,
                    gateways,
                    output,
                },
                admission,
            );
        }
        let Some(mapping) = self
            .mappings
            .get_mut(&datagram.source)
            .map(|held| &mut held.record)
        else {
            return;
        };
        let unrecorded = match mapping.permits(datagram.destination.ip()) {
            Permit::Recorded => false,
            Permit::Free => true,
            Permit::Full => {
                self.counters.unprepared += 1;
                return;
            }
        };
        if unrecorded
            && admission
                .grow(&mapping.lease, Request::records(1, Class::General))
                .is_err()
        {
            self.counters.denied += 1;
            return;
        }
        let socket = mapping.socket.get_ref();
        if !datagram.destination.is_ipv6() {
            if let Err(e) = egress::set_fragmentation(
                socket,
                if datagram.dont_fragment {
                    Fragmentation::Prohibited
                } else {
                    Fragmentation::Permitted
                },
            ) {
                if unrecorded {
                    admission.shrink(&mapping.lease, Request::records(1, Class::General));
                }
                self.counters.df_failed += 1;
                report::io_with_details(
                    "shizuku.udp_fragmentation",
                    e,
                    [
                        ("destination", datagram.destination.to_string()),
                        ("dont_fragment", datagram.dont_fragment.to_string()),
                    ],
                );
                return;
            }
        }
        match egress::send_to(socket, datagram.destination, datagram.payload, hop_limit) {
            Ok(_) => {
                self.counters.sent += 1;
                mapping.deadline = Instant::now() + MAPPING_TIMEOUT;
                mapping.history.record(
                    datagram.destination,
                    datagram.payload,
                    hop_limit,
                    HISTORY_DEPTH,
                    Instant::now(),
                );
                mapping
                    .remotes
                    .insert(datagram.destination.ip(), mapping.deadline);
            }
            Err(e) => {
                if unrecorded {
                    admission.shrink(&mapping.lease, Request::records(1, Class::General));
                }
                self.fail(e, packet, datagram, gateways, output);
            }
        }
    }

    fn fail(
        &mut self,
        e: io::Error,
        packet: &[u8],
        datagram: Relayed<'_>,
        gateways: &Gateways,
        output: &mut Output,
    ) {
        let Some(mapping) = self.mappings.get(&datagram.source) else {
            return;
        };
        let cancel = mapping.cancel.clone();
        let socket = Arc::clone(&mapping.record.socket);
        match send_failure::classify(&e) {
            Failure::Blocked => self.counters.blocked += 1,
            Failure::TooBig => {
                self.counters.too_big += 1;
                match egress::drain_local_error(socket.get_ref(), &mut self.errors) {
                    Ok(Some(queued)) if queued.errno == libc::EMSGSIZE => {
                        self.report(
                            packet,
                            Reason::TooBig { mtu: queued.info },
                            gateways,
                            output,
                        );
                    }
                    Ok(_) => self.counters.unreported += 1,
                    Err(e) => {
                        self.counters.unreported += 1;
                        report::io_with_details(
                            "shizuku.udp_path_mtu",
                            e,
                            [
                                ("principal", datagram.source.ip().to_string()),
                                ("destination", datagram.destination.to_string()),
                            ],
                        );
                    }
                }
            }
            Failure::NetworkGone => {
                cancel.cancel();
                self.counters.unreachable += 1;
            }
            Failure::Unreachable => self.counters.unreachable += 1,
            Failure::Unexpected => {
                if !self.reported_send_failure {
                    self.reported_send_failure = true;
                    report::io_with_details(
                        "shizuku.udp_send",
                        e,
                        [
                            ("destination", datagram.destination.to_string()),
                            ("note", "later failures are counted only".to_owned()),
                        ],
                    );
                }
                self.counters.send_failed += 1;
            }
        }
    }

    fn live(&self, key: &SocketAddr) -> bool {
        self.mappings
            .get(key)
            .is_some_and(|held| held.record.state == State::Live)
    }

    fn report(&mut self, packet: &[u8], reason: Reason, gateways: &Gateways, output: &mut Output) {
        match gateways.report(packet, reason) {
            Some(error) => {
                self.counters.reported += 1;
                output.packet(self.stamp, error);
            }
            None => self.counters.unreported += 1,
        }
    }

    fn open(&mut self, first: FirstSend<'_>, admission: &mut Admission) {
        let FirstSend {
            upstream,
            packet,
            datagram,
            hop_limit,
            gateways,
            output,
        } = first;
        let key = datagram.source;
        if self.mappings.admits(&key).is_err() {
            self.counters.unprepared += 1;
            return;
        }
        let Some(bytes) = Mapping::footprint() else {
            self.counters.denied += 1;
            return;
        };
        let Ok(lease) = admission.reserve(Request {
            records: 2,
            record_class: Class::General,
            bytes,
            byte_class: Class::General,
            ..Request::default()
        }) else {
            self.counters.denied += 1;
            return;
        };
        let socket = match self.bind(upstream.network, datagram.destination.is_ipv6()) {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                report::io_with_details("shizuku.udp_open", e, [("source", key)]);
                admission.release(lease);
                self.counters.open_failed += 1;
                return;
            }
        };
        let (commit, gate) = oneshot::channel();
        let Ok(identity) = self.mappings.identity() else {
            drop((socket, commit, gate));
            self.counters.denied += 1;
            return admission.release(lease);
        };
        let worker = receive(
            Arc::clone(&socket),
            key,
            identity.id,
            Sizing::Peek,
            Gate::Pending(gate),
            self.events.clone(),
            identity.cancel.clone(),
        );
        let provisional = Mapping {
            state: State::Provisional,
            commit: Some(commit),
            socket: Arc::clone(&socket),
            remotes: HashMap::with_capacity(REMOTES_PREPARED),
            deadline: Instant::now() + MAPPING_TIMEOUT,
            lease,
            history: History::with_capacity(HISTORY_DEPTH),
        };
        if let Err((provisional, _)) = self.mappings.admit(key, &identity, provisional, worker) {
            drop(socket);
            self.discard(provisional, admission);
            self.counters.unprepared += 1;
            return;
        }
        if !datagram.destination.is_ipv6() {
            if let Err(e) = egress::set_fragmentation(
                socket.get_ref(),
                if datagram.dont_fragment {
                    Fragmentation::Prohibited
                } else {
                    Fragmentation::Permitted
                },
            ) {
                self.counters.df_failed += 1;
                report::io_with_details(
                    "shizuku.udp_fragmentation",
                    e,
                    [
                        ("destination", datagram.destination.to_string()),
                        ("dont_fragment", datagram.dont_fragment.to_string()),
                    ],
                );
                return self.roll_back(key);
            }
        }
        let sent = match egress::send_to(
            socket.get_ref(),
            datagram.destination,
            datagram.payload,
            hop_limit,
        ) {
            Ok(sent) => sent,
            Err(e) => {
                let queued = match send_failure::classify(&e) {
                    Failure::TooBig => {
                        match egress::drain_local_error(socket.get_ref(), &mut self.errors) {
                            Ok(queued) => queued,
                            Err(drain) => {
                                report::io_with_details(
                                    "shizuku.udp_path_mtu",
                                    drain,
                                    [
                                        ("principal", datagram.source.ip().to_string()),
                                        ("destination", datagram.destination.to_string()),
                                    ],
                                );
                                None
                            }
                        }
                    }
                    _ => None,
                };
                self.roll_back(key);
                return self.fail_unmapped(e, packet, datagram, queued, gateways, output);
            }
        };
        if sent != datagram.payload.len() {
            self.counters.short += 1;
            return self.roll_back(key);
        }
        self.counters.sent += 1;
        let Some(mapping) = self.mappings.get_mut(&key).map(|held| &mut held.record) else {
            self.counters.stale += 1;
            return;
        };
        mapping.deadline = Instant::now() + MAPPING_TIMEOUT;
        mapping.history.record(
            datagram.destination,
            datagram.payload,
            hop_limit,
            HISTORY_DEPTH,
            mapping.deadline - MAPPING_TIMEOUT,
        );
        mapping
            .remotes
            .insert(datagram.destination.ip(), mapping.deadline);
        mapping.state = State::Live;
        // The receive task cannot observe or report this mapping until the first send has committed.
        if let Some(commit) = mapping.commit.take() {
            let _ = commit.send(());
        }
    }

    fn roll_back(&mut self, key: SocketAddr) {
        if let Some(held) = self.mappings.get_mut(&key) {
            held.record.state = State::RolledBack;
            held.record.commit = None;
        }
        self.mappings.cancel(&key);
    }

    fn discard(&mut self, provisional: Mapping, admission: &mut Admission) {
        let Mapping {
            socket,
            remotes,
            lease,
            history,
            ..
        } = provisional;
        drop(socket);
        drop(remotes);
        drop(history);
        admission.release(lease);
    }

    fn fail_unmapped(
        &mut self,
        e: io::Error,
        packet: &[u8],
        datagram: Relayed<'_>,
        queued: Option<egress::QueuedError>,
        gateways: &Gateways,
        output: &mut Output,
    ) {
        match send_failure::classify(&e) {
            Failure::Blocked => self.counters.blocked += 1,
            Failure::TooBig => {
                self.counters.too_big += 1;
                match queued {
                    Some(queued) if queued.errno == libc::EMSGSIZE => self.report(
                        packet,
                        Reason::TooBig { mtu: queued.info },
                        gateways,
                        output,
                    ),
                    _ => self.counters.unreported += 1,
                }
            }
            Failure::NetworkGone | Failure::Unreachable => self.counters.unreachable += 1,
            Failure::Unexpected => {
                if !self.reported_send_failure {
                    self.reported_send_failure = true;
                    report::io_with_details(
                        "shizuku.udp_send",
                        e,
                        [
                            ("destination", datagram.destination.to_string()),
                            ("note", "later failures are counted only".to_owned()),
                        ],
                    );
                }
                self.counters.send_failed += 1;
            }
        }
    }

    fn bind(&self, network: Network, ipv6: bool) -> io::Result<AsyncFd<Socket>> {
        let socket = egress::open_udp(network, ipv6)?;
        socket.bind(&SockAddr::from(SocketAddr::new(
            if ipv6 {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            } else {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            },
            0,
        )))?;
        AsyncFd::with_interest(socket, ERROR_OR_READABLE)
    }

    fn translate(&mut self, key: SocketAddr, error: &Reported, output: &mut Output) {
        let Some(mapping) = self
            .mappings
            .get(&key)
            .filter(|held| held.record.state == State::Live)
        else {
            self.counters.stale += 1;
            return;
        };
        let Some(destination) = error.destination else {
            self.counters.implausible += 1;
            return;
        };
        if !mapping.record.remotes.contains_key(&destination.ip()) {
            self.counters.unpermitted += 1;
            return;
        }
        let refused = match icmp_translate::repeat(key, error, Correlation::Address) {
            Ok(packet) => {
                self.counters.translated += 1;
                output.packet(self.stamp, packet);
                return;
            }
            Err(refused) => refused,
        };
        if refused != Untranslatable::Uncorrelated {
            match refused {
                Untranslatable::Implausible => self.counters.implausible += 1,
                _ => self.counters.untranslated += 1,
            }
            return;
        }
        let Some(mapping) = self.mappings.get_mut(&key) else {
            self.counters.stale += 1;
            return;
        };
        let (resolution, _) =
            mapping
                .record
                .history
                .resolve(destination, error.quoted.as_slice(), Instant::now());
        let hop_limit = match resolution {
            Resolution::Matched { hop_limit } => hop_limit,
            Resolution::Ambiguous => {
                self.counters.ambiguous += 1;
                return;
            }
            Resolution::Untracked => {
                self.counters.unsent += 1;
                return;
            }
            Resolution::Spent => {
                self.counters.untranslated += 1;
                return;
            }
        };
        match icmp_translate::repeat(key, error, Correlation::Datagram { hop_limit }) {
            Ok(packet) => {
                self.counters.translated += 1;
                output.packet(self.stamp, packet);
            }
            Err(Untranslatable::Implausible) => self.counters.implausible += 1,
            Err(_) => self.counters.untranslated += 1,
        }
    }

    pub(crate) fn handle(&mut self, event: Event<SocketAddr>, output: &mut Output) {
        let (key, id, remote, hop_limit, interface, payload) = match event {
            Event::Error { key, id, error } => {
                if !self.mappings.current(&key, id) {
                    self.counters.stale += 1;
                    return;
                }
                self.translate(key, &error, output);
                return;
            }
            Event::Reply {
                key,
                id,
                remote,
                hop_limit,
                interface,
                payload,
            } => (key, id, remote, hop_limit, interface, payload),
        };
        let Some(upstream) = self.upstream else {
            self.counters.stale += 1;
            return;
        };
        match self.mappings.get(&key) {
            Some(mapping) if mapping.id == id && mapping.record.state == State::Live => {
                if interface != upstream.interface {
                    self.counters.foreign_interface += 1;
                    return;
                }
                if !mapping.record.remotes.contains_key(&remote.ip()) {
                    self.counters.unpermitted += 1;
                    return;
                }
            }
            _ => {
                self.counters.stale += 1;
                return;
            }
        }
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            self.counters.expired += 1;
            return;
        };
        output.datagram(self.stamp, remote, key, hop_limit, &payload);
        self.counters.written += 1;
    }

    pub(crate) fn close(&mut self, terminal: Terminal<SocketAddr>, admission: &mut Admission) {
        let Terminal { key, id, ended } = terminal;
        match ended {
            Ended::Expected => {}
            Ended::Reported(reason) => report::stdout!("udp mapping {key} closed: {reason}"),
            Ended::Failed { context, error } => {
                report::io_with_details(context, error, [("mapping", key)])
            }
        }
        match self.mappings.retire(&key, id) {
            Some(mapping) => {
                let Mapping {
                    socket,
                    remotes,
                    lease,
                    history,
                    commit,
                    ..
                } = mapping;
                drop(commit);
                drop(socket);
                drop(remotes);
                drop(history);
                // The worker was joined before this terminal, so dropping the retained socket closes the fd.
                admission.release(lease);
            }
            None => self.counters.stale += 1,
        }
    }

    pub(crate) async fn finished(&mut self) -> Terminal<SocketAddr> {
        self.mappings.finished().await
    }

    pub(crate) fn sweep(&mut self, admission: &mut Admission) {
        let now = Instant::now();
        for mapping in self.mappings.values_mut() {
            if mapping.cancel.is_cancelled() || mapping.record.state != State::Live {
                continue;
            }
            let before = mapping.record.remotes.len();
            mapping.record.remotes.retain(|_, deadline| *deadline > now);
            let expired = before - mapping.record.remotes.len();
            if expired > 0 {
                admission.shrink(
                    &mapping.record.lease,
                    Request::records(expired as u32, Class::General),
                );
                self.counters.swept += expired as u64;
            }
            if mapping.record.deadline <= now {
                mapping.cancel.cancel();
            }
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.mappings
            .values()
            .filter(|mapping| !mapping.cancel.is_cancelled() && mapping.record.state == State::Live)
            .flat_map(|mapping| {
                std::iter::once(mapping.record.deadline)
                    .chain(mapping.record.remotes.values().copied())
            })
            .min()
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} mappings, {}",
            self.mappings.len(),
            self.counters.describe()
        )
    }
}
