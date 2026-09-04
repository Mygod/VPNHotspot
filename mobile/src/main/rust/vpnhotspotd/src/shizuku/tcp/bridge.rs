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
    /// A retained client-only flow whose close or reset has finished.
    ClientClosed {
        handle: SocketHandle,
        incarnation: u64,
    },
    /// A resolver transaction settlement independent of flow lifetime, or a terminal table-invariant
    /// failure.
    Transaction(io::Result<tcp_dns::Settlement>),
    /// Bridge progress requires a stack poll and lifetime refresh.
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
        // Client-only closure has no separate waker.
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
        // `Turns` visits each live flow once and rotates the next starting point.
        while let Some(handle) = self.outgoing.turn() {
            moved |= self.cross(handle, cx);
        }
        moved
    }

    /// One flow's turn, and what this engine records about it.
    fn cross(&mut self, handle: SocketHandle, cx: &mut Context<'_>) -> bool {
        // Share ingress crossing and terminal-tail handling.
        let Some(crossing) = vpnhotspotd::shared::ingress::crossed(self, handle, cx) else {
            // Unreachable while the order indexes the live flows, and answered rather than asserted because
            // the pass must not stop on it.
            self.counters.ingress.stale += 1;
            return false;
        };
        if crossing.moved {
            // Record stack mutation before this cancellable select arm returns.
            self.stack_changed();
        }
        self.counters.to_client += crossing.to_client as u64;
        self.counters.ingress.to_upstream += crossing.to_upstream as u64;
        if crossing.stranded {
            self.counters.ingress.stale += 1;
        }
        // Only actual progress refreshes the idle floor.
        if let Some(held) = self.flows.get_mut(&handle) {
            held.record.refresh |= crossing.delivered;
        }
        crossing.moved
    }

    /// Polls after bridge progress and refreshes affected idle floors.
    pub(crate) fn traffic(&mut self, admitting: bool, now: Instant, output: &mut Output) {
        self.poll(output);
        // After the poll, because the end of stream this pass may have taken is what makes this owner close
        // its own half, and the floor that applies is the one the socket lands on.
        let Engine {
            flows,
            sockets,
            outgoing,
            idle,
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
                lifetime::rearm(flows, sockets, idle, *handle, incarnation, now);
            }
        }
    }
}
