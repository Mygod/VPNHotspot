//! What makes this engine's owner runnable, and the pass that gives every flow its turn.
//!
//! One task owns the stack, so every byte in both directions is moved from here. The crossing itself -
//! which direction may move, how much, and what a direction ending means - is
//! [vpnhotspotd::shared::bridge], because it is the half that is a decision rather than a table walk, and
//! therefore the half a host test can drive against a real `smoltcp` connection. What is here is the
//! engine's use of it: whose turn it is, what the counters say, and which lifetime a delivery refreshes.
//!
//! # Fairness is the pass, not a scheduler
//!
//! [Engine::pump] walks the round-robin order once and gives every live flow exactly one turn, rotating which
//! flow is first. A turn is bounded by that flow's own charged buffers - one crossing takes at most the
//! contiguous free window of its 64 KiB send buffer and gives at most what its bridge will admit - so no flow
//! can hold the scan against the others and no flow can be skipped by one that is busier. What this replaces
//! is a deficit round-robin with a one-chunk row per flow, which existed only because the owner could not
//! write into `smoltcp` without first holding the chunk somewhere.

use std::future::poll_fn;
use std::task::{Context, Poll};
use std::time::Instant;

use smoltcp::iface::SocketHandle;

use super::{lifetime, Engine};
use crate::shizuku::output::Output;
use crate::shizuku::tcp_dns;
use vpnhotspotd::shared::workers::Terminal;

/// What this engine needs its owner to do next.
///
/// Three of them are endings and are settled by [crate::shizuku::tcp::terminal]; the fourth is traffic and is
/// settled by polling the stack. They are one enum because they are one wait: the ingress task cannot hold
/// two futures over the same tables, and which of these is answered first is a decision rather than an
/// accident - a worker that has finished is holding a record and a charge nothing else will release, so it
/// comes before bytes that are merely waiting.
pub(crate) enum Attention {
    Flow(Terminal<SocketHandle>),
    /// A flow whose worker finished cleanly earlier and whose client side has now finished too. Its exact
    /// identity rather than a terminal, because there is no task left to produce one - see
    /// [Engine::settled].
    Detached {
        handle: SocketHandle,
        worker: u64,
    },
    /// A resolver transaction that outlived the flow which asked for it. A value rather than a terminal
    /// message, because this owner polls its rows itself - see [crate::shizuku::tcp_dns].
    Transaction(tcp_dns::Settlement),
    /// Bytes crossed a bridge in one direction or the other, or a half-close did, so the stack has something
    /// to do about it. Payload-free and flow-free on purpose: the pass has already moved everything it could,
    /// and what is left is to run the stack and refresh the lifetimes that saw traffic.
    Traffic,
}

impl Engine {
    /// The next thing this engine needs its owner for. Selected on by the owning task, so it waits forever
    /// while there is nothing rather than answering at once.
    ///
    /// Cancellation-safe in every arm, which is what lets the ingress task abandon this for another and come
    /// back: `JoinSet::poll_join_next_with_id` is, the transaction table's own scan is, and the traffic pass
    /// below moves bytes out of one buffer and into another under this owner's own `&mut`, so an abandoned
    /// poll leaves them delivered rather than lost.
    pub(crate) async fn attention(&mut self) -> Attention {
        poll_fn(|cx| self.poll_attention(cx)).await
    }

    /// Every source, in one poll and in the order they matter.
    ///
    /// Sequential rather than concurrent, and that is the whole reason this is a hand-written poll instead of
    /// a `select!`: each of these borrows the tables the others do, so they can be registered one after
    /// another but never held at the same time. Answering early leaves the sources after it unregistered for
    /// this poll, which is what a biased `select!` does too - the owner comes straight back here once it has
    /// dealt with the answer.
    pub(crate) fn poll_attention(&mut self, cx: &mut Context<'_>) -> Poll<Attention> {
        // Answered before anything is polled, and it is not a wait: what it looks at is state this owner
        // already holds, and the only thing that can change it is work this owner just did - every
        // transition to `Closed` comes from a packet or a poll this loop performed, and the loop re-enters
        // here immediately afterwards, so there is no waker to register.
        if let Some(detached) = self.detached() {
            return Poll::Ready(detached);
        }
        if let Poll::Ready(terminal) = self.flows.poll_finished(cx) {
            return Poll::Ready(Attention::Flow(terminal));
        }
        if let Poll::Ready(settlement) = self.queries.poll_finished(cx) {
            return Poll::Ready(Attention::Transaction(settlement));
        }
        if self.pump(cx) {
            return Poll::Ready(Attention::Traffic);
        }
        Poll::Pending
    }

    /// One pass over every live flow, in the round-robin order, giving each exactly one bounded turn.
    ///
    /// Answers whether anything at all moved, which is what makes this a readiness source: `false` means
    /// every flow that could still be told something has this task registered with the bridge that would tell
    /// it, and the two places that are deliberately *not* registered - a full send buffer, an empty receive
    /// buffer - are the two that only a packet or a timer can change.
    fn pump(&mut self, cx: &mut Context<'_>) -> bool {
        debug_assert_eq!(
            self.outgoing.len(),
            self.flows.len(),
            "the round-robin order indexes exactly the live flows"
        );
        let mut moved = false;
        // Every live flow exactly once, and the order's own cursor is what says which - so the pass cannot
        // reach one twice, cannot skip one, and cannot start at the same flow for ever. Running it to `None`
        // is what ends it and moves the starting position on; see [vpnhotspotd::shared::flow::Turns::turn].
        while let Some(handle) = self.outgoing.turn() {
            moved |= self.cross(handle, cx);
        }
        moved
    }

    /// One flow's turn, and what this engine records about it.
    ///
    /// The two borrows are of separate fields, which is what lets the socket set be reached while the flow
    /// table is held. The phase is the crossing's own to read, so this owner cannot hold a second opinion
    /// about it.
    fn cross(&mut self, handle: SocketHandle, cx: &mut Context<'_>) -> bool {
        // The crossing, and any terminal-tail failure it finds, are both
        // [vpnhotspotd::shared::ingress::crossed]'s - the same answer an ingress gives, from the same code,
        // so a flow does not care which path noticed. The stack is polled by [Engine::traffic] straight after
        // this pass, which is what puts the resulting reset on the wire.
        let Some(crossing) = vpnhotspotd::shared::ingress::crossed(self, handle, cx) else {
            // Unreachable while the order indexes the live flows, and answered rather than asserted because
            // the pass must not stop on it.
            self.counters.ingress.stale += 1;
            return false;
        };
        self.counters.to_client += crossing.to_client as u64;
        self.counters.ingress.to_upstream += crossing.to_upstream as u64;
        if crossing.stranded {
            self.counters.ingress.stale += 1;
        }
        // Only the direction this owner performs refreshes an idle floor. A client that stops acknowledging
        // fills its send buffer, which stops that direction and lets the flow expire - see [lifetime::rearm].
        if let Some(held) = self.flows.get_mut(&handle) {
            held.record.refresh |= crossing.delivered;
        }
        crossing.moved
    }

    /// What the owner does about a pass that moved something: run the stack for it, and refresh the idle
    /// floor of every flow that was delivered to.
    ///
    /// Byte movement is [Engine::pump]'s alone, and it happens inside the poll the ingress task makes so that
    /// every registration is that task's own - which is what leaves [Engine::poll] doing nothing but running
    /// the stack, and is all the packet, timer, retirement and expiry paths ever needed from it.
    pub(crate) fn traffic(&mut self, admitting: bool, now: Instant, output: &mut Output) {
        self.poll(output);
        // After the poll, because the end of stream this pass may have taken is what makes this owner close
        // its own half, and the floor that applies is the one the socket lands on.
        //
        // `admitting` is the session's current admission state rather than the one a flow was opened under.
        // An admission-closed session may drain what it already owns - the payload still reaches the client -
        // but it may not refresh a lifetime, because refreshing one is tracking state after admission has
        // closed. The mark is cleared either way, so a flow cannot carry one across into a session that is
        // admitting again.
        //
        // Walked read-only, which is what [vpnhotspotd::shared::flow::Turns::iter] is for: taking turns here
        // would rotate the order behind the pass that just ran and hand the front to a different flow twice
        // over. Destructured because the walk reads one field while the refresh writes two others.
        let Engine {
            flows,
            sockets,
            outgoing,
            ..
        } = self;
        for handle in outgoing.iter() {
            let Some(held) = flows.get_mut(handle) else {
                continue;
            };
            if !std::mem::take(&mut held.record.refresh) {
                continue;
            }
            let worker = held.record.worker;
            if admitting {
                lifetime::rearm(flows, sockets, *handle, worker, now);
            }
        }
    }
}
