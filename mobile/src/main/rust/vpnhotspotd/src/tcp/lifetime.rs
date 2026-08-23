//! How long an idle terminated TCP flow lives in this daemon's own outer state.
//!
//! This is the outer, userspace half and nothing else. Android's inner IPv4 NAT keeps conntrack state of its
//! own for the same client, and none of it is mirrored, configured or timed from here: what this owns is the
//! flow record, its smoltcp socket and the worker behind them.
//!
//! Two floors, and RFC 5382 section 5's exact classification of which phase gets which. `FinWait1`,
//! `FinWait2` and `CloseWait` are *established* rather than transitory, because in each of them one
//! direction can still carry application data - a FIN in a header is not the connection being over, and a
//! client waiting out a long request it has finished sending is not idle. `TimeWait` is excluded from
//! REQ-5's transitory timeout by REQ-5 itself, and is left to smoltcp's own close timer - which in the
//! pinned `smoltcp 0.13.1` is `CLOSE_DELAY`, ten seconds, and not the two-minute 2MSL a Linux host waits.
//! Naming the real figure matters both ways: nothing here holds a flow for a conventional TIME-WAIT, and
//! nothing here shortens one either.
//!
//! These are idle floors rather than lifetimes: daemon-observable activity rearms the whole of the current
//! phase's floor. What counts as observable is narrower than a full TCP implementation's, deliberately, and
//! [Engine::rearm] says where the line is.
//!
//! **No post-RST retention is claimed.** RFC 7857 later recommends holding a mapping for four minutes after
//! a matching RST, which would need state outliving the live flow entirely. `Closed` is terminal here, and a
//! reset - the client's or this daemon's - ends the flow rather than starting a tombstone.

use std::time::{Duration, Instant};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{Socket, State};

use super::{identity, Engine};
use crate::output::Output;

/// RFC 5382 REQ-5's floor for an established connection: two hours four minutes.
const ESTABLISHED: Duration = Duration::from_secs(7_440);

/// RFC 5382 REQ-5's floor for a transitory one - partially open, or partially closed: four minutes.
const TRANSITORY: Duration = Duration::from_secs(240);

/// How long a flow in this phase may stay idle, or `None` where this owner has no say at all.
///
/// Exhaustive and without a wildcard, deliberately: a state smoltcp adds is a phase this table has no
/// opinion about yet, and a compile error is the only honest way to be told so.
fn floor(state: State) -> Option<Duration> {
    match state {
        // Partially open. A listening socket is a flow whose SYN has not reached the stack yet, which is the
        // same "no connection has been made" the transitory floor is for.
        State::Listen | State::SynSent | State::SynReceived => Some(TRANSITORY),
        // Established, and the three half-closed phases REQ-5 keeps with it because each can still carry
        // application data in one direction.
        State::Established | State::FinWait1 | State::FinWait2 | State::CloseWait => {
            Some(ESTABLISHED)
        }
        // Partially closed with nothing left to carry either way.
        State::Closing | State::LastAck => Some(TRANSITORY),
        // Named by REQ-5 as outside the transitory timeout, and outside this owner: the close wait is a
        // protocol timer and smoltcp holds it - ten seconds in the pinned version, `CLOSE_DELAY`, rather than
        // a Linux host's 2MSL. [Engine::next_deadline] still schedules the wake it asks for.
        State::TimeWait => None,
        // Terminal. The poll that reached this state has already begun the flow's retirement, so no rearm
        // ever writes this value - and a zero floor is what says this owner would not have waited either.
        State::Closed => Some(Duration::ZERO),
    }
}

/// When a flow in this phase next falls idle, measured from the activity that was just observed.
pub(super) fn deadline(now: Instant, state: State) -> Option<Instant> {
    floor(state).map(|floor| now + floor)
}

/// Whether this phase is one a connection can only be in *after* its handshake completed.
///
/// The question [crate::tcp::Flow]'s `established` answers, and it has to be asked of the phase rather than
/// of one state: smoltcp accepts a third-handshake ACK that also carries FIN and goes `SYN-RECEIVED` ->
/// `CLOSE-WAIT` in a single step, without `ESTABLISHED` ever being observable (smoltcp-0.13.1,
/// `src/socket/tcp.rs:1880-1886`). A flow that watched only for `ESTABLISHED` therefore never learned it was
/// open, never propagated the client's half-close to its upstream, and sat on the established floor with a
/// peer waiting for bytes that would never come.
///
/// Exhaustive and without a wildcard, for the same reason [floor] is: a state smoltcp adds has to be
/// classified here rather than fall silently to one side.
pub(super) fn opened(state: State) -> bool {
    match state {
        State::Listen | State::SynSent | State::SynReceived | State::Closed => false,
        State::Established
        | State::FinWait1
        | State::FinWait2
        | State::CloseWait
        | State::Closing
        | State::LastAck
        | State::TimeWait => true,
    }
}

/// Whether this phase cannot send *yet* - as opposed to cannot send *any more*.
///
/// smoltcp answers one question with `may_send`, which is false for both, and the difference decides what an
/// owner does with bytes it is holding for this client. A half that has not finished handshaking will be able
/// to take them, so they are kept and offered again; a half whose send side is over never will, so they belong
/// on the retirement path. Collapsing the two is how a payload gets dropped on the floor - or worse, consumed
/// and acknowledged to a producer whose bytes never reached anyone - see [crate::tcp::Engine::pump_to_client].
///
/// `Closed` is deliberately on the *later* side, unlike in [opened] where it groups with the states before a
/// handshake: a closed socket has no handshake left to finish, and holding a payload for one would hold it for
/// ever. Exhaustive and without a wildcard for the same reason [floor] is: a state smoltcp adds has to be
/// classified here rather than fall silently to one side.
pub(super) fn handshaking(state: State) -> bool {
    match state {
        State::Listen | State::SynSent | State::SynReceived => true,
        State::Established
        | State::FinWait1
        | State::FinWait2
        | State::CloseWait
        | State::Closing
        | State::LastAck
        | State::TimeWait
        | State::Closed => false,
    }
}

impl Engine {
    /// Rearms one exact flow from the state its socket is really in.
    ///
    /// Called only after the stack has been polled, because the phase a packet or a payload puts a flow into
    /// is where the socket *ends up*, not where it was when the bytes arrived: a client's final handshake ACK
    /// is observed on a socket that is already `Established`.
    ///
    /// What counts as observable is narrower than a TCP implementation's, and the difference is stated rather
    /// than hidden. The boundary is **offered to smoltcp for this exact live flow**, not accepted by it:
    /// [vpnhotspotd::shared::tcp_wire::peek] reads the four-tuple, the hop limit and the SYN bit and nothing
    /// else, so a segment smoltcp goes on to discard - a bad checksum, a sequence outside the window - rearms
    /// anyway. Telling those apart would mean a second TCP implementation beside the one the packet was just
    /// handed to, which is a worse trade than a client holding its own connection open with segments it is
    /// already free to send.
    ///
    /// What is refused *before* the stack sees it moves nothing, and neither does anything this daemon
    /// produced for itself: a packet the ingress parse rejected, one the device would not take because the
    /// previous one had not been consumed, one naming no live flow, output the stack generated,
    /// acknowledgements and resets this daemon originated, a config being applied, and anything naming a
    /// stale, cancelled or absent flow.
    pub(super) fn rearm(&mut self, handle: SocketHandle, worker: u64, now: Instant) {
        // Both halves, because smoltcp reuses handles: a packet or a marker naming a replaced flow's handle
        // would otherwise hand the successor its predecessor's lease of life.
        if !self.flows.current(&handle, worker) {
            return;
        }
        // Already retiring and waiting only on its worker. A refreshed deadline would outlive the record it
        // belongs to, and [Engine::next_deadline] excludes it from the schedule anyway.
        if self
            .flows
            .get(&handle)
            .is_some_and(|held| held.cancel.is_cancelled())
        {
            return;
        }
        let armed = deadline(now, self.sockets.get::<Socket>(handle).state());
        if let Some(held) = self.flows.get_mut(&handle) {
            held.record.deadline = armed;
        }
    }

    /// When this engine next needs to run regardless of traffic.
    ///
    /// Two sources and one answer: smoltcp's own protocol timers - retransmission, delayed acknowledgement
    /// and the ten-second close wait it owns for `TIME-WAIT` - and the earliest outer idle deadline any live
    /// flow holds.
    ///
    /// A cancelled flow is excluded, and that is load-bearing rather than tidy. Cancelling does not remove
    /// one - what removes it is whichever of its two endings applies: an attached flow leaves when its worker
    /// finishes, so that the refund lands when the descriptor actually closes, and a *detached* one has no
    /// worker left and leaves when this owner's own scan finds its client finished (see
    /// [Engine::detached]). Either way a flow just retired for being idle would otherwise keep its passed
    /// deadline as the earliest in the table and spin the owner's select loop until that ending arrived.
    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        let stack = self
            .interface
            .poll_delay(self.now(), &self.sockets)
            // Added to the runtime's own reading of now, because that is the clock the owner's
            // `sleep_until` measures this against - see [crate::tun_reader::run]. Outside a test harness it
            // is `std::time::Instant::now()`: tokio's test clock only exists when the `test-util` feature is
            // built, which is a dev-dependency and so never unified into the daemon binary.
            .map(|delay| {
                tokio::time::Instant::now().into_std() + Duration::from_micros(delay.total_micros())
            });
        let idle = self
            .flows
            .values()
            .filter(|held| !held.cancel.is_cancelled())
            .filter_map(|held| held.record.deadline)
            .min();
        [stack, idle].into_iter().flatten().min()
    }

    /// Retires every flow whose outer idle deadline has passed.
    ///
    /// Exactly the sequence [Engine::retire] uses, per exact identity rather than by axis, with the one
    /// difference that matters: the engine-wide sweep token is untouched. That token is what
    /// [crate::tcp_flow::splice] reads to close its upstream with `SO_LINGER(0)`, and a flow that fell idle
    /// is not a network being left - its upstream closes the ordinary way.
    ///
    /// Nothing is removed or refunded here. The flow keeps its record, its socket and its charge until its
    /// own ending arrives, and which ending that is depends on whether it still has a worker: an attached
    /// flow waits for that worker's terminal through [Engine::close], the join fence every other ending goes
    /// through, while a detached one has no terminal coming and is settled by this owner's own scan through
    /// [Engine::settled]. Repeated ticks are idempotent either way: a flow already on its way out is
    /// skipped.
    pub(crate) fn expire(&mut self, now: Instant, output: &mut Output) {
        // Walked over the round-robin order rather than into a list of what is due. That order is registered
        // with every admitted flow and deregistered with every closed one, so it already holds each live
        // handle exactly once - see [vpnhotspotd::shared::fair::register] - and a list built here would be
        // scratch sized by traffic that no lease covers, allocated on a path a stopping session still runs.
        // Destructured because the walk reads one field while the steps below write four others.
        let expired = {
            let Engine {
                flows,
                sockets,
                fair,
                outgoing,
                counters,
                ..
            } = self;
            debug_assert_eq!(
                outgoing.len(),
                flows.len(),
                "the round-robin order indexes exactly the live flows"
            );
            let mut expired = 0u64;
            for handle in outgoing.iter() {
                let Some(held) = flows.get_mut(handle) else {
                    continue;
                };
                // A flow already on its way out - by an earlier tick, by a config, or by its own socket
                // closing - is skipped rather than begun again, which is what makes a repeated tick add
                // nothing and what keeps a config retirement from aborting a socket this already closed.
                // A *detached* flow is not on its way out and is not skipped: it has no worker left, so its
                // floor is the only thing that can still end it, and it is settled by the owner's own scan
                // rather than by a terminal it will never produce.
                if held.cancel.is_cancelled()
                    || !held.record.deadline.is_some_and(|deadline| deadline <= now)
                {
                    continue;
                }
                // Discard before cancel, and per exact identity: a worker parked on an acknowledgment may
                // only be released once the owner has committed to dropping what that acknowledgment was for.
                drop(fair.begin_retire(identity(*handle, held.record.worker)));
                held.cancel.cancel();
                // dropped so a task blocked on the client's half of the splice wakes and exits
                held.record.downstream = None;
                // At most one reset per expired flow, built while the socket that carries it still exists,
                // so a client fails fast instead of waiting out its own retransmissions. Counted only where the
                // stack really has somewhere to send one: a socket with no remote endpoint - one still
                // listening, or one already closed - is aborted silently, and counting that would overstate
                // what was sent.
                let socket = sockets.get_mut::<Socket>(*handle);
                if socket.remote_endpoint().is_some() {
                    counters.reset += 1;
                }
                socket.abort();
                expired += 1;
            }
            counters.expired += expired;
            expired
        };
        if expired == 0 {
            return;
        }
        // Under the stamp current now and before anything is freed, for the same reason [Engine::retire]
        // polls here: a reset is a packet the stack has not built yet, and removing the socket first would
        // abort the connection and tell the client nothing. Whether it reaches the wire is the writer's
        // ordinary business - a config that changes the stamp before the writer dequeues it purges this
        // packet exactly as it purges every other one of the retired stamp.
        self.poll(output);
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::Arc;

    use smoltcp::wire::{TcpControl, TcpRepr, TcpSeqNumber};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use vpnhotspotd::shared::admission::Admission;
    use vpnhotspotd::shared::tcp_wire;

    use super::*;
    // One set of fixtures, shared with the engine's own tests rather than rebuilt here: a session built the
    // way a session is built, an aggregate solved for an exact prepared bound, and the gate that stands in
    // for an upstream a host cannot open.
    use crate::tcp::tests::{
        admission_for, client, parse, segment, session, Session, Wire, DESTINATION, MTU, RESOLVER,
    };
    use crate::tcp::{Finished, Gate};
    use crate::tun_writer::Stamp;

    /// Every floor this table may produce, spelled out where a reader can see the numbers.
    ///
    /// One entry per state smoltcp has. Exhaustiveness is proved twice over: [floor] itself has no wildcard
    /// arm, so a state smoltcp adds is a compile error in production, and [index] below has none either, so
    /// this table cannot quietly omit one of the states that already exist.
    const TABLE: [(State, Option<Duration>); 11] = [
        (State::Closed, Some(Duration::ZERO)),
        (State::Listen, Some(Duration::from_secs(240))),
        (State::SynSent, Some(Duration::from_secs(240))),
        (State::SynReceived, Some(Duration::from_secs(240))),
        (State::Established, Some(Duration::from_secs(7_440))),
        (State::FinWait1, Some(Duration::from_secs(7_440))),
        (State::FinWait2, Some(Duration::from_secs(7_440))),
        (State::CloseWait, Some(Duration::from_secs(7_440))),
        (State::Closing, Some(Duration::from_secs(240))),
        (State::LastAck, Some(Duration::from_secs(240))),
        (State::TimeWait, None),
    ];

    /// Where each state belongs in [TABLE]. No wildcard, so the table is a bijection onto smoltcp's states
    /// rather than a sample of them.
    fn index(state: State) -> usize {
        match state {
            State::Closed => 0,
            State::Listen => 1,
            State::SynSent => 2,
            State::SynReceived => 3,
            State::Established => 4,
            State::FinWait1 => 5,
            State::FinWait2 => 6,
            State::CloseWait => 7,
            State::Closing => 8,
            State::LastAck => 9,
            State::TimeWait => 10,
        }
    }

    /// The classification is RFC 5382 section 5's, at the literal durations REQ-5 names.
    ///
    /// Literal rather than written from the constants, which is the whole point: a floor edited to some
    /// other number, or a half-closed state moved out of the established bucket, has to fail here.
    #[test]
    fn every_state_carries_rfc_5382s_own_floor() {
        for (position, (state, expected)) in TABLE.into_iter().enumerate() {
            assert_eq!(index(state), position, "{state} is in the table once");
            assert_eq!(floor(state), expected, "{state}");
        }
    }

    /// One client on the far side of the TUN, tracking only what it needs to build its next segment.
    struct Peer {
        source: SocketAddr,
        destination: SocketAddr,
        /// The next sequence number this client will send.
        seq: TcpSeqNumber,
        /// What it has been told about the engine's sequence, which is what it acknowledges. Advanced only
        /// by reading the engine's output, so a test that never reads a FIN has a client that cannot
        /// acknowledge one - which is exactly how a simultaneous close is arranged.
        ack: TcpSeqNumber,
    }

    /// One terminated flow, driven end to end: a real session, a real client, and - where the test needs a
    /// remote that can finish sending first - a real loopback upstream behind the gate.
    struct Terminating {
        session: Session,
        peer: Peer,
        handle: SocketHandle,
        worker: u64,
    }

    impl Terminating {
        /// Opens one flow the way a client opens one: a SYN through [Engine::accept], with the destination
        /// the dispatcher would have classified.
        fn opened(
            session: Session,
            admission: &mut Admission,
            port: u16,
            destination: SocketAddr,
            now: Instant,
        ) -> Self {
            let mut terminating = Self {
                session,
                peer: Peer {
                    source: client(port),
                    destination,
                    seq: TcpSeqNumber(1_000),
                    ack: TcpSeqNumber(0),
                },
                handle: SocketHandle::default(),
                worker: 0,
            };
            terminating.syn(admission, now);
            terminating
        }

        /// A whole session and one flow on it, for the tests that need only one.
        async fn one(
            admission: &mut Admission,
            gate: &Arc<Gate>,
            port: u16,
            destination: SocketAddr,
            now: Instant,
        ) -> Self {
            Self::opened(
                stamped(admission, gate).await,
                admission,
                port,
                destination,
                now,
            )
        }

        /// Sends the current client's SYN and moves this harness's cursor onto the flow it opened.
        ///
        /// Found by the client address rather than by "the one handle in the table", so a session carrying
        /// several connections names each of them the way the engine does.
        fn syn(&mut self, admission: &mut Admission, now: Instant) {
            self.deliver(admission, TcpControl::Syn, false, &[], now);
            let (handle, worker) = self
                .session
                .engine
                .flows
                .iter()
                .find(|(_, held)| held.record.client == self.peer.source)
                .map(|(handle, held)| (*handle, held.record.worker))
                .expect("the SYN opened a flow");
            self.handle = handle;
            self.worker = worker;
        }

        /// Opens and establishes one more connection on the same session, and answers its exact identity.
        ///
        /// The harness's cursor moves to it: the peer, the handle and the worker are all "whichever
        /// connection the client is driving now". A test that needs to look back at an earlier one keeps the
        /// identity this answers and reads it through [Terminating::deadline_of].
        fn establish_another(
            &mut self,
            admission: &mut Admission,
            port: u16,
            now: Instant,
        ) -> (SocketHandle, u64) {
            self.peer = Peer {
                source: client(port),
                destination: self.peer.destination,
                seq: TcpSeqNumber(1_000),
                ack: TcpSeqNumber(0),
            };
            self.syn(admission, now);
            self.establish(admission, now);
            (self.handle, self.worker)
        }

        fn state(&self) -> State {
            self.session
                .engine
                .sockets
                .get::<Socket>(self.handle)
                .state()
        }

        fn deadline(&self) -> Option<Instant> {
            self.deadline_of(self.handle)
        }

        fn deadline_of(&self, handle: SocketHandle) -> Option<Instant> {
            self.session
                .engine
                .flows
                .get(&handle)
                .expect("the flow is still held")
                .record
                .deadline
        }

        /// Builds one segment from this client and hands it to the engine.
        fn deliver(
            &mut self,
            admission: &mut Admission,
            control: TcpControl,
            acking: bool,
            payload: &[u8],
            now: Instant,
        ) {
            let bytes = self.build(control, acking, payload);
            self.offer(admission, &bytes, now);
        }

        /// One segment from this client as bytes, with the sequence space it consumes already taken.
        fn build(&mut self, control: TcpControl, acking: bool, payload: &[u8]) -> Vec<u8> {
            let repr = TcpRepr {
                src_port: self.peer.source.port(),
                dst_port: self.peer.destination.port(),
                control,
                seq_number: self.peer.seq,
                ack_number: acking.then_some(self.peer.ack),
                window_len: 32_768,
                window_scale: None,
                max_seg_size: matches!(control, TcpControl::Syn).then_some(1_400),
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload,
            };
            self.peer.seq += payload.len() + control.len();
            segment(self.peer.source, self.peer.destination, repr)
        }

        /// Hands bytes to the engine exactly as the dispatcher does: parsed by the same peek, through the
        /// same entry point, with the same injected instant.
        fn offer(&mut self, admission: &mut Admission, bytes: &[u8], now: Instant) {
            let peeked = tcp_wire::peek(bytes).expect("the dispatcher's own parse accepts it");
            let resolver = self.peer.destination != DESTINATION;
            self.session.engine.accept(
                bytes,
                peeked,
                resolver,
                now,
                &mut self.session.output,
                admission,
            );
        }

        /// Everything the engine has queued for the wire since this was last called, with the client's
        /// acknowledgement advanced over it. Reading is what teaches the client the engine's sequence, so a
        /// test that wants a client which has not seen a FIN simply does not call this.
        fn drain(&mut self) -> Vec<(Stamp, Wire)> {
            let mut seen = Vec::new();
            while let Some((stamp, bytes)) = self.session.queue.dequeue() {
                let wire = parse(&bytes);
                self.peer.ack = wire.acknowledging;
                seen.push((stamp, wire));
            }
            seen
        }

        /// The handshake, from the SYN [Terminating::syn] already sent.
        fn establish(&mut self, admission: &mut Admission, now: Instant) {
            let seen = self.drain();
            assert_eq!(
                seen.last().expect("a SYN-ACK").1.control,
                TcpControl::Syn,
                "the stack answers the SYN"
            );
            self.deliver(admission, TcpControl::None, true, &[], now);
            assert_eq!(self.state(), State::Established);
        }

        /// Drives the current connection to a *detached* flow in `TIME-WAIT`: the remote finishes sending,
        /// this daemon closes its own half, the client acknowledges and closes too, and the worker's clean
        /// terminal hands the flow on rather than ending it.
        async fn detach_in_time_wait(
            &mut self,
            admission: &mut Admission,
            remote: &mut tokio::net::TcpStream,
            now: Instant,
        ) {
            remote
                .shutdown()
                .await
                .expect("the remote finishes sending");
            self.worker_event(true, now).await;
            self.drain();
            self.deliver(admission, TcpControl::None, true, &[], now);
            self.deliver(admission, TcpControl::Fin, true, &[], now);
            assert_eq!(self.state(), State::TimeWait);
            assert_eq!(
                self.deadline(),
                None,
                "the close timer is smoltcp's, so this holds no outer floor at all"
            );
            let Finished::Flow(terminal) = self.session.engine.finished().await else {
                panic!("the flow's own worker is what finished")
            };
            self.session
                .engine
                .close(terminal, admission, &mut self.session.output);
        }

        /// One readiness marker, handed to the engine the way the ingress task's own arm hands it over.
        ///
        /// Awaited rather than polled: parking here is what lets the flow's worker and the runtime's I/O
        /// driver run, so no test in this module has to guess how many yields a loopback round trip takes.
        async fn worker_event(&mut self, admitting: bool, now: Instant) {
            let event = self
                .session
                .markers
                .recv()
                .await
                .expect("a worker produced a readiness marker");
            self.session
                .engine
                .handle(event, admitting, now, &mut self.session.output);
        }
    }

    /// A session with a stamp that is not the default already adopted, so every assertion about what a
    /// packet was written under is about a real value rather than about zero.
    async fn stamped(admission: &mut Admission, gate: &Arc<Gate>) -> Session {
        let mut session = session(admission, gate).await;
        session
            .engine
            .apply(STAMP, Some(1), MTU, admission, &mut session.output)
            .await;
        session
    }

    /// A gate that opens onto a real listener, which is the only way a host can give a flow a remote that
    /// finishes sending first.
    async fn opening_gate() -> (Arc<Gate>, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a loopback listener");
        let gate = Arc::new(Gate {
            opens_onto: Some(listener.local_addr().expect("bound")),
            ..Gate::default()
        });
        (gate, listener)
    }

    const STAMP: Stamp = Stamp {
        generation: 7,
        epoch: 3,
    };

    /// Upstream bytes that arrive before the client's final ACK are held rather than consumed, and go out
    /// exactly once when the handshake completes.
    ///
    /// The window is real and not narrow: this daemon connects out at the *SYN*, because holding the client's
    /// SYN would need the stack to defer a segment it has already been given - see the Step 8 notes. So a
    /// remote that greets first, as SSH and SMTP both do, can have bytes in the mailbox while the client half
    /// is still `SYN-RECEIVED`.
    ///
    /// What that used to cost: `pump_to_client` took the readiness marker off the order, tried `send_slice`,
    /// got `InvalidState` from smoltcp - which refuses to send in any state but `ESTABLISHED` and `CLOSE-WAIT`
    /// (smoltcp-0.13.1, `src/socket/tcp.rs:1162-1170`, `1222-1228`) - and continued. The payload stayed owned
    /// and its worker stayed parked on an acknowledgment, but the marker that was the only thing left to offer
    /// it again was gone, so those bytes could never be sent and that flow could never carry another. The
    /// guard keeps the marker and consumes nothing until the half can take it.
    #[tokio::test]
    async fn an_early_upstream_greeting_waits_for_the_handshake_and_then_sends_once() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let session = stamped(&mut admission, &gate).await;
        let opened = Instant::now();

        // The SYN opens the flow and connects the upstream. The client's final ACK deliberately does not
        // follow, so its half stays where a real one is for a whole round trip.
        let mut flow = Terminating::opened(session, &mut admission, 10_190, DESTINATION, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");
        assert_eq!(flow.state(), State::SynReceived);

        let greeting = b"220 ready\r\n";
        remote.write_all(greeting).await.expect("the remote greets");
        flow.worker_event(true, opened).await;

        let id = identity(flow.handle, flow.worker);
        assert!(
            flow.session.engine.fair.owes(id),
            "the greeting is still owed: a handshaking half cannot take it"
        );
        assert_eq!(
            flow.session.engine.fair.ready_len(),
            1,
            "and its marker is still queued, which is the only thing that will offer it again"
        );

        // The handshake, with the SYN-ACK read back - and nothing carrying payload before it.
        let seen = flow.drain();
        assert_eq!(
            seen.last().expect("a SYN-ACK").1.control,
            TcpControl::Syn,
            "the stack answers the SYN"
        );
        assert!(
            seen.iter().all(|(_, wire)| wire.payload.is_empty()),
            "no payload reached the client before its handshake finished"
        );
        flow.deliver(&mut admission, TcpControl::None, true, &[], opened);
        assert_eq!(flow.state(), State::Established);

        // The ACK is what runs the pump again, and the greeting goes then - once.
        let delivered: Vec<u8> = flow
            .drain()
            .into_iter()
            .flat_map(|(_, wire)| wire.payload)
            .collect();
        assert_eq!(
            delivered, greeting,
            "the held greeting went, whole and once"
        );
        assert!(
            !flow.session.engine.fair.owes(id),
            "and is no longer owed by anyone"
        );
        assert_eq!(flow.session.engine.fair.ready_len(), 0, "marker consumed");

        // And the flow is left carrying on rather than wedged: still held, still open, with its mailbox free
        // for the next chunk. How many yields a loopback read takes is the runtime's business, so what is
        // asserted is the state this owner is in and not a second round trip.
        assert!(
            flow.session.engine.fair.accepts(id),
            "the mailbox is free, so its producer may read again"
        );
        assert_eq!(flow.state(), State::Established);
        assert_eq!(flow.session.engine.flows.len(), 1, "and the flow is held");
        drop(remote);
    }

    /// An ordered end of stream that arrives before the client's final ACK is not acknowledged early either.
    ///
    /// The same guard, on the branch with no payload at all. Consuming it early told the producer its
    /// half-close had been delivered while the client had not even finished connecting - and the worker's
    /// clean terminal then reclaimed a flow that had never opened, so the client got a reset instead of the
    /// ordered close the remote actually performed.
    #[tokio::test]
    async fn an_early_end_of_stream_waits_for_the_handshake_before_it_is_acknowledged() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let session = stamped(&mut admission, &gate).await;
        let opened = Instant::now();

        let mut flow = Terminating::opened(session, &mut admission, 10_191, DESTINATION, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");
        assert_eq!(flow.state(), State::SynReceived);

        // Nothing but a half-close, so the mailbox carries an ordered end of stream and no bytes.
        remote
            .shutdown()
            .await
            .expect("the remote finishes sending");
        flow.worker_event(true, opened).await;

        let id = identity(flow.handle, flow.worker);
        assert!(
            flow.session.engine.fair.owes(id),
            "the end of stream is still owed rather than acknowledged"
        );
        assert_eq!(flow.session.engine.fair.ready_len(), 1, "marker kept");
        let seen = flow.drain();
        assert_eq!(seen.last().expect("a SYN-ACK").1.control, TcpControl::Syn);
        assert!(
            seen.iter().all(|(_, wire)| wire.control != TcpControl::Fin),
            "and no FIN was emitted before the handshake finished"
        );

        // The handshake completes, and only then is the close propagated.
        flow.deliver(&mut admission, TcpControl::None, true, &[], opened);
        flow.session.engine.poll(&mut flow.session.output);
        assert!(
            flow.drain()
                .iter()
                .any(|(_, wire)| wire.control == TcpControl::Fin),
            "the client is told the remote finished, in order"
        );
        assert!(
            !flow.session.engine.fair.owes(id),
            "and the end of stream is settled exactly once"
        );
    }

    /// A flow that has only been opened is transitory; the handshake completing is what buys it the
    /// established floor, and it buys the whole of it from the moment it completed.
    #[tokio::test]
    async fn the_handshake_is_what_moves_a_flow_onto_the_established_floor() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_000, DESTINATION, opened).await;

        assert_eq!(flow.state(), State::SynReceived);
        assert_eq!(
            flow.deadline(),
            Some(opened + Duration::from_secs(240)),
            "a half-open connection is transitory"
        );

        let established = opened + Duration::from_secs(60);
        flow.establish(&mut admission, established);
        assert_eq!(
            flow.deadline(),
            Some(established + Duration::from_secs(7_440)),
            "and the whole established floor runs from the acknowledgement that completed it"
        );
    }

    /// Activity one nanosecond before the floor runs out buys the whole floor again, measured from the
    /// activity rather than from what was left.
    #[tokio::test]
    async fn activity_at_the_last_moment_rearms_the_entire_floor() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_010, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let first = flow.deadline().expect("an established deadline");

        let late = first - Duration::from_nanos(1);
        flow.deliver(&mut admission, TcpControl::Psh, true, b"still here", late);
        assert_eq!(
            flow.deadline(),
            Some(late + Duration::from_secs(7_440)),
            "the floor is rearmed in full, not topped up to what it was"
        );

        // And the old deadline is now just a moment this flow lived through.
        flow.session.engine.expire(first, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.expired, 0);
        assert!(flow.session.engine.flows.contains(&flow.handle));
    }

    /// A client that half-closes is not idle: RFC 5382 keeps `CLOSE-WAIT` on the established floor because
    /// this daemon may still be sending. Only when its own half closes too does the flow become transitory.
    #[tokio::test]
    async fn a_client_half_close_keeps_the_established_floor_until_this_side_closes_too() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_020, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");

        let half_closed = opened + Duration::from_secs(1);
        flow.deliver(&mut admission, TcpControl::Fin, true, &[], half_closed);
        assert_eq!(flow.state(), State::CloseWait);
        assert_eq!(
            flow.deadline(),
            Some(half_closed + Duration::from_secs(7_440)),
            "one direction can still carry data, so this is established in REQ-5's sense"
        );

        // The client's half-close reaches the upstream as a shutdown, and the remote answers it with one.
        assert_eq!(
            remote.read(&mut [0u8; 8]).await.expect("readable"),
            0,
            "the upstream write half was shut down"
        );
        remote.shutdown().await.expect("the remote may close too");
        let last_ack = opened + Duration::from_secs(2);
        flow.worker_event(true, last_ack).await;
        assert_eq!(flow.state(), State::LastAck);
        assert_eq!(
            flow.deadline(),
            Some(last_ack + Duration::from_secs(240)),
            "with both halves finished there is nothing left to carry"
        );
    }

    /// A remote that finishes sending first leaves the *client* half open, which is the ordinary
    /// server-closes-first shape - and all three phases of it stay on the established floor.
    #[tokio::test]
    async fn a_remote_that_finishes_first_stays_established_until_time_wait() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_030, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");

        remote
            .shutdown()
            .await
            .expect("the remote finishes sending");
        let closing = opened + Duration::from_secs(1);
        flow.worker_event(true, closing).await;
        assert_eq!(flow.state(), State::FinWait1);
        assert_eq!(
            flow.deadline(),
            Some(closing + Duration::from_secs(7_440)),
            "the client may still be sending, so this is not transitory"
        );

        // Reading the engine's FIN is what lets the client acknowledge it.
        assert!(flow
            .drain()
            .iter()
            .any(|(_, wire)| wire.control == TcpControl::Fin));
        let acknowledged = opened + Duration::from_secs(2);
        flow.deliver(&mut admission, TcpControl::None, true, &[], acknowledged);
        assert_eq!(flow.state(), State::FinWait2);
        assert_eq!(
            flow.deadline(),
            Some(acknowledged + Duration::from_secs(7_440))
        );

        let finished = opened + Duration::from_secs(3);
        flow.deliver(&mut admission, TcpControl::Fin, true, &[], finished);
        assert_eq!(flow.state(), State::TimeWait);
        assert_eq!(
            flow.deadline(),
            None,
            "the close wait is a protocol timer, and smoltcp owns it"
        );
        // Which does not mean nothing wakes for it, and this is the stack's side of the same minimum: an
        // ordinary flow beside it holds a floor two hours away, and what the owner wakes for is smoltcp's
        // ten-second close wait rather than that.
        let ordinary = flow.establish_another(&mut admission, 10_031, finished).0;
        let wake = flow
            .session
            .engine
            .next_deadline()
            .expect("the stack still has a deadline");
        assert!(
            wake < flow.deadline_of(ordinary).expect("an established floor"),
            "the protocol timer is earlier than any floor, and the minimum is over both"
        );
    }

    /// A simultaneous close - both sides sending FIN before either acknowledges the other - is transitory,
    /// because by then neither direction can carry anything.
    #[tokio::test]
    async fn a_simultaneous_close_is_transitory() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_040, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");

        remote
            .shutdown()
            .await
            .expect("the remote finishes sending");
        flow.worker_event(true, opened).await;
        assert_eq!(flow.state(), State::FinWait1);
        // Deliberately not drained: a client that has not seen the engine's FIN cannot acknowledge it, which
        // is what makes the close below simultaneous rather than an ordinary answer to it.
        let closing = opened + Duration::from_secs(1);
        flow.deliver(&mut admission, TcpControl::Fin, true, &[], closing);
        assert_eq!(flow.state(), State::Closing);
        assert_eq!(flow.deadline(), Some(closing + Duration::from_secs(240)));
    }

    /// Only what this owner really observed on this exact flow moves its deadline.
    ///
    /// Five things that look like activity and are not, and one that is. A `STOPPING` session is among the
    /// five, and the payload it drains is what makes it a real distinction rather than a dropped chunk:
    /// the bytes reach the client and the lifetime still does not move.
    #[tokio::test]
    async fn only_observed_activity_on_the_exact_flow_moves_the_deadline() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_050, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");
        let armed = flow.deadline().expect("an established deadline");
        let later = opened + Duration::from_secs(600);

        // A marker naming a worker this handle no longer holds, *while the current flow's mailbox really has
        // something in it*. smoltcp reuses handles, so this is a predecessor's wake reaching its successor,
        // and validating only the handle would let it renew the successor's lifetime and drain the
        // successor's payload into the round - which is why the mailbox is loaded first rather than empty.
        remote
            .write_all(b"the successor's own")
            .await
            .expect("the remote may write");
        let marker = flow
            .session
            .markers
            .recv()
            .await
            .expect("a worker produced a readiness marker");
        flow.session.engine.handle(
            crate::tcp_flow::Event {
                handle: marker.handle,
                worker: marker.worker + 1,
            },
            true,
            later,
            &mut flow.session.output,
        );
        assert_eq!(flow.deadline(), Some(armed), "a stale identity");
        assert!(
            flow.drain().is_empty(),
            "and it did not put the successor's mailbox into the round either"
        );

        // The same marker, named correctly, does both.
        let taken = opened + Duration::from_secs(300);
        flow.session
            .engine
            .handle(marker, true, taken, &mut flow.session.output);
        assert!(
            flow.drain()
                .iter()
                .any(|(_, wire)| wire.payload == b"the successor's own"),
            "the exact identity is what delivers it"
        );
        // Acknowledged, because a client that never does leaves the stack holding everything after it - and
        // the same instant, so what this proves below is still about the deadline rather than about the ack.
        flow.deliver(&mut admission, TcpControl::None, true, &[], taken);
        let armed = taken + Duration::from_secs(7_440);
        assert_eq!(flow.deadline(), Some(armed));

        // The exact identity, with nothing in its mailbox: a wake this daemon produced for itself.
        flow.session.engine.handle(
            crate::tcp_flow::Event {
                handle: flow.handle,
                worker: flow.worker,
            },
            true,
            later,
            &mut flow.session.output,
        );
        assert_eq!(flow.deadline(), Some(armed), "an empty marker");

        // The same client from a port this session has no flow for. The stack answers it; this owner has
        // nothing to rearm, and in particular does not rearm the connection next to it.
        let kept = std::mem::replace(
            &mut flow.peer,
            Peer {
                source: client(10_059),
                destination: DESTINATION,
                seq: TcpSeqNumber(9_000),
                ack: TcpSeqNumber(9_000),
            },
        );
        flow.deliver(&mut admission, TcpControl::Psh, true, b"nobody", later);
        flow.peer = kept;
        assert_eq!(flow.deadline(), Some(armed), "a packet matching no flow");

        // Real payload, while the session has stopped serving. It drains - the client gets the bytes - and
        // the lifetime stays exactly where it was.
        remote
            .write_all(b"stopping")
            .await
            .expect("the remote may write");
        flow.worker_event(false, later).await;
        assert_eq!(flow.deadline(), Some(armed), "STOPPING creates no lifetime");
        assert!(
            flow.drain()
                .iter()
                .any(|(_, wire)| wire.payload == b"stopping"),
            "and the payload it already owned still reached the client"
        );

        // The same event while serving, which is the one thing here that is activity.
        remote
            .write_all(b"serving")
            .await
            .expect("the remote may write");
        let refreshed = opened + Duration::from_secs(700);
        flow.worker_event(true, refreshed).await;
        assert_eq!(
            flow.deadline(),
            Some(refreshed + Duration::from_secs(7_440)),
            "an upstream payload accepted into the fair owner is activity"
        );
    }

    /// The owner wakes for the earliest floor any *live* flow holds; one already retiring is not in the
    /// answer at all.
    #[tokio::test]
    async fn the_wake_is_the_earliest_live_floor() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_070, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let first = flow.handle;
        let later = opened + Duration::from_secs(30);
        let second = flow.establish_another(&mut admission, 10_071, later).0;
        assert!(
            flow.deadline_of(second) > flow.deadline_of(first),
            "the two floors differ, so choosing between them is a real choice"
        );

        assert_eq!(
            flow.session.engine.next_deadline(),
            flow.deadline_of(first),
            "with the stack quiet, the earliest floor is the whole answer"
        );

        // And a flow that has just been taken back is not in it: it is cancelled and waiting only to be
        // joined, so its passed deadline must not be what the owner wakes for.
        flow.session.engine.expire(
            flow.deadline_of(first).expect("an established floor"),
            &mut flow.session.output,
        );
        assert_eq!(
            flow.session.engine.next_deadline(),
            flow.deadline_of(second),
            "the retiring flow's passed deadline cannot spin the owner's loop"
        );
    }

    /// Several flows due at once retire in one pass, the pass is idempotent, and what was not due is
    /// untouched and becomes the next wake.
    #[tokio::test]
    async fn co_due_flows_retire_once_and_the_rest_advance_the_wake() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_080, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let first = flow.handle;
        let second = flow.establish_another(&mut admission, 10_081, opened).0;
        let later = opened + Duration::from_secs(120);
        let third = flow.establish_another(&mut admission, 10_082, later).0;
        let due = flow.deadline_of(first).expect("an established floor");
        assert_eq!(
            flow.deadline_of(second),
            Some(due),
            "co-due by construction"
        );
        flow.drain();

        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.expired, 2);
        let resets = flow.drain();
        assert_eq!(resets.len(), 2, "one terminal packet each");
        assert!(resets
            .iter()
            .all(|(_, wire)| wire.control == TcpControl::Rst));

        // Both are cancelled and waiting only to be joined, so neither is in the schedule any more - the
        // third is, and it is what the owner wakes for next.
        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(
            flow.session.engine.counters.expired, 2,
            "a repeated tick retires nothing twice"
        );
        assert!(flow.drain().is_empty(), "and writes nothing twice");
        assert_eq!(
            flow.session.engine.next_deadline(),
            flow.deadline_of(third),
            "a due record awaiting its join cannot be the next wake"
        );
    }

    /// The whole ending, through the owners that really do it: alive a nanosecond short of the floor, a
    /// reset on the wire at it, and the record, the descriptor and the charge released only once the
    /// worker has actually finished.
    #[tokio::test]
    async fn an_expiry_resets_the_client_and_refunds_only_after_the_join() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let session = stamped(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        let leases = admission.outstanding_leases();

        let opened = Instant::now();
        let mut flow = Terminating::opened(session, &mut admission, 10_090, DESTINATION, opened);
        flow.establish(&mut admission, opened);
        flow.drain();
        tokio::task::yield_now().await;
        assert_eq!(
            gate.entered.load(Relaxed),
            1,
            "the flow's worker is really parked on its upstream"
        );
        let due = flow.deadline().expect("an established floor");
        assert!(admission.bytes_charged() > idle, "the flow is charged for");

        flow.session
            .engine
            .expire(due - Duration::from_nanos(1), &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.expired, 0);
        assert!(flow.drain().is_empty(), "a nanosecond short is still alive");

        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.expired, 1);
        let resets = flow.drain();
        assert_eq!(resets.len(), 1);
        let (stamp, reset) = &resets[0];
        assert_eq!(reset.control, TcpControl::Rst, "the client is told");
        assert_eq!(reset.source, DESTINATION, "from the endpoint it dialled");
        assert_eq!(reset.destination, client(10_090));
        assert_eq!(
            *stamp, STAMP,
            "under the retirement the session is actually in"
        );

        // Nothing has been given back yet: the worker still holds the upstream this flow is charged for.
        assert!(flow.session.engine.flows.contains(&flow.handle));
        assert!(admission.bytes_charged() > idle);
        assert_eq!(gate.left.load(Relaxed), 0, "nor has that wait ended yet");

        // A second tick while it is waiting to be joined adds nothing at all.
        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.expired, 1);
        assert!(flow.drain().is_empty());

        let Finished::Flow(terminal) = flow.session.engine.finished().await else {
            panic!("the flow's own worker is what finished")
        };
        assert_eq!(
            gate.left.load(Relaxed),
            1,
            "the upstream wait was cancelled rather than abandoned"
        );
        flow.session
            .engine
            .close(terminal, &mut admission, &mut flow.session.output);
        assert!(!flow.session.engine.flows.contains(&flow.handle));
        assert_eq!(flow.session.engine.counters.closed, 1);
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "and every byte comes back exactly once"
        );
        assert_eq!(admission.outstanding_leases(), leases);
    }

    /// A config and a due deadline are the same ending reached two ways, and a flow ends once whichever
    /// arrives first.
    ///
    /// The owner's own priority is the first order below: its biased select takes a config ahead of any
    /// deadline, so what an expiry finds afterwards is a table the retirement already emptied. The second
    /// order is the one that has to be safe rather than preferred - a retirement arriving after an expiry
    /// has begun, with the flow still there because its worker has not been joined yet.
    #[tokio::test]
    async fn a_config_and_a_due_deadline_retire_a_flow_exactly_once() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let session = stamped(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        let opened = Instant::now();
        let mut flow = Terminating::opened(session, &mut admission, 10_100, DESTINATION, opened);
        flow.establish(&mut admission, opened);
        let due = flow.deadline().expect("an established floor");

        let successor = Stamp {
            generation: STAMP.generation,
            epoch: STAMP.epoch + 1,
        };
        flow.session
            .engine
            .apply(
                successor,
                Some(1),
                MTU,
                &mut admission,
                &mut flow.session.output,
            )
            .await;
        assert_eq!(flow.session.engine.flows.len(), 0, "the epoch took it");
        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(
            flow.session.engine.counters.expired, 0,
            "and the deadline that was already due finds nothing to take"
        );
        assert_eq!(admission.bytes_charged(), idle, "refunded once");

        // The other order, on a second flow: the expiry begins, and the config that follows finishes the
        // same retirement rather than starting another. Every step of it has to happen once - the count, the
        // reset, the packet, the join and the refund - because a config that walked over a row an expiry had
        // already cancelled would abort a closed socket, count a second reset, and say a client had been told
        // twice what it was told once.
        let reopened = Instant::now();
        flow.peer = Peer {
            source: client(10_101),
            destination: DESTINATION,
            seq: TcpSeqNumber(1_000),
            ack: TcpSeqNumber(0),
        };
        flow.syn(&mut admission, reopened);
        flow.establish(&mut admission, reopened);
        flow.drain();
        let reset = flow.session.engine.counters.reset;
        let closed = flow.session.engine.counters.closed;
        let due = flow.deadline().expect("an established floor");

        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.expired, 1);
        assert_eq!(flow.session.engine.counters.reset, reset + 1);
        let terminals = flow.drain();
        assert_eq!(terminals.len(), 1, "one terminal packet");
        assert_eq!(terminals[0].1.control, TcpControl::Rst);
        assert_eq!(terminals[0].1.destination, client(10_101));
        assert!(
            flow.session.engine.flows.contains(&flow.handle),
            "still here, because its worker has not been joined"
        );

        flow.session
            .engine
            .apply(
                Stamp {
                    generation: successor.generation,
                    epoch: successor.epoch + 1,
                },
                Some(1),
                MTU,
                &mut admission,
                &mut flow.session.output,
            )
            .await;
        assert_eq!(flow.session.engine.flows.len(), 0, "joined by the config");
        assert_eq!(
            flow.session.engine.counters.expired, 1,
            "the config completes the expiry rather than counting a second one"
        );
        assert_eq!(
            flow.session.engine.counters.reset,
            reset + 1,
            "and does not reset a client whose socket it already closed"
        );
        assert_eq!(
            flow.session.engine.counters.closed,
            closed + 1,
            "settled once"
        );
        assert!(
            flow.drain().is_empty(),
            "and wrote nothing a second time for it"
        );
        assert_eq!(admission.bytes_charged(), idle, "and refunds once");
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A segment for the exact live tuple that smoltcp then throws away still rearms, and that is the
    /// contract rather than a gap in it.
    ///
    /// The negative control for [Engine::rearm]'s stated boundary. `tcp_wire::peek` reads the four-tuple, the
    /// hop limit and the SYN bit and nothing else - no checksum, no window, no state - so "the stack accepted
    /// this" is not something this owner knows. Answering it would mean a second TCP implementation beside
    /// the one the packet was just handed to. So the line is drawn where it can be drawn: offered to smoltcp
    /// for a live tuple counts, and this pins that so the prose and the code cannot drift apart.
    #[tokio::test]
    async fn a_segment_the_stack_throws_away_still_counts_as_activity() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_130, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        flow.drain();

        // Exactly the segment the client would have sent, with its TCP checksum flipped. The engine's device
        // advertises full checksum capabilities, so smoltcp verifies this one and drops it without a word.
        let mut corrupt = flow.build(TcpControl::Psh, true, b"unverifiable");
        let checksum = vpnhotspotd::shared::packet_writer::IPV4_HEADER_LEN + 16;
        corrupt[checksum] ^= 0xff;

        let late = opened + Duration::from_secs(600);
        flow.offer(&mut admission, &corrupt, late);
        assert_eq!(
            flow.state(),
            State::Established,
            "the stack took nothing from it"
        );
        assert!(
            flow.drain().is_empty(),
            "and did not so much as acknowledge it"
        );
        assert_eq!(
            flow.deadline(),
            Some(late + Duration::from_secs(7_440)),
            "and this owner rearmed anyway, which is the coarseness the design accepts"
        );
    }

    /// A client that resets out of its own handshake leaves a flow the *floor* has to take back, and the
    /// floor takes it back silently.
    ///
    /// Two things are pinned here, and the first is why there is no stranded-`Closed` case to fix. smoltcp
    /// returns a *passively opened* socket to `Listen` on a reset rather than closing it - `(State::Listen,
    /// Rst)` is ignored outright and `(State::SynReceived, Rst)` with a listen endpoint sets `Listen`, at
    /// smoltcp-0.13.1 `src/socket/tcp.rs:1818-1831` - and every flow this engine opens has one. So a client
    /// cannot drive a flow of this engine's to `Closed` before it is established at all: every
    /// non-established `Closed` comes from this daemon's own `abort`, which always cancels or removes the
    /// flow in the same breath.
    ///
    /// The second is what does end it: the transitory floor, and with no reset, because a listening socket
    /// has no remote endpoint for the stack to answer with one. That is the silent half of the expiry
    /// counters, and the reason [Counters::expired] is not simply [Counters::reset] under another name.
    #[tokio::test]
    async fn a_handshake_the_client_resets_falls_back_to_the_floor_and_ends_silently() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let session = stamped(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        let leases = admission.outstanding_leases();

        let opened = Instant::now();
        let mut flow = Terminating::opened(session, &mut admission, 10_140, DESTINATION, opened);
        assert_eq!(flow.state(), State::SynReceived);
        // The SYN-ACK, which is what the client's reset below is answering.
        assert!(flow
            .drain()
            .iter()
            .any(|(_, wire)| wire.control == TcpControl::Syn));

        let reset = opened + Duration::from_secs(5);
        flow.deliver(&mut admission, TcpControl::Rst, true, &[], reset);
        assert_eq!(
            flow.state(),
            State::Listen,
            "a passive open goes back to listening rather than closing"
        );
        assert!(
            !flow
                .session
                .engine
                .flows
                .get(&flow.handle)
                .expect("still held")
                .cancel
                .is_cancelled(),
            "so nothing about it is terminal, and only its floor will end it"
        );
        let due = reset + Duration::from_secs(240);
        assert_eq!(flow.deadline(), Some(due), "back on the transitory floor");

        let counted = flow.session.engine.counters.reset;
        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.expired, 1);
        assert_eq!(
            flow.session.engine.counters.reset, counted,
            "a listening socket has no client to reset, so the ending is silent"
        );
        assert!(flow.drain().is_empty(), "and writes nothing");

        let Finished::Flow(terminal) = flow.session.engine.finished().await else {
            panic!("the flow's own worker is what finished")
        };
        flow.session
            .engine
            .close(terminal, &mut admission, &mut flow.session.output);
        assert!(!flow.session.engine.flows.contains(&flow.handle));
        assert_eq!(admission.bytes_charged(), idle, "refunded once");
        assert_eq!(admission.outstanding_leases(), leases);
    }

    /// An idle expiry is not a network being left, and the peer is where the difference shows.
    ///
    /// The engine-wide sweep token is what [crate::tcp_flow::splice] reads to close its upstream with
    /// `SO_LINGER(0)`, so an expiry that touched it would abort a connection whose network is perfectly
    /// usable. Proved against the remote's own answer rather than against the token alone: an ordinary close
    /// is a FIN the peer reads as end of stream, and a swept one is a reset it reads as an error. The second
    /// flow is the contrast, without which the first assertion could pass for a harness that never closes
    /// anything abortively.
    #[tokio::test]
    async fn an_idle_expiry_closes_its_upstream_ordinarily_and_a_sweep_does_not() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_150, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let (mut idle_peer, _) = listener.accept().await.expect("the flow connected out");
        let expiring = flow.handle;
        // A second later, so it is still well inside its own floor when the first one runs out.
        flow.establish_another(&mut admission, 10_151, opened + Duration::from_secs(1));
        let (mut swept_peer, _) = listener.accept().await.expect("the second connected out");

        let due = flow.deadline_of(expiring).expect("an established floor");
        flow.session.engine.expire(due, &mut flow.session.output);
        assert_eq!(
            flow.session.engine.counters.expired, 1,
            "only the one whose floor ran out"
        );
        assert!(
            !flow.session.engine.sweep.is_cancelled(),
            "an idle flow is not the selection being left"
        );
        let Finished::Flow(terminal) = flow.session.engine.finished().await else {
            panic!("a flow's own worker is what finished")
        };
        flow.session
            .engine
            .close(terminal, &mut admission, &mut flow.session.output);
        let mut sink = [0u8; 8];
        assert_eq!(
            idle_peer.read(&mut sink).await.expect("an orderly close"),
            0,
            "the upstream ended rather than being aborted"
        );

        // And the contrast: a generation change *is* the network being left, so that flow's upstream is
        // closed abortively and its peer is told so.
        flow.session
            .engine
            .apply(
                Stamp {
                    generation: STAMP.generation + 1,
                    epoch: STAMP.epoch,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut flow.session.output,
            )
            .await;
        assert_eq!(
            swept_peer
                .read(&mut sink)
                .await
                .expect_err("a reset")
                .kind(),
            std::io::ErrorKind::ConnectionReset,
            "which is what SO_LINGER(0) looks like from the other end"
        );
    }

    /// A worker that finishes cleanly hands its flow on instead of ending it, and the client's teardown gets
    /// to finish.
    ///
    /// The bug this is the regression for: both workers return as soon as *their* ordered work is done, and
    /// at that moment the client socket is still in `LAST-ACK` with a FIN to retransmit and a final
    /// acknowledgment to wait for. Removing the flow on that terminal took the client's half of the
    /// connection away mid-teardown, so a lost FIN could never be retransmitted and the client's ACK arrived
    /// at nothing.
    #[tokio::test]
    async fn a_clean_terminal_keeps_the_client_side_until_its_teardown_finishes() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let session = stamped(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        let leases = admission.outstanding_leases();

        let opened = Instant::now();
        let mut flow = Terminating::opened(session, &mut admission, 10_160, DESTINATION, opened);
        flow.establish(&mut admission, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");

        // The client finishes sending, which reaches the upstream as a shutdown; the remote answers with one
        // of its own, and that end of stream is what makes this daemon close its own half.
        flow.deliver(&mut admission, TcpControl::Fin, true, &[], opened);
        assert_eq!(flow.state(), State::CloseWait);
        assert_eq!(
            remote.read(&mut [0u8; 8]).await.expect("readable"),
            0,
            "the client's half-close reached the upstream"
        );
        remote.shutdown().await.expect("the remote may close too");
        flow.worker_event(true, opened).await;
        assert_eq!(flow.state(), State::LastAck);

        // The worker has nothing left to do and says so. The flow does not go with it.
        let Finished::Flow(terminal) = flow.session.engine.finished().await else {
            panic!("the flow's own worker is what finished")
        };
        flow.session
            .engine
            .close(terminal, &mut admission, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.detached, 1);
        assert_eq!(flow.session.engine.counters.closed, 0, "not settled yet");
        assert_eq!(
            flow.state(),
            State::LastAck,
            "the client's socket is still here, mid-teardown"
        );
        assert!(admission.bytes_charged() > idle, "and still charged for");
        // Which means the FIN can still be retransmitted, and it is smoltcp's own timer that says so rather
        // than the outer floor: the combined wake is *earlier* than this flow's four-minute closing floor, so
        // what it is waiting for is the retransmission. `is_some()` alone would have been true of the floor.
        let wake = flow
            .session
            .engine
            .next_deadline()
            .expect("something wants a wake");
        let floor = flow.deadline().expect("a closing floor");
        assert!(
            wake < floor,
            "the wake is the stack's retransmission, not the outer floor"
        );

        // Only the client's own acknowledgment finishes it, and then it is settled exactly once.
        flow.drain();
        flow.deliver(&mut admission, TcpControl::None, true, &[], opened);
        assert_eq!(flow.state(), State::Closed);
        let Finished::Detached { handle, worker } = flow.session.engine.finished().await else {
            panic!("no worker is left to report; the owner's own scan is what settles it")
        };
        assert_eq!(
            (handle, worker),
            (flow.handle, flow.worker),
            "exact identity"
        );
        flow.session.engine.settled(handle, worker, &mut admission);
        assert_eq!(flow.session.engine.counters.closed, 1);
        assert_eq!(
            flow.session.engine.counters.reset, 0,
            "a connection that closed properly is never reset"
        );
        assert!(!flow.session.engine.flows.contains(&flow.handle));
        assert_eq!(admission.bytes_charged(), idle, "refunded once");
        assert_eq!(admission.outstanding_leases(), leases);
    }

    /// The same for a DNS-over-TCP transport, whose worker also returns the moment its ordered work is done.
    #[tokio::test]
    async fn a_dns_transport_that_finished_asking_keeps_its_closing_socket() {
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let session = stamped(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        let opened = Instant::now();
        let mut flow = Terminating::opened(session, &mut admission, 10_170, RESOLVER, opened);
        flow.establish(&mut admission, opened);
        // The client has asked everything it means to ask and closes its half on a message boundary, which
        // the transport answers with an ordered end of stream.
        flow.deliver(&mut admission, TcpControl::Fin, true, &[], opened);
        assert_eq!(flow.state(), State::CloseWait);
        flow.worker_event(true, opened).await;
        assert_eq!(flow.state(), State::LastAck);

        let Finished::Flow(terminal) = flow.session.engine.finished().await else {
            panic!("the transport's own task is what finished")
        };
        flow.session
            .engine
            .close(terminal, &mut admission, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.detached, 1);
        assert_eq!(flow.state(), State::LastAck, "still finishing");
        assert!(admission.bytes_charged() > idle);

        flow.drain();
        flow.deliver(&mut admission, TcpControl::None, true, &[], opened);
        let Finished::Detached { handle, worker } = flow.session.engine.finished().await else {
            panic!("the owner's own scan is what settles it")
        };
        flow.session.engine.settled(handle, worker, &mut admission);
        assert_eq!(flow.session.engine.counters.closed, 1);
        assert_eq!(flow.session.engine.counters.reset, 0);
        assert_eq!(admission.bytes_charged(), idle, "refunded once");
        assert_eq!(admission.dns_tokens_charged(), 0);
    }

    /// A config change and a session shutdown each settle a flow whose worker is already gone, without
    /// waiting for a terminal that can never arrive and without resetting or refunding twice.
    ///
    /// `TIME-WAIT` is the shape that matters here: it holds no outer floor of this owner's, so nothing but
    /// smoltcp's own ten-second close wait would ever end it, and a retirement that waited for a second
    /// worker terminal would wait for ever.
    #[tokio::test]
    async fn a_detached_flow_is_settled_by_a_config_and_by_shutdown() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let session = stamped(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        let opened = Instant::now();
        let mut flow = Terminating::opened(session, &mut admission, 10_180, DESTINATION, opened);
        flow.establish(&mut admission, opened);
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");
        flow.detach_in_time_wait(&mut admission, &mut remote, opened)
            .await;
        assert_eq!(flow.session.engine.counters.detached, 1);
        assert_eq!(flow.session.engine.flows.len(), 1, "still owned");
        assert!(admission.bytes_charged() > idle, "and still charged for");

        // A generation change retires it. It must not wait for a terminal, and it settles it once.
        flow.session
            .engine
            .apply(
                Stamp {
                    generation: STAMP.generation + 1,
                    epoch: STAMP.epoch,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut flow.session.output,
            )
            .await;
        assert_eq!(flow.session.engine.flows.len(), 0, "the generation took it");
        assert_eq!(flow.session.engine.counters.closed, 1, "settled once");
        assert_eq!(
            flow.session.engine.counters.reset, 1,
            "a TIME-WAIT socket still has a remote endpoint, so it is told once"
        );
        assert_eq!(admission.bytes_charged(), idle, "refunded once");

        // And the whole-session path settles one just as readily.
        let reopened = Instant::now();
        flow.peer = Peer {
            source: client(10_181),
            destination: DESTINATION,
            seq: TcpSeqNumber(1_000),
            ack: TcpSeqNumber(0),
        };
        flow.syn(&mut admission, reopened);
        flow.establish(&mut admission, reopened);
        let (mut remote, _) = listener.accept().await.expect("the second connected out");
        flow.detach_in_time_wait(&mut admission, &mut remote, reopened)
            .await;
        assert_eq!(flow.session.engine.counters.detached, 2);

        let charged = admission.bytes_charged();
        assert!(
            charged > idle,
            "the second leg really is still charged before this"
        );
        flow.session
            .engine
            .shutdown(&mut admission, &mut flow.session.output)
            .await;
        assert_eq!(flow.session.engine.flows.len(), 0);
        assert_eq!(flow.session.engine.counters.closed, 2, "settled once each");
        assert_eq!(
            flow.session.engine.counters.reset, 2,
            "and told its client once, like the first leg"
        );
        assert_eq!(admission.bytes_charged(), idle, "refunded once");
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A worker that failed for its own reasons is not a clean completion, and still ends its flow at once.
    #[tokio::test]
    async fn a_reported_worker_failure_still_resets_and_ends_at_once() {
        let mut admission = admission_for(4);
        // A listener that is gone by the time the flow connects, so the upstream is refused - the ordinary
        // unreachable-destination outcome, which the client learns of through a reset.
        let (gate, listener) = opening_gate().await;
        drop(listener);
        let session = stamped(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        let opened = Instant::now();
        let mut flow = Terminating::opened(session, &mut admission, 10_190, DESTINATION, opened);
        flow.establish(&mut admission, opened);
        flow.drain();

        let Finished::Flow(terminal) = flow.session.engine.finished().await else {
            panic!("the flow's own worker is what finished")
        };
        flow.session
            .engine
            .close(terminal, &mut admission, &mut flow.session.output);
        assert_eq!(
            flow.session.engine.counters.detached, 0,
            "a failure is not a clean completion and hands nothing on"
        );
        assert_eq!(flow.session.engine.counters.reset, 1, "the client is told");
        assert_eq!(flow.session.engine.counters.closed, 1);
        assert!(!flow.session.engine.flows.contains(&flow.handle));
        assert!(flow
            .drain()
            .iter()
            .any(|(_, wire)| wire.control == TcpControl::Rst));
        assert_eq!(admission.bytes_charged(), idle, "refunded once");
    }

    /// A third-handshake acknowledgment that also carries FIN opens the connection and half-closes it in one
    /// step, and both halves of that have to be noticed.
    ///
    /// smoltcp goes `SYN-RECEIVED` -> `CLOSE-WAIT` directly here, so `ESTABLISHED` is never observable
    /// (smoltcp-0.13.1, `src/socket/tcp.rs:1880-1886`). A flow that watched for that one state stayed on the
    /// established floor for two hours with `downstream` open and an upstream peer waiting for a request
    /// nobody would finish sending.
    #[tokio::test]
    async fn an_ack_that_also_carries_fin_opens_the_flow_and_propagates_the_half_close() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_200, DESTINATION, opened).await;
        assert_eq!(flow.state(), State::SynReceived);
        assert!(flow
            .drain()
            .iter()
            .any(|(_, wire)| wire.control == TcpControl::Syn));
        let (mut remote, _) = listener.accept().await.expect("the flow connected out");

        // The handshake completes and the client closes its half in the same segment.
        flow.deliver(&mut admission, TcpControl::Fin, true, &[], opened);
        assert_eq!(flow.state(), State::CloseWait);
        assert!(
            flow.session
                .engine
                .flows
                .get(&flow.handle)
                .expect("held")
                .record
                .established,
            "the connection was open, whether or not ESTABLISHED was ever observable"
        );
        assert_eq!(
            flow.deadline(),
            Some(opened + Duration::from_secs(7_440)),
            "and CLOSE-WAIT is the established floor"
        );
        assert!(
            flow.session
                .engine
                .flows
                .get(&flow.handle)
                .expect("held")
                .record
                .downstream
                .is_none(),
            "the half-close was propagated rather than left open"
        );
        assert_eq!(
            remote.read(&mut [0u8; 8]).await.expect("readable"),
            0,
            "so the upstream peer really saw the end of the request"
        );

        // And it finishes without a premature removal or a second close.
        remote.shutdown().await.expect("the remote answers");
        flow.worker_event(true, opened).await;
        assert_eq!(flow.state(), State::LastAck);
        let Finished::Flow(terminal) = flow.session.engine.finished().await else {
            panic!("the flow's own worker is what finished")
        };
        flow.session
            .engine
            .close(terminal, &mut admission, &mut flow.session.output);
        assert_eq!(flow.session.engine.counters.detached, 1);
        flow.drain();
        flow.deliver(&mut admission, TcpControl::None, true, &[], opened);
        let Finished::Detached { handle, worker } = flow.session.engine.finished().await else {
            panic!("the owner's own scan settles it")
        };
        flow.session.engine.settled(handle, worker, &mut admission);
        assert_eq!(flow.session.engine.counters.closed, 1);
        assert_eq!(
            flow.session.engine.counters.reset, 0,
            "and none of it a reset"
        );
    }

    /// The scans that walk the round-robin order visit every live flow and act on exactly the right ones.
    ///
    /// Three of the engine's owner paths used to collect a fresh list of handles per call, sized by the live
    /// flows and charged to nothing: the closed-socket scan in `poll`, the half-close scan in
    /// `pump_to_upstream`, and a trailing orphan-socket sweep in the config retirement. They now walk the
    /// order the fair queue registers every admitted flow into, in place, and the orphan sweep is a
    /// `debug_assert` on the pairing that made it dead code.
    ///
    /// What this covers is the part a test can reach: that the in-place walks still see all of it and none of
    /// what they should not. Three concurrent flows, each in a different state, with the round-robin rotated
    /// between them - a walk that stopped early, started from the wrong end, or acted on a neighbour fails
    /// here.
    ///
    /// Two structural assertions ride along, and only because this drives the paths that check them. Every
    /// poll here asserts that the round-robin order indexes exactly the live flows, which is what both walks
    /// rest on. The socket-and-flow pairing assertion lives in the config retirement - it replaced the sweep
    /// that used to look for orphans - so the retirement at the end is what drives it. That the walks
    /// allocate nothing is not something a test can observe: it is a property of the code, checked by reading
    /// it and by the compiler.
    #[tokio::test]
    async fn the_in_place_scans_visit_every_live_flow_and_only_the_right_ones() {
        let mut admission = admission_for(4);
        let (gate, listener) = opening_gate().await;
        let opened = Instant::now();
        let mut flow = Terminating::one(&mut admission, &gate, 10_210, DESTINATION, opened).await;
        flow.establish(&mut admission, opened);
        let (_first, _) = listener.accept().await.expect("connected out");
        let untouched = flow.handle;
        let peer = flow.peer.source;

        // Each one is driven while the harness's client cursor is still its own, and all three are asserted
        // together afterwards - which is what makes this about the scans rather than about one flow.
        let halving = flow.establish_another(&mut admission, 10_211, opened);
        let (mut middle, _) = listener.accept().await.expect("connected out");
        flow.deliver(&mut admission, TcpControl::Fin, true, &[], opened);
        assert_eq!(flow.state(), State::CloseWait);
        assert_eq!(
            middle.read(&mut [0u8; 8]).await.expect("readable"),
            0,
            "the half-close reached its own upstream"
        );

        let closing = flow.establish_another(&mut admission, 10_212, opened);
        let (_last, _) = listener.accept().await.expect("connected out");
        flow.drain();
        flow.deliver(&mut admission, TcpControl::Rst, true, &[], opened);
        assert_eq!(flow.state(), State::Closed);
        assert_eq!(flow.session.engine.flows.len(), 3, "all three still held");

        let downstream = |flow: &Terminating, handle| {
            flow.session
                .engine
                .flows
                .get(&handle)
                .expect("held")
                .record
                .downstream
                .is_some()
        };
        let cancelled = |flow: &Terminating, handle| {
            flow.session
                .engine
                .flows
                .get(&handle)
                .expect("held")
                .cancel
                .is_cancelled()
        };

        // The half-close scan dropped exactly one upstream write half.
        assert!(!downstream(&flow, halving.0), "the one that half-closed");
        assert!(downstream(&flow, untouched), "and not its neighbours");

        // The closed-socket scan began exactly one retirement.
        assert!(
            cancelled(&flow, closing.0),
            "the one the stack finished with"
        );
        assert!(!cancelled(&flow, untouched), "and neither neighbour");
        assert!(!cancelled(&flow, halving.0));

        // The untouched flow is still exactly what it was, which is the other half of "only the right ones".
        assert_eq!(
            flow.session
                .engine
                .flows
                .get(&untouched)
                .expect("held")
                .record
                .client,
            peer
        );
        assert_eq!(
            flow.session.engine.sockets.get::<Socket>(untouched).state(),
            State::Established
        );

        // And the retirement, which is where the socket-and-flow pairing assertion lives: it replaced the
        // orphan sweep that used to allocate a list to find nothing, so it is only checked when a retirement
        // really runs. Three flows in three different states go through it at once.
        flow.session
            .engine
            .apply(
                Stamp {
                    generation: STAMP.generation + 1,
                    epoch: STAMP.epoch,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut flow.session.output,
            )
            .await;
        assert_eq!(flow.session.engine.flows.len(), 0, "all three retired");
        assert_eq!(flow.session.engine.counters.closed, 3, "settled once each");
        assert_eq!(admission.invariant_violations(), 0);
    }
}
