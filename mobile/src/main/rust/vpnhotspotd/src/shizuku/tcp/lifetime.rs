//! How long an idle terminated TCP flow lives in this daemon's own outer state.
//!
//! This is the outer, userspace half and nothing else. Android's inner IPv4 NAT keeps conntrack state of its
//! own for the same client, and none of it is mirrored, configured or timed from here: what this owns is the
//! flow record, its smoltcp socket and the transport task behind them.
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
//! [rearm] says where the line is.
//!
//! The floors themselves, the phase classification and the rearm decision are
//! [vpnhotspotd::shared::lifetime]'s: they are pure maps from a phase to a duration, and the one that decides
//! what a *halted* flow keeps is the difference between a clean flush finishing and a transport task
//! cancelled out from under bytes this daemon already acknowledged. What is left here is the engine's use of them - which
//! flow, which incarnation, and the two tables the answer is written into.
//!
//! **No post-RST retention is claimed.** RFC 7857 later recommends holding a mapping for four minutes after
//! a matching RST, which would need state outliving the live flow entirely. `Closed` is terminal here, and a
//! reset - the client's or this daemon's - ends the flow rather than starting a tombstone.

use std::time::{Duration, Instant};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::Socket;

use super::{Engine, Flow};
use crate::shizuku::flow_setup::Sockets;
use crate::shizuku::output::Output;
use vpnhotspotd::shared::lifetime::rearmed;
pub(super) use vpnhotspotd::shared::lifetime::{deadline, opened};
use vpnhotspotd::shared::workers::Workers;

/// Rearms one exact flow from the state its socket is really in.
///
/// A free function over the two tables it touches rather than a method, because both callers reach it while
/// holding something else of the engine's: the packet path has the segment it just offered, and the traffic
/// path is walking the round-robin order read-only. One copy either way - the policy below is what must not
/// be written twice.
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
/// and one *crossing toward the client* - bytes this owner moved out of a flow's bridge and into that
/// client's send buffer. Bytes the upstream half merely wrote into the bridge rearm nothing, because a
/// client that has stopped acknowledging fills its send buffer and stops the crossing: such a flow is
/// then refreshed only by the client's own acknowledgements, and one that has stopped sending them is a
/// flow this owner is entitled to expire.
pub(super) fn rearm(
    flows: &mut Workers<SocketHandle, Flow>,
    sockets: &Sockets,
    handle: SocketHandle,
    incarnation: u64,
    now: Instant,
) {
    // Both halves, because smoltcp reuses handles: a packet or a delivery naming a replaced flow's handle
    // would otherwise hand the successor its predecessor's lease of life.
    if !flows.current(&handle, incarnation) {
        return;
    }
    // Already retiring and waiting only on its transport task. A refreshed deadline would outlive the
    // record it belongs to, and [Engine::next_deadline] excludes it from the schedule anyway.
    let Some(held) = flows.get(&handle) else {
        return;
    };
    if held.cancel.is_cancelled() {
        return;
    }
    // The decision is [vpnhotspotd::shared::lifetime::rearmed]'s, and where this flow stands in the clean
    // client-ending lifecycle is [vpnhotspotd::shared::bridge::Bridge::ending]'s - read from the bridge and
    // the phase together, because the two disagree at the moment a floor is armed: the packet has reached
    // the stack and the ending has not been extracted yet. This is the *other* caller - a delivery or a
    // timer, not a client packet; the packet boundary arms its own, in order, in
    // [vpnhotspotd::shared::ingress::accept].
    let state = sockets.get::<Socket>(handle).state();
    let armed = rearmed(
        held.record.deadline,
        state,
        held.record.bridge.ending(state),
        now,
    );
    if let Some(held) = flows.get_mut(&handle) {
        held.record.deadline = armed;
    }
}

impl Engine {
    /// When this engine next needs to run regardless of traffic.
    ///
    /// Two sources and one answer: smoltcp's own protocol timers - retransmission, delayed acknowledgement
    /// and the ten-second close wait it owns for `TIME-WAIT` - and the earliest outer idle deadline any live
    /// flow holds.
    ///
    /// A cancelled flow is excluded, and that is load-bearing rather than tidy. Cancelling does not remove
    /// one - what removes it is whichever of its two endings applies: a flow whose transport task is still
    /// running leaves when that task finishes, so that the refund lands once everything the task held is
    /// back, while one already closing client-side has no task left and leaves when this owner's own scan
    /// finds its client finished (see [Engine::next_client_closed]). Either way a flow just retired for being
    /// idle would otherwise keep its passed deadline as the earliest in the table and spin the owner's select
    /// loop until that ending arrived.
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
    /// own ending arrives, and which ending that is depends on which phase it is in: one whose transport task
    /// is still running waits for that task's terminal through [Engine::close], the join fence every other
    /// ending goes through, while one already closing client-side has no terminal coming and is settled by
    /// this owner's own scan through [Engine::finish_client_close]. Repeated ticks are idempotent either way:
    /// a flow already on its way out is skipped.
    pub(crate) fn expire(&mut self, now: Instant, output: &mut Output) {
        // Walked over the round-robin order rather than into a list of what is due. That order is registered
        // with every admitted flow and deregistered with every closed one, so it already holds each live
        // handle exactly once - see [vpnhotspotd::shared::flow::admit_flow] - and a list built here would be
        // scratch sized by traffic that no lease covers, allocated on a path a stopping session still runs.
        // Destructured because the walk reads one field while the steps below write three others.
        let expired = {
            let Engine {
                flows,
                sockets,
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
                // A flow closing client-side is not on its way out and is not skipped: it has no task left,
                // so its floor is the only thing that can still end it, and it is settled by the owner's own
                // scan rather than by a terminal it will never produce.
                if held.cancel.is_cancelled()
                    || !held.record.deadline.is_some_and(|deadline| deadline <= now)
                {
                    continue;
                }
                // Cancelling is the whole of it, and the whole of it is abortive: every wait a worker can be
                // in races this token, and whatever either direction of the bridge still holds is discarded
                // with the bridge when the flow is reclaimed.
                held.cancel.cancel();
                // At most one reset per expired flow, built while the socket that carries it still exists,
                // so a client fails fast instead of waiting out its own retransmissions. Counted only where the
                // stack really has somewhere to send one: a socket with no remote endpoint - one still
                // listening, or one already closed - is aborted silently, and counting that would overstate
                // what was sent.
                let socket = sockets.get_mut::<Socket>(*handle);
                if socket.remote_endpoint().is_some() {
                    counters.ingress.reset += 1;
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
