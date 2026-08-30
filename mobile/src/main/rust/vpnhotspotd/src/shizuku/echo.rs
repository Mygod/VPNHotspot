use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use socket2::Socket;
use tokio::sync::mpsc;
use vpnhotspotd::shared::echo_wire::{self, Request, ECHO_HEADER_LEN};
use vpnhotspotd::shared::icmp_error::{self, Reason};
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::icmp_translate::{self, Correlation, Reported, Untranslatable};
use vpnhotspotd::shared::workers::{Ended, Terminal};

use crate::report;
use crate::shizuku::echo_session::{Found, Sessions};
use crate::shizuku::echo_socket::{Family, Refused, Sockets};
use crate::shizuku::egress::{self, Fragmentation};
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::reply::Event;
use crate::shizuku::send_failure::{self, Failure};
use vpnhotspotd::shared::admission::{Admission, Class, Denied, Lease, Request as Grant};

/// Counters rather than a report per event, for the same reason as everywhere else on this path: the input is
/// chosen by whoever puts packets on the interface, so anything printed per packet is a flood by construction.
/// Only once-per-socket outcomes are printed.
#[derive(Default)]
struct Counters {
    sent: u64,
    written: u64,
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
    exhausted: u64,
    unmatched: u64,
    unparseable: u64,
    translated: u64,
    untranslated: u64,
    ambiguous: u64,
    implausible: u64,
    stale: u64,
    swept: u64,
    /// Refused because a prepared table was full rather than because the aggregate was.
    unprepared: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "sent {} written {} denied {} expired {} too-big {} blocked {} \
             unreachable {} send-failed {} reported {} unreported {} df-failed {} open-failed {} \
             exhausted {} unmatched {} unparseable {} translated {} untranslated {} ambiguous {} \
             implausible {} stale {} swept {} unprepared {}",
            self.sent,
            self.written,
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
            self.exhausted,
            self.unmatched,
            self.unparseable,
            self.translated,
            self.untranslated,
            self.ambiguous,
            self.implausible,
            self.stale,
            self.swept,
            self.unprepared
        )
    }
}

/// How many outstanding pings the session table is prepared for.
const SESSIONS_PREPARED: usize = 1024;

pub(crate) struct Relay {
    sockets: Sockets,
    sessions: Sessions,
    /// The session table's own row state and the error-queue scratch below, charged once for the session and
    /// released once. Retained, not per-entry: what a session's own record pays for is its place in the bound,
    /// not a second copy of the row.
    tables: Lease,
    /// The one error-queue scratch this owner lends to its send-failure path - held rather than built per
    /// failure, for the same reason [crate::shizuku::udp::Relay] holds one.
    errors: egress::ErrorQueue,
    counters: Counters,
    reported_send_failure: bool,
    reported_unparseable: bool,
    reported_unattributable: bool,
}

impl Relay {
    pub(crate) fn new(
        admission: &mut Admission,
    ) -> Result<(Self, mpsc::Receiver<Event<Family>>), Denied> {
        let bytes = Sessions::footprint(SESSIONS_PREPARED)
            .and_then(|table| table.checked_add(egress::ErrorQueue::footprint()))
            .ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Grant::bytes(bytes, Class::General))?;
        // Released explicitly if the nested owner refuses. A lease is an inert identity, so dropping it gives
        // nothing back: returning here without this would leave this owner's own rows charged to a session
        // that never started.
        let (sockets, receiver) = match Sockets::new(admission) {
            Ok(built) => built,
            Err(why) => {
                admission.release(tables);
                return Err(why);
            }
        };
        Ok((
            Self {
                sockets,
                sessions: Sessions::with_capacity(SESSIONS_PREPARED),
                tables,
                errors: egress::ErrorQueue::new(),
                counters: Counters::default(),
                reported_send_failure: false,
                reported_unparseable: false,
                reported_unattributable: false,
            },
            receiver,
        ))
    }

    /// Releases both tables' capacity, after every socket has been settled and every session dropped.
    /// Gives this owner's retained capacity back, once everything it covers is physically gone.
    pub(crate) fn release(self, echoes: mpsc::Receiver<Event<Family>>, admission: &mut Admission) {
        self.sockets.release(echoes, admission);
        drop(self.sessions);
        drop(self.errors);
        admission.release(self.tables);
    }

    /// Drops every session, cancels every socket, and joins every receive task.
    pub(crate) async fn shutdown(&mut self, admission: &mut Admission) {
        // Every session's record goes with it. The table's own row state stays charged to [Relay::tables]:
        // it was charged for the bound rather than for what is in it - and `HashMap::clear`, which is what
        // empties it below, is documented to keep the allocated memory for reuse anyway.
        let cleared = self.sessions.clear();
        if cleared > 0 {
            admission.shrink(&self.tables, Grant::records(cleared as u32, Class::General));
        }
        self.sockets.cancel();
        while self.sockets.working() {
            let terminal = self.sockets.finished().await;
            self.closed(terminal, admission);
        }
    }

    /// Sends one client Echo Request upstream under a sequence of the daemon's own, opening the family's ping
    /// socket if this is the first.
    pub(crate) fn relay(
        &mut self,
        packet: &[u8],
        request: Request<'_>,
        gateways: &Gateways,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(request.hop_limit)) else {
            self.counters.expired += 1;
            // A router owes Time Exceeded here, and for a ping it is the whole point: this is what makes the
            // daemon appear as a hop in a traceroute rather than as a gap in one.
            return self.report(packet, Reason::Expired, gateways, output);
        };
        let family = Family::of(request.remote);
        let socket = match self.sockets.acquire(family, admission) {
            Ok(socket) => socket,
            Err(Refused::Denied) => {
                self.counters.denied += 1;
                return;
            }
            Err(Refused::OpenFailed) => {
                self.counters.open_failed += 1;
                return;
            }
        };
        if !family.ipv6() {
            if let Err(e) = egress::set_fragmentation(
                socket.get_ref(),
                // Reapplied immediately before each send and never left to another task to interleave, because
                // this one socket carries requests from every client and their DF bits differ. Nothing is
                // awaited between here and the send, which is what makes that safe. IPv6 has no such bit: no
                // router may fragment an IPv6 packet, so there is nothing per-packet to reapply.
                if request.dont_fragment {
                    Fragmentation::Prohibited
                } else {
                    Fragmentation::Permitted
                },
            ) {
                self.counters.df_failed += 1;
                // This daemon's own socket refusing this daemon's own option: not client-driven, and a
                // counter on its own says only that some ping did not leave. Reported like the UDP relay's
                // identical step, and coalesced by site so a persistent failure costs one report a window.
                report::io_with_details(
                    "shizuku.echo_fragmentation",
                    e,
                    [
                        ("remote", request.remote.to_string()),
                        ("dont_fragment", request.dont_fragment.to_string()),
                    ],
                );
                return;
            }
        }
        // Chosen before the send because it is what goes on the wire, and charged before it because a session
        // that cannot be recorded is a ping whose reply would arrive unattributable.
        let Some(sequence) = self.sessions.allocate(request.remote) else {
            self.counters.exhausted += 1;
            return;
        };
        // The table has to have a free slot before the send, because a request already on the wire whose
        // session could not be recorded is a reply nothing can attribute.
        if !self.sessions.admits() {
            self.counters.unprepared += 1;
            return;
        }
        // One record for the session. Its bytes were prepared and charged with the table, so this is the
        // whole of what a session costs beyond what already exists.
        if admission
            .grow(&self.tables, Grant::records(1, Class::General))
            .is_err()
        {
            self.counters.denied += 1;
            return;
        }
        let message = echo_wire::build_request(family.ipv6(), sequence, request.payload);
        // Port zero because a ping socket has no remote port: the kernel reads the address out of the sockaddr
        // and ignores the rest.
        match egress::send_to(
            socket.get_ref(),
            SocketAddr::new(request.remote, 0),
            &message,
            hop_limit,
        ) {
            Ok(_) => {
                self.counters.sent += 1;
                self.sessions.insert(
                    request.remote,
                    sequence,
                    request.client,
                    request.identity,
                    request.hop_limit,
                    Instant::now(),
                );
            }
            // Nothing was recorded, so the record is given back here rather than by an expiry that would
            // never come. The table's own charge is not: it covers the bound, not the entry.
            Err(e) => {
                admission.shrink(&self.tables, Grant::records(1, Class::General));
                self.fail(packet, socket.get_ref(), e, gateways, output);
            }
        }
    }

    /// Turns one failed send into the counter that names it and, where the daemon owes the client an
    /// explanation, into the ICMP error that gives it.
    fn fail(
        &mut self,
        packet: &[u8],
        socket: &Socket,
        e: io::Error,
        gateways: &Gateways,
        output: &mut Output,
    ) {
        match send_failure::classify(&e) {
            Failure::Blocked => self.counters.blocked += 1,
            Failure::TooBig => {
                self.counters.too_big += 1;
                match egress::drain_local_error(socket, &mut self.errors) {
                    Ok(Some(queued)) if queued.errno == libc::EMSGSIZE => {
                        self.report(
                            packet,
                            Reason::TooBig { mtu: queued.info },
                            gateways,
                            output,
                        );
                    }
                    // No local refusal in the queue, or one about something else. Errors a router sent are
                    // drained past rather than used, because each is about whichever destination *its* packet
                    // was aimed at and one ping socket serves every client. Without an MTU attributable to this
                    // destination there is nothing truthful to say, and a wrong one is cached for minutes.
                    Ok(queued) => {
                        if !self.reported_unattributable {
                            self.reported_unattributable = true;
                            report::stdout!(
                                "echo refused an oversized request but found no local path MTU, and later \
                                 ones are counted only: {queued:?}"
                            );
                        }
                        self.counters.unreported += 1;
                    }
                    Err(e) => {
                        self.counters.unreported += 1;
                        report::io("shizuku.echo_path_mtu", e);
                    }
                }
            }
            Failure::Unreachable => self.counters.unreachable += 1,
            Failure::Unexpected => {
                if !self.reported_send_failure {
                    self.reported_send_failure = true;
                    report::io_with_details(
                        "shizuku.echo_send",
                        e,
                        [("note", "later failures are counted only")],
                    );
                }
                self.counters.send_failed += 1;
            }
        }
    }

    /// Tells the client why its ping did not go, from the interface's own address as a router would.
    fn report(&mut self, packet: &[u8], reason: Reason, gateways: &Gateways, output: &mut Output) {
        match gateways.report(packet, reason) {
            Some(error) => {
                self.counters.reported += 1;
                output.packet(error);
            }
            None => self.counters.unreported += 1,
        }
    }

    pub(crate) fn handle(
        &mut self,
        event: Event<Family>,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        let (family, id, remote, hop_limit, message) = match event {
            Event::Error { key, id, error } => {
                if !self.sockets.current(key, id) {
                    self.counters.stale += 1;
                    return;
                }
                self.repeat(key, &error, output, admission);
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
        if !self.sockets.current(family, id) {
            self.counters.stale += 1;
            return;
        }
        // Worker identity rejects queued events from a replaced socket. If the kernel later reuses the ping
        // identifier, a packet delivered to the current socket is indistinguishable here; the remote and
        // translated sequence below still have to name a live session.
        // The identifier in the reply is the kernel's own, which is what it demultiplexed on to reach this
        // socket at all, so it identifies nothing further and is not compared against anything.
        let (reply, payload) = match echo_wire::peek_reply(&message, family.ipv6()) {
            Ok(parsed) => parsed,
            Err(e) => {
                // Printed once with the bytes that failed, because a bare count here is not diagnosable: the
                // difference between a message the kernel truncated, one of a type this does not handle, and a
                // genuinely corrupt one is invisible in the total, and all three arrive as the same counter.
                if !self.reported_unparseable {
                    self.reported_unparseable = true;
                    let head = &message[..message.len().min(ECHO_HEADER_LEN)];
                    report::stdout!(
                        "echo cannot parse a {family} reply, and later ones are counted only: {e:?}, \
                         {} bytes starting {head:02x?}",
                        message.len()
                    );
                }
                self.counters.unparseable += 1;
                return;
            }
        };
        let Some(session) = self.sessions.take(remote.ip(), reply.sequence) else {
            // A duplicate of a reply already restored, one for a session that timed out, or one from a remote
            // this daemon never sent to. Consuming the session on the first reply is what makes the second of
            // those indistinguishable from the third, which is the safe direction to be wrong in.
            self.counters.unmatched += 1;
            return;
        };
        admission.shrink(&self.tables, Grant::records(1, Class::General));
        // Relayed traffic preserves what arrived rather than substituting a local default, and this daemon is
        // one hop, so a reply whose remaining hop limit dies here dies here.
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            self.counters.expired += 1;
            return;
        };
        output.echo(
            remote.ip(),
            session.client,
            hop_limit,
            session.identity,
            payload,
        );
        self.counters.written += 1;
    }

    /// The next ping socket to have finished, which is the only thing that removes one.
    pub(crate) async fn finished(&mut self) -> Terminal<Family> {
        self.sockets.finished().await
    }

    /// Settles one ping socket whose receive task has finished, whether it failed on its own or was cancelled.
    pub(crate) fn closed(&mut self, terminal: Terminal<Family>, admission: &mut Admission) {
        let Terminal { key, id, ended } = terminal;
        match ended {
            Ended::Expected => {}
            // once per socket rather than once per packet
            Ended::Reported(reason) => report::stdout!("echo {key} socket closed: {reason}"),
            Ended::Failed { context, error } => {
                report::io_with_details(context, error, [("family", key)])
            }
        }
        if !self.sockets.close(key, id, admission) {
            self.counters.stale += 1;
        }
    }

    /// Repeats one ICMP error a router sent about a relayed ping.
    fn repeat(
        &mut self,
        family: Family,
        error: &Reported,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        let Ok((quoted, _)) = echo_wire::peek_request(error.quoted.as_slice(), family.ipv6())
        else {
            self.counters.unparseable += 1;
            return;
        };
        let (remote, session) = match self.sessions.take_by_sequence(quoted.sequence) {
            Found::One { remote, session } => (remote, session),
            Found::Ambiguous => {
                self.counters.ambiguous += 1;
                return;
            }
            Found::Missing => {
                self.counters.unmatched += 1;
                return;
            }
        };
        admission.shrink(&self.tables, Grant::records(1, Class::General));
        if remote.is_ipv6() != family.ipv6() || error.remote.is_ipv6() != family.ipv6() {
            self.counters.implausible += 1;
            return;
        }
        let reason = match icmp_translate::translate(
            family.ipv6(),
            error.icmp_type,
            error.code,
            error.info,
            // The matched session identifies the request itself, which is the strongest proof there is here.
            Correlation::Datagram {
                hop_limit: session.hop_limit,
            },
        ) {
            Ok(reason) => reason,
            Err(Untranslatable::Implausible) => {
                self.counters.implausible += 1;
                return;
            }
            Err(_) => {
                self.counters.untranslated += 1;
                return;
            }
        };
        // Quoted as the client's own request, and sourced from the router that complained rather than from the
        // interface - the same rule the UDP path follows, and for the same reason.
        let invoking = match echo_wire::build_request_packet(
            session.client,
            remote,
            session.hop_limit,
            session.identity,
            &[],
        ) {
            Ok(invoking) => invoking,
            Err(_) => {
                self.counters.implausible += 1;
                return;
            }
        };
        match icmp_error::build(error.remote, &invoking, reason) {
            Ok(packet) => {
                self.counters.translated += 1;
                output.packet(packet);
            }
            Err(_) => self.counters.implausible += 1,
        }
    }

    /// Retires sessions that timed out. Unlike a UDP mapping a session holds no descriptor, so there is nothing
    /// to join and the refund happens here rather than following a task's completion.
    pub(crate) fn sweep(&mut self, admission: &mut Admission) {
        let expired = self.sessions.expire(Instant::now());
        if expired > 0 {
            // Records only. The table's charge covers the bound it was prepared for, whatever is in it.
            admission.shrink(&self.tables, Grant::records(expired as u32, Class::General));
            self.counters.swept += expired as u64;
        }
    }

    /// The earliest deadline in the table, which is what the owning task sleeps until. None means there is
    /// nothing to expire, not that expiry is off.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.sessions.next_deadline()
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} sessions over {} sockets, {}",
            self.sessions.len(),
            self.sockets.len(),
            self.counters.describe()
        )
    }
}
