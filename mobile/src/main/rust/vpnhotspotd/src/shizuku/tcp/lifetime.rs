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

use super::Engine;
use crate::shizuku::output::Output;
use vpnhotspotd::shared::fair::FlowId;

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
/// The question [crate::shizuku::tcp::Flow]'s `established` answers, and it has to be asked of the phase rather than
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
/// and acknowledged to a producer whose bytes never reached anyone - see [crate::shizuku::tcp::Engine::pump_to_client].
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
    ///
    /// Two things reach here, and the second is narrower than it looks: a client packet offered to the stack,
    /// and one *delivery* - a chunk this owner's own scan moved out of a flow's queue and into its row. A
    /// chunk the consumption path takes instead, because the row was busy when it arrived, rearms nothing;
    /// under a download that is the common case, and what refreshes such a flow is the client's own
    /// acknowledgements, which arrive as packets. A client that has stopped acknowledging is a flow this
    /// owner is entitled to expire.
    pub(super) fn rearm(&mut self, handle: SocketHandle, worker: u64, now: Instant) {
        // Both halves, because smoltcp reuses handles: a packet or a delivery naming a replaced flow's handle
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
            // `sleep_until` measures this against - see [crate::shizuku::tun_reader::run].
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
    /// [crate::shizuku::tcp_flow::splice] reads to close its upstream with `SO_LINGER(0)`, and a flow that fell idle
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
                // Discard before cancel, and per exact identity: a worker parked on a handover may only be
                // released once the owner has committed to dropping what that wait was for.
                drop(fair.begin_retire(FlowId::new(*handle, held.record.worker)));
                held.cancel.cancel();
                // The slot goes, which closes the queue toward the task: one blocked on the client's half of
                // the splice wakes and exits, and the reservation this owner held is released with it.
                held.record.transfer.stop_sending();
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
