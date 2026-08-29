use std::future::poll_fn;
use std::io;
use std::task::{Context, Poll};
use std::time::Instant;

use smoltcp::iface::SocketHandle;

use super::{lifetime, Engine};
use crate::shizuku::output::Output;
use crate::shizuku::tcp_dns;
use vpnhotspotd::shared::workers::Terminal;

/// What this engine needs its owner to do next.
pub(crate) enum Attention {
    Flow(Terminal<SocketHandle>),
    /// A flow whose transport task completed cleanly earlier and whose client side has now finished too. Its
    /// exact identity rather than a terminal, because there is no task left to produce one - see
    /// [Engine::finish_client_close].
    ClientClosed {
        handle: SocketHandle,
        incarnation: u64,
    },
    /// A resolver transaction settlement independent of flow lifetime, or a terminal table-invariant
    /// failure.
    Transaction(io::Result<tcp_dns::Settlement>),
    /// Bytes crossed a bridge in one direction or the other, or a half-close did, so the stack has something
    /// to do about it. Payload-free and flow-free on purpose: the pass has already moved everything it could,
    /// and what is left is to run the stack and refresh the lifetimes that saw traffic.
    Traffic,
}

impl Engine {
    /// The next thing this engine needs its owner for. Selected on by the owning task, so it waits forever
    /// while there is nothing rather than answering at once.
    pub(crate) async fn attention(&mut self) -> Attention {
        poll_fn(|cx| self.poll_attention(cx)).await
    }

    /// Every source, in one poll and in the order they matter.
    pub(crate) fn poll_attention(&mut self, cx: &mut Context<'_>) -> Poll<Attention> {
        // Drain readiness queues before scanning flows; otherwise each completion triggers a full scan.
        if let Poll::Ready(terminal) = self.flows.poll_finished(cx) {
            return Poll::Ready(Attention::Flow(terminal));
        }
        if let Poll::Ready(settlement) = self.queries.poll_finished(cx) {
            return Poll::Ready(Attention::Transaction(settlement));
        }
        // Client closure is owner-local state with no waker. Check it after finite readiness batches and
        // before pumping bridges.
        if let Some(closed) = self.next_client_closed() {
            return Poll::Ready(closed);
        }
        if self.pump(cx) {
            return Poll::Ready(Attention::Traffic);
        }
        Poll::Pending
    }

    /// One pass over every live flow, in the round-robin order, giving each exactly one bounded turn.
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
    pub(crate) fn traffic(&mut self, admitting: bool, now: Instant, output: &mut Output) {
        self.poll(output);
        // After the poll, because the end of stream this pass may have taken is what makes this owner close
        // its own half, and the floor that applies is the one the socket lands on.
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
            let incarnation = held.id;
            if admitting {
                lifetime::rearm(flows, sockets, *handle, incarnation, now);
            }
        }
    }
}
