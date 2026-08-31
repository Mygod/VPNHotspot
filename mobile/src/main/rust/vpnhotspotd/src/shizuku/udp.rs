//! Per-client UDP mappings with time-bounded reply authorization.
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use socket2::{SockAddr, Socket};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::icmp_translate::{self, Correlation, Reported, Untranslatable};
use vpnhotspotd::shared::send_history::{History, Resolution};
use vpnhotspotd::shared::udp_wire::Relayed;
use vpnhotspotd::shared::workers::{Ended, Terminal, Workers};

use vpnhotspotd::shared::admission::{Admission, Class, Lease};
use vpnhotspotd::shared::deadlines::Deadlines;
use vpnhotspotd::shared::egress_socket;

use crate::report;
use crate::shizuku::egress::{self, Fragmentation};
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::reply::{receive, reply_channel, Event, Gate, Sizing, ERROR_OR_READABLE};
use crate::shizuku::send_failure::{self, Failure};
use vpnhotspotd::shared::icmp_error::Reason;

/// Bounds one UDP mapping's upstream socket and permitted remotes. RFC 4787 REQ-5 recommends a five-minute
/// default (and forbids less than two minutes). On expiry the mapping, socket, contacted-remotes, and dynamically
/// growing contacted-endpoint hop-limit history are released; late replies and errors are dropped, and later
/// traffic recreates the mapping. Each IP authorization and all its endpoint evidence use that same deadline;
/// there is no history-specific timeout or capacity. See
/// https://www.rfc-editor.org/rfc/rfc4787.html#section-4.3.
const MAPPING_TIMEOUT: Duration = Duration::from_secs(300);

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
    history: History,
    deadline: Instant,
    /// This mapping's indexed deadline, absent while provisional or cancelled.
    armed: Option<Instant>,
    lease: Lease,
}

#[derive(Default)]
struct Counters {
    sent: u64,
    /// Replies admitted by the interface queue, not written to the TUN.
    queued: u64,
    /// Replies not admitted; `tun output` records the reason.
    unqueued: u64,
    denied: u64,
    expired: u64,
    too_big: u64,
    blocked: u64,
    unreachable: u64,
    send_failed: u64,
    reported: u64,
    unreported: u64,
    df_failed: u64,
    open_failed: u64,
    unpermitted: u64,
    translated: u64,
    untranslated: u64,
    ambiguous: u64,
    untracked: u64,
    implausible: u64,
    stale: u64,
    swept: u64,
    short: u64,
    unavailable: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "sent {} queued {} unqueued {} denied {} expired {} too-big {} blocked {} \
             unreachable {} send-failed {} reported {} unreported {} df-failed {} open-failed {} \
             unpermitted {} translated {} untranslated {} ambiguous {} untracked {} implausible {} stale {} \
             swept {} short {} unavailable {}",
            self.sent,
            self.queued,
            self.unqueued,
            self.denied,
            self.expired,
            self.too_big,
            self.blocked,
            self.unreachable,
            self.send_failed,
            self.reported,
            self.unreported,
            self.df_failed,
            self.open_failed,
            self.unpermitted,
            self.translated,
            self.untranslated,
            self.ambiguous,
            self.untracked,
            self.implausible,
            self.stale,
            self.swept,
            self.short,
            self.unavailable
        )
    }
}

pub(crate) struct Relay {
    mappings: Workers<SocketAddr, Mapping>,
    /// Each live mapping's earliest idle or remote-history deadline.
    deadlines: Deadlines<SocketAddr>,
    errors: egress::ErrorQueue,
    events: mpsc::Sender<Event<SocketAddr>>,
    counters: Counters,
}

struct FirstSend<'a> {
    packet: &'a [u8],
    datagram: Relayed<'a>,
    hop_limit: u8,
    gateways: &'a Gateways,
    output: &'a mut Output,
}

impl Relay {
    pub(crate) fn new() -> (Self, mpsc::Receiver<Event<SocketAddr>>) {
        let (events, receiver) = reply_channel::<SocketAddr>();
        (
            Self {
                mappings: Workers::new("shizuku.udp_mapping"),
                deadlines: Deadlines::default(),
                errors: egress::ErrorQueue::new(),
                events,
                counters: Counters::default(),
            },
            receiver,
        )
    }

    pub(crate) fn release(self, events: mpsc::Receiver<Event<SocketAddr>>) {
        drop(self.mappings);
        drop(self.deadlines);
        drop(self.events);
        drop(events);
        drop(self.errors);
    }

    /// Re-indexes one mapping after its deadline or armed state changes.
    fn rearm(&mut self, key: SocketAddr) {
        let Relay {
            mappings,
            deadlines,
            ..
        } = self;
        let Some(held) = mappings.get_mut(&key) else {
            return;
        };
        // Provisional and cancelled mappings are not expiry candidates.
        let due = (!held.cancel.is_cancelled() && held.record.state == State::Live).then(|| {
            match held.record.history.next_deadline() {
                Some(history) => history.min(held.record.deadline),
                None => held.record.deadline,
            }
        });
        match due {
            Some(due) => deadlines.arm(key, held.record.armed, due),
            None => {
                if let Some(armed) = held.record.armed {
                    deadlines.disarm(key, armed);
                }
            }
        }
        held.record.armed = due;
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
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(datagram.hop_limit)) else {
            self.counters.expired += 1;
            return self.report(packet, Reason::Expired, gateways, output);
        };
        if !self.live(&datagram.source) {
            if self.mappings.contains(&datagram.source) {
                self.counters.unavailable += 1;
                return;
            }
            return self.open(
                FirstSend {
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
        let socket = mapping.socket.get_ref();
        // IPv6 sockets prohibit source fragmentation when opened.
        if !datagram.destination.is_ipv6() {
            if let Err(e) = egress::set_fragmentation(
                socket,
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
                return;
            }
        }
        match egress::send_to(socket, datagram.destination, datagram.payload, hop_limit) {
            Ok(sent) if sent == datagram.payload.len() => {
                self.counters.sent += 1;
                mapping.deadline = Instant::now() + MAPPING_TIMEOUT;
                mapping
                    .history
                    .record(datagram.destination, hop_limit, mapping.deadline);
                self.rearm(datagram.source);
            }
            Ok(_) => self.counters.short += 1,
            Err(e) => {
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
            Failure::Unreachable => self.counters.unreachable += 1,
            Failure::Unexpected => {
                report::io_with_details(
                    "shizuku.udp_send",
                    e,
                    [("destination", datagram.destination.to_string())],
                );
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
                output.packet(error);
            }
            None => self.counters.unreported += 1,
        }
    }

    fn open(&mut self, first: FirstSend<'_>, admission: &mut Admission) {
        let FirstSend {
            packet,
            datagram,
            hop_limit,
            gateways,
            output,
        } = first;
        let key = datagram.source;
        if self.mappings.admits(&key).is_err() {
            self.counters.unavailable += 1;
            return;
        }
        // One mapping owns one upstream UDP socket. Its remote-address rows are memory-only state that grows
        // with the mapping rather than pretending to consume descriptors.
        let Ok(lease) = admission.reserve(Class::General) else {
            self.counters.denied += 1;
            return;
        };
        let socket = match self.bind(datagram.destination.is_ipv6()) {
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
            history: History::default(),
            deadline: Instant::now() + MAPPING_TIMEOUT,
            armed: None,
            lease,
        };
        if let Err((provisional, _)) = self.mappings.admit(key, &identity, provisional, worker) {
            drop(socket);
            self.discard(provisional, admission);
            self.counters.unavailable += 1;
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
        mapping
            .history
            .record(datagram.destination, hop_limit, mapping.deadline);
        mapping.state = State::Live;
        // The receive task cannot observe or report this mapping until the first send has committed.
        if let Some(commit) = mapping.commit.take() {
            let _ = commit.send(());
        }
        // Arm only after the provisional mapping commits.
        self.rearm(key);
    }

    fn roll_back(&mut self, key: SocketAddr) {
        if let Some(held) = self.mappings.get_mut(&key) {
            held.record.state = State::RolledBack;
            held.record.commit = None;
        }
        self.mappings.cancel(&key);
        // Rollback removes the mapping from expiry scheduling.
        self.rearm(key);
    }

    fn discard(&mut self, provisional: Mapping, admission: &mut Admission) {
        let Mapping {
            socket,
            history,
            lease,
            ..
        } = provisional;
        drop(socket);
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
            Failure::Unreachable => self.counters.unreachable += 1,
            Failure::Unexpected => {
                report::io_with_details(
                    "shizuku.udp_send",
                    e,
                    [("destination", datagram.destination.to_string())],
                );
                self.counters.send_failed += 1;
            }
        }
    }

    fn bind(&self, ipv6: bool) -> io::Result<AsyncFd<Socket>> {
        let socket = egress_socket::open_udp(ipv6)?;
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

    fn translate(&mut self, key: SocketAddr, error: &Reported, now: Instant, output: &mut Output) {
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
        if !mapping.record.history.authorizes(destination.ip(), now) {
            self.counters.unpermitted += 1;
            return;
        }
        match icmp_translate::repeat(key, error, Correlation::Address) {
            Ok(packet) => {
                self.counters.translated += 1;
                output.packet(packet);
                return;
            }
            Err(Untranslatable::Implausible) => {
                self.counters.implausible += 1;
                return;
            }
            Err(Untranslatable::Unsupported) => {
                self.counters.untranslated += 1;
                return;
            }
            Err(Untranslatable::Uncorrelated) => {}
        }
        let Some(mapping) = self
            .mappings
            .get_mut(&key)
            .filter(|held| held.record.state == State::Live)
        else {
            self.counters.stale += 1;
            return;
        };
        let correlation = match mapping.record.history.resolve(destination) {
            Resolution::Matched { hop_limit } => Correlation::Datagram { hop_limit },
            Resolution::Ambiguous => {
                self.counters.ambiguous += 1;
                return;
            }
            Resolution::Untracked => {
                self.counters.untracked += 1;
                return;
            }
        };
        match icmp_translate::repeat(key, error, correlation) {
            Ok(packet) => {
                self.counters.translated += 1;
                output.packet(packet);
            }
            Err(Untranslatable::Implausible) => self.counters.implausible += 1,
            Err(_) => self.counters.untranslated += 1,
        }
    }

    pub(crate) fn handle(&mut self, event: Event<SocketAddr>, now: Instant, output: &mut Output) {
        let (key, id, remote, hop_limit, payload) = match event {
            Event::Error { key, id, error } => {
                if !self.mappings.current(&key, id) {
                    self.counters.stale += 1;
                    return;
                }
                self.translate(key, &error, now, output);
                return;
            }
            Event::Reply {
                key,
                id,
                remote,
                hop_limit,
                payload,
            } => (key, id, remote, hop_limit, payload),
        };
        match self.mappings.get(&key) {
            Some(mapping) if mapping.id == id && mapping.record.state == State::Live => {
                if !mapping.record.history.authorizes(remote.ip(), now) {
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
        // Count actual handoff admission, not an attempted reply.
        if output.datagram(now, remote, key, hop_limit, &payload) {
            self.counters.queued += 1;
        } else {
            self.counters.unqueued += 1;
        }
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
                    history,
                    lease,
                    commit,
                    armed,
                    ..
                } = mapping;
                if let Some(armed) = armed {
                    self.deadlines.disarm(key, armed);
                }
                drop(commit);
                drop(socket);
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

    /// Settles due mappings and re-arms any surviving state.
    pub(crate) fn sweep(&mut self) {
        let now = Instant::now();
        while let Some(key) = self.deadlines.due(now) {
            let Relay {
                mappings, counters, ..
            } = self;
            let Some(held) = mappings.get_mut(&key) else {
                continue;
            };
            // The entry this loop took is gone, so the re-arm below inserts rather than replaces.
            held.record.armed = None;
            let expired = held.record.history.expire(now);
            if expired > 0 {
                counters.swept += expired as u64;
            }
            if held.record.deadline <= now {
                held.cancel.cancel();
            }
            self.rearm(key);
        }
    }

    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.next()
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} mappings, {}",
            self.mappings.len(),
            self.counters.describe()
        )
    }
}
