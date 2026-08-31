use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use socket2::Socket;
use tokio::sync::mpsc;
use vpnhotspotd::shared::echo_session::{Found, Sessions};
use vpnhotspotd::shared::echo_wire::{self, Request, ECHO_HEADER_LEN};
use vpnhotspotd::shared::icmp_error::{self, Reason};
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::icmp_translate::{self, Correlation, Reported, Untranslatable};
use vpnhotspotd::shared::workers::{Ended, Terminal};

use crate::report;
use crate::shizuku::echo_socket::{Family, Refused, Sockets};
use crate::shizuku::egress::{self, Fragmentation};
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::reply::Event;
use crate::shizuku::send_failure::{self, Failure};
use vpnhotspotd::shared::admission::Admission;

/// Expected client and path outcomes are counters because whoever puts packets on the interface chooses them.
/// Every unexpected daemon-owned failure is handed to the shared reporter; its source-site coalescer bounds
/// pending diagnostics while preserving the latest summary.
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
    exhausted: u64,
    unmatched: u64,
    unparseable: u64,
    translated: u64,
    untranslated: u64,
    ambiguous: u64,
    implausible: u64,
    stale: u64,
    swept: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "sent {} queued {} unqueued {} denied {} expired {} too-big {} blocked {} \
             unreachable {} send-failed {} reported {} unreported {} df-failed {} open-failed {} \
             exhausted {} unmatched {} unparseable {} translated {} untranslated {} ambiguous {} \
             implausible {} stale {} swept {}",
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
            self.exhausted,
            self.unmatched,
            self.unparseable,
            self.translated,
            self.untranslated,
            self.ambiguous,
            self.implausible,
            self.stale,
            self.swept
        )
    }
}

pub(crate) struct Relay {
    sockets: Sockets,
    sessions: Sessions,
    /// The one error-queue scratch this owner lends to its send-failure path - held rather than built per
    /// failure, for the same reason [crate::shizuku::udp::Relay] holds one.
    errors: egress::ErrorQueue,
    counters: Counters,
}

impl Relay {
    pub(crate) fn new() -> (Self, mpsc::Receiver<Event<Family>>) {
        let (sockets, receiver) = Sockets::new();
        (
            Self {
                sockets,
                sessions: Sessions::default(),
                errors: egress::ErrorQueue::new(),
                counters: Counters::default(),
            },
            receiver,
        )
    }

    /// Releases every memory-only session after the family sockets have been settled.
    pub(crate) fn release(self, echoes: mpsc::Receiver<Event<Family>>) {
        self.sockets.release(echoes);
        drop(self.sessions);
        drop(self.errors);
    }

    /// Drops every session, cancels every socket, and joins every receive task.
    pub(crate) async fn shutdown(&mut self, admission: &mut Admission) {
        self.sessions.clear();
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
                // Apply this request's IPv4 DF policy; no await can interleave another client before send.
                // IPv6 policy is fixed when the socket opens.
                if request.dont_fragment {
                    Fragmentation::Prohibited
                } else {
                    Fragmentation::Permitted
                },
            ) {
                self.counters.df_failed += 1;
                // This daemon's own socket refusing this daemon's own option: not client-driven, and a
                // counter on its own says only that some ping did not leave. Reported like the UDP relay's
                // identical step; the shared reporter coalesces this source site while its writer is occupied.
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
        // Chosen before the send because it is what goes on the wire. Sessions are memory-only and grow
        // dynamically; descriptor admission belongs only to the family socket acquired above.
        let Some(sequence) = self.sessions.allocate(request.remote) else {
            self.counters.exhausted += 1;
            return;
        };
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
            Err(e) => {
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
                        report::message_with_details(
                            "shizuku.echo_path_mtu",
                            "echo send returned EMSGSIZE without an attributable local path MTU",
                            "unattributable",
                            [("queued_error", format!("{queued:?}"))],
                        );
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
                report::io("shizuku.echo_send", e);
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

    pub(crate) fn handle(&mut self, event: Event<Family>, now: Instant, output: &mut Output) {
        let (family, id, remote, hop_limit, message) = match event {
            Event::Error { key, id, error } => {
                if !self.sockets.current(key, id) {
                    self.counters.stale += 1;
                    return;
                }
                self.repeat(key, &error, now, output);
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
                // Include the bytes that failed because a bare count cannot distinguish truncation, a type
                // this owner does not handle, and corruption. Every occurrence reaches the shared reporter,
                // whose source-site coalescer retains the latest blocked summary.
                let head = &message[..message.len().min(ECHO_HEADER_LEN)];
                report::message_with_details(
                    "shizuku.echo_receive",
                    format!("cannot parse an Echo reply: {e:?}"),
                    "packet",
                    [
                        ("family", family.to_string()),
                        ("bytes", message.len().to_string()),
                        ("head", format!("{head:02x?}")),
                    ],
                );
                self.counters.unparseable += 1;
                return;
            }
        };
        let Some(session) = self.sessions.take(remote.ip(), reply.sequence, now) else {
            // A duplicate of a reply already restored, one for a session that timed out, or one from a remote
            // this daemon never sent to. Consuming the session on the first reply is what makes the second of
            // those indistinguishable from the third, which is the safe direction to be wrong in.
            self.counters.unmatched += 1;
            return;
        };
        // Relayed traffic preserves what arrived rather than substituting a local default, and this daemon is
        // one hop, so a reply whose remaining hop limit dies here dies here.
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            self.counters.expired += 1;
            return;
        };
        // Count actual handoff admission, not an attempted reply.
        if output.echo(
            now,
            remote.ip(),
            session.client,
            hop_limit,
            session.identity,
            payload,
        ) {
            self.counters.queued += 1;
        } else {
            self.counters.unqueued += 1;
        }
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
    fn repeat(&mut self, family: Family, error: &Reported, now: Instant, output: &mut Output) {
        let Ok((quoted, _)) = echo_wire::peek_request(error.quoted.as_slice(), family.ipv6())
        else {
            self.counters.unparseable += 1;
            return;
        };
        // Socket family scopes sequence reuse; lookup first expires rows that could create stale ambiguity.
        let (swept, found) = self.sessions.take_by_sequence(family, quoted.sequence, now);
        self.counters.swept += swept as u64;
        let (remote, session) = match found {
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
        // The indexed session already matches this socket family; validate the reporting router.
        if error.remote.is_ipv6() != family.ipv6() {
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

    /// Retires memory-only sessions that timed out. Unlike a UDP mapping, a session holds no descriptor and
    /// therefore has no admission grant to refund.
    pub(crate) fn sweep(&mut self) {
        let expired = self.sessions.expire(Instant::now());
        if expired > 0 {
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
