//! Where bytes cross between the client's stack and a flow's upstream half, and what makes this owner
//! runnable when they can.
//!
//! One task owns the stack, so every byte in both directions is moved from here. What each direction needs is
//! therefore the same two things - a bound on what may be in flight, and a wake when the other end has made
//! room - and both come from the queue itself rather than from a message this daemon sends about it.
//!
//! # Where the decisions live
//!
//! The per-flow half of it is [vpnhotspotd::shared::transfer]: what a flow's owner may take, what it may hand
//! over, and which queue registers it for which wake. What is here is the engine's use of that - the smoltcp
//! reads and writes, the fair round, and the one readiness answer the ingress task selects on.
//!
//! **Upstream to client.** The upstream half queues each read into that flow's own queue and reads again as
//! soon as there is room, up to [vpnhotspotd::shared::flow_budget::READ_AHEAD] chunks. This owner takes
//! exactly one of them at a time into the fair queue's row - the chunk being written, at an exact offset -
//! and takes the next the moment that one is fully written. So the producer is never waiting on this owner's
//! *turn*, only on the depth it was charged for, and this owner is never waiting on a message about it: the
//! queue is the wake. What this replaces was a rendezvous per segment, and after that a global readiness
//! marker per chunk that every flow shared.
//!
//! DNS-over-TCP keeps the acknowledged handover, deliberately: its delivery grant pays for the answer, the
//! framed copy and *one* piece, so its pieces may not be pipelined into the same queue. It is the only reason
//! [Engine::acknowledge] still exists, and the flow's own kind is what decides - see
//! [vpnhotspotd::shared::mailbox].
//!
//! **Client to upstream.** This owner holds one slot in each flow's upstream queue. Holding it is the
//! permission to move a chunk out of the stack's receive buffer; using it starts the next reservation, and
//! that reservation is what registers this owner to be woken when the flow's task takes the chunk. Reading
//! the queue's *capacity* instead - which is what this replaces - answered whether there was room and
//! registered nothing, so a full queue left the stack's receive buffer undrained and the client's window
//! closed until some unrelated event happened to wake this task: another client's packet, a stack timer, a
//! config. For a flow whose only traffic is the upload that filled the queue, the next such event is the
//! client's own zero-window probe. See [vpnhotspotd::shared::room].
//!
//! # One readiness answer for the whole engine
//!
//! [Engine::attention] is what the ingress task selects on, and it answers all four things this owner can be
//! needed for: a flow whose worker finished, a detached flow whose client finished, a resolver transaction
//! that settled, and a flow whose own queue has something to move. One future rather than four arms because
//! each of them borrows the same tables, and one poll is what lets them be registered in sequence rather than
//! held at once.

use std::future::poll_fn;
use std::task::{Context, Poll};
use std::time::Instant;

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::Socket;
use vpnhotspotd::shared::fair::{FlowId, Progress};
use vpnhotspotd::shared::flow_budget::READ_CHUNK;
use vpnhotspotd::shared::transfer::{self, Polling, Refilled};

use super::{lifetime, Engine, Kind};
use crate::shizuku::output::Output;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_dns;
use crate::shizuku::workers::Terminal;

/// What this engine needs its owner to do next.
///
/// Three of them are endings and are settled by [crate::shizuku::tcp::terminal]; the fourth is readiness and
/// is settled by polling the stack. They are one enum because they are one wait: the ingress task cannot hold
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
    /// One flow's queued payload - or its ordered end of stream - is now in that flow's row, so the stack
    /// has something to be given. Named, because the identity is the lifetime this owner refreshes.
    Delivered(FlowId<SocketHandle>),
    /// At least one flow is holding its upstream slot and has bytes in the stack's receive buffer, so
    /// polling the stack will move them. Payload-free and flow-free on purpose: what to do about it is
    /// decided from owner state.
    Upstream,
}

impl Engine {
    /// The next thing this engine needs its owner for. Selected on by the owning task, so it waits forever
    /// while there is nothing rather than answering at once.
    ///
    /// Cancellation-safe in every arm, which is what lets the ingress task abandon this for another and come
    /// back: `JoinSet::poll_join_next_with_id` is, the transaction table's own scan is, and a reservation
    /// this owner acquired is kept by the flow it belongs to rather than by the future that asked for it -
    /// so an abandoned poll leaves a slot in hand, never a slot lost.
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
    fn poll_attention(&mut self, cx: &mut Context<'_>) -> Poll<Attention> {
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
        match self.poll_flows(cx) {
            transfer::Ready::Delivered(id) => Poll::Ready(Attention::Delivered(id)),
            transfer::Ready::Upstream => Poll::Ready(Attention::Upstream),
            transfer::Ready::Nothing => Poll::Pending,
        }
    }

    /// Polls every flow's own two queues, which is the whole of this engine's traffic readiness.
    ///
    /// The scan itself is [vpnhotspotd::shared::transfer::poll_flows]; what this adds is the half that scan
    /// has no business knowing - which flow each row belongs to, and whether the client-side stack has bytes
    /// that flow's task could take. The two borrows are of separate fields, which is what lets the socket set
    /// be read while the flow table is walked.
    fn poll_flows(&mut self, cx: &mut Context<'_>) -> transfer::Ready<SocketHandle> {
        let Engine {
            flows,
            sockets,
            fair,
            ..
        } = self;
        let sockets = &*sockets;
        transfer::poll_flows(
            cx,
            flows.iter_mut().map(|(handle, held)| Polling {
                id: FlowId::new(*handle, held.record.worker),
                transfer: &mut held.record.transfer,
                receiving: sockets.get::<Socket>(*handle).can_recv(),
            }),
            fair,
        )
    }

    /// One flow whose queued payload the scan just moved into its row: give it to the stack, and refresh the
    /// lifetime it belongs to.
    ///
    /// The identity comes from this owner's own scan rather than from a message, so there is nothing stale to
    /// check: the flow was in the table when the chunk was taken and nothing has awaited since.
    ///
    /// `admitting` is the session's current admission state rather than the one the flow was opened under.
    /// An admission-closed session may drain what it already owns - the payload still reaches the client -
    /// but it may not refresh a lifetime, because refreshing one is tracking state after admission has
    /// closed.
    pub(crate) fn delivered(
        &mut self,
        id: FlowId<SocketHandle>,
        admitting: bool,
        now: Instant,
        output: &mut Output,
    ) {
        self.poll(output);
        // After the poll, because the end of stream the scan may have taken is what makes this owner close
        // its own half, and the floor that applies is the one the socket lands on. A DNS-over-TCP answer
        // arrives here as ordinary payload and counts exactly as much as an upstream's bytes do.
        if admitting {
            self.rearm(id.handle, id.worker, now);
        }
    }

    /// Moves at most one queued chunk into this flow's row, which is what a consumed row is followed by.
    ///
    /// Validated on both halves first, because smoltcp reuses handles and the queue read here belongs to
    /// whichever flow holds that handle now. Everything else is
    /// [vpnhotspotd::shared::transfer::Transfer::refill]'s, including why taking one at a time is what keeps
    /// the producer's order - and a chunk the fair queue would not take is counted here rather than at either
    /// call site, so both of them get the count and neither has to remember to.
    fn refill(&mut self, id: FlowId<SocketHandle>) -> Refilled {
        if !self.flows.current(&id.handle, id.worker) {
            return Refilled::Idle;
        }
        let Engine {
            flows,
            fair,
            counters,
            ..
        } = self;
        let Some(held) = flows.get_mut(&id.handle) else {
            return Refilled::Idle;
        };
        let refilled = held.record.transfer.refill(id, fair);
        if matches!(refilled, Refilled::Stale) {
            counters.stale += 1;
        }
        refilled
    }

    /// Moves bytes between the stack and the flow tasks. Returns whether anything moved, so the caller knows
    /// to poll again.
    pub(super) fn pump(&mut self) -> bool {
        // Upstream to client first, because a full stack send buffer is what throttles the remote.
        let mut moved = self.pump_to_client();
        moved |= self.pump_to_upstream();
        moved
    }

    /// One fair round toward the clients: every flow that was ready when the round began is offered its turn
    /// before any flow gets a second one.
    ///
    /// The budget is taken at the start, so a flow that rotates to the back after a short write does not
    /// extend the round, and a flow signalling readiness in a tight loop cannot make the round its own. A flow
    /// whose client has stopped reading keeps its exact offset and its turn passes to the next one; nothing
    /// about it reaches any other flow.
    fn pump_to_client(&mut self) -> bool {
        let mut moved = false;
        let mut round = self.fair.begin_round();
        while let Some(id) = self.fair.next(&mut round) {
            // Before anything is consumed, and for payload and ordered end of stream alike: a client half
            // that has not finished handshaking cannot take bytes yet, and taking its place in the round away
            // is how a payload gets stranded. Its own queue will not bring it back either, because the row is
            // holding that payload and a busy row is not polled. So the place in the round goes back, nothing
            // is consumed, and `moved` deliberately stays as it was: reporting progress here would spin
            // [Engine::poll] on a socket that cannot make any. The client's own final ACK is what runs this
            // again.
            //
            // A half whose send side is *over* is the opposite case and falls through to the send below,
            // where the error leaves it on the retirement path - see [lifetime::handshaking].
            if self
                .socket(id.handle)
                .is_some_and(|socket| lifetime::handshaking(socket.state()))
            {
                self.fair.mark_ready(id);
                continue;
            }
            // A row emptied by anything other than the write below is refilled before the turn is spent on
            // it. What the refill answers is nothing this turn needs: whether the row has anything is what
            // the peek below says.
            self.refill(id);
            let Some(pending) = self.fair.peek(id) else {
                // Nothing owed but this flow's place in the round, or an ordered end of stream with no
                // payload before it. Both are settled by acknowledging with nothing sent.
                if matches!(self.fair.serviced(id, 0), Progress::Eof) {
                    self.acknowledge(id);
                    moved = true;
                }
                continue;
            };
            // Capped at the read quantum in this direction too, so one flow's chunk cannot be an arbitrarily
            // long turn: what does not go now keeps its exact offset and comes back next round.
            let offered = &pending[..pending.len().min(READ_CHUNK)];
            // The socket is reached through the fields rather than through [Engine::socket], because the
            // pending slice above borrows the fair queue and a `&mut self` helper would take the whole owner
            // with it. Written out, the two borrows are of different fields and the payload never has to be
            // copied to satisfy them.
            if !self.flows.contains(&id.handle) {
                self.counters.stale += 1;
                continue;
            }
            let sent = match self
                .sockets
                .get_mut::<Socket>(id.handle)
                .send_slice(offered)
            {
                Ok(sent) => sent,
                // The client half is gone; the flow's own close follows, and nothing here has to force it.
                Err(_) => {
                    moved = true;
                    continue;
                }
            };
            if sent > 0 {
                moved = true;
                self.counters.to_client += sent as u64;
            }
            match self.fair.serviced(id, sent) {
                // The whole chunk went, so the row is free for the next one this flow has queued. Refilled
                // here rather than waited for: nothing wakes this owner for a chunk queued while the row was
                // busy - the queue of a busy row is deliberately not polled - so consumption is what takes
                // the next one, and delivering it is also what puts this flow back in the round-robin order.
                // A DNS-over-TCP transport is told instead, because its next piece does not exist until it
                // is.
                Progress::Consumed | Progress::Eof => {
                    self.acknowledge(id);
                    self.refill(id);
                    moved = true;
                }
                // Still owed. The offset is exact and the flow is already back in the order.
                Progress::Blocked | Progress::Idle => {}
            }
        }
        moved
    }

    /// Tells one DNS-over-TCP transport that its piece has been consumed.
    ///
    /// Only that kind, because only that kind waits: an ordinary flow's producer is freed by this owner
    /// *taking* a chunk, which the queue itself reports, while a resolver transport may not build its next
    /// piece until the one before it is gone. Depth one and never awaited: the transport takes the previous
    /// acknowledgment before it frames the next piece, so the slot is free. A full slot or a gone receiver
    /// both mean the flow is on its way out, which its own terminal settles.
    fn acknowledge(&mut self, id: FlowId<SocketHandle>) {
        let unacknowledged = self.flows.get(&id.handle).is_some_and(|flow| {
            flow.record.worker == id.worker
                && flow.record.kind == Kind::Resolver
                && flow.record.consumed.try_send(()).is_err()
        });
        if unacknowledged {
            self.counters.unacknowledged += 1;
        }
    }

    /// One fair round toward the upstreams, in the same explicit order and with the same quantum.
    ///
    /// A `HashMap` iteration would be an arbitrary order that changes when the map is resized, which is not
    /// fairness but a different unfairness each time - and the quantum is what stops one flow with a full
    /// 64 KiB receive buffer from being the whole round.
    fn pump_to_upstream(&mut self) -> bool {
        let mut moved = false;
        for _ in 0..self.outgoing.len() {
            let Some(handle) = self.outgoing.pop_front() else {
                break;
            };
            self.outgoing.push_back(handle);
            // Only ever read as much as the flow task can take right now: leaving the rest in the stack's
            // receive buffer is what closes the client's window instead of buffering here. The question is
            // whether this owner is *holding* that flow's slot rather than what the queue's capacity says,
            // because a slot in hand cannot be taken away between reading it and using it - and asking for
            // one is what registers the wake for when the task frees it.
            let room = self
                .flows
                .get(&handle)
                .is_some_and(|flow| flow.record.transfer.reserved());
            let finished = self
                .flows
                .get(&handle)
                .is_some_and(|flow| flow.record.transfer.finished());
            // Read from the *phase* rather than from one state, because a third-handshake ACK that also
            // carries FIN opens and half-closes this connection in a single step and `Established` is never
            // observable for it - see [lifetime::opened]. Watching for that one state left such a flow
            // believing it had never opened, so the half-close below was never propagated and its upstream
            // peer waited for bytes nobody would send.
            if self
                .socket(handle)
                .is_some_and(|socket| lifetime::opened(socket.state()))
            {
                if let Some(flow) = self.flows.get_mut(&handle) {
                    flow.record.established = true;
                }
            }
            let Some(socket) = self.socket(handle) else {
                continue;
            };
            if finished && socket.may_send() {
                // the remote finished sending, so the client is told the same way rather than reset
                socket.close();
                moved = true;
            }
            if !room || !socket.can_recv() {
                continue;
            }
            // Bounded by the quantum rather than by whatever the receive buffer holds: a whole 64 KiB copied
            // into one slot is one flow's turn lasting as long as it likes, and the rest stays where it is
            // until this flow comes round again.
            let mut chunk = Vec::new();
            if socket
                .recv(|data| {
                    let taken = data.len().min(READ_CHUNK);
                    // Exactly what was taken, so the buffer this flow is charged for is the size it really
                    // holds rather than whatever an amortised growth happened to reserve.
                    chunk = data[..taken].to_vec();
                    (taken, ())
                })
                .is_err()
                || chunk.is_empty()
            {
                continue;
            }
            self.counters.to_upstream += chunk.len() as u64;
            // Counted from here to wherever it is dropped - queued, taken by the flow task, or discarded with
            // a receiver that is gone - because that whole span is one of the chunk-sized terms in
            // [crate::shizuku::tcp]'s per-flow footprint.
            let chunk = Owned::new(chunk);
            moved = true;
            if let Some(flow) = self.flows.get_mut(&handle) {
                if let Err(unsent) = flow.record.transfer.send(chunk) {
                    // the slot was in hand immediately above and this engine is the only sender, so a
                    // refusal here means the receiver is gone and the flow's close is already on its way.
                    // Dropped where it was refused, because the buffer is this owner's again.
                    drop(unsent);
                    self.counters.stale += 1;
                }
            }
        }
        // A client that half-closed stops the upstream write half, which the task sees as its channel
        // closing. Walked over the round-robin order in place rather than collected: the order already holds
        // each live handle exactly once, and a list here would be scratch proportional to live flows that no
        // lease covers, on the path every poll takes.
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
            if held.record.established
                && held.record.transfer.sending()
                && !sockets.get::<Socket>(*handle).may_recv()
            {
                // Giving up the slot releases whatever reservation it held and closes the queue with it, so
                // the task reads what is still queued and then sees the half-close.
                held.record.transfer.stop_sending();
                moved = true;
            }
        }
        moved
    }
}
