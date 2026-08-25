//! How a terminating flow ends, and the one place everything it owns is given back.
//!
//! The four endings are one decision with one exit. What they have in common is the fence: a flow's record,
//! its socket and its charge may only be released once every physical owner covered by its grants is gone,
//! and [Engine::reclaim] is the only code that releases them.
//!
//! # A worker finishing is not the same as a flow ending
//!
//! Both workers return as soon as *their* ordered work is done - the upstream half-close is written, the
//! remote's end of stream has been handed over, and the client's stack has taken it. The client's own
//! teardown is not finished at that moment: the socket is typically in `LAST-ACK`, `CLOSING` or `TIME-WAIT`,
//! with a FIN to retransmit and a final acknowledgment still to come. Removing the flow there took the
//! client's half of the connection away mid-teardown, so a lost FIN could never be retransmitted and the
//! client's acknowledgment arrived at nothing.
//!
//! So there are four endings rather than one:
//!
//! - a **clean terminal** from a flow nobody cancelled, whose client is past its handshake and not yet
//!   `Closed`, *detaches* the flow: the worker's descriptor went with its task, and what stays is
//!   client-side state with no task of its own and no per-flow timer task behind it - its teardown is still
//!   *scheduled*, by smoltcp's own timers through the engine's combined deadline, which is what lets the FIN
//!   be retransmitted;
//! - a **detached flow's client finishing**, which this owner finds by scanning its own rows - the same
//!   shape [crate::tcp_dns::Transactions] uses, and for the same reason;
//! - a **failed or reported terminal**, which is not a clean completion: it resets its client and ends the
//!   flow at once;
//! - a **retirement or shutdown**, which is [Engine::retire]'s, and which settles a detached row directly
//!   because no terminal will ever name it again.
//!
//! Every flow is removed and refunded exactly once, but not necessarily by the ending that first reached it:
//! a clean terminal is an intermediate handoff rather than a removal, and the flow it detached is removed
//! later by its client finishing, by its floor, by a retirement, or by shutdown - whichever comes first.
//!
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{Socket, State};
use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::dns_debt;
use vpnhotspotd::shared::fair::FlowId;

use super::{lifetime, Engine, Flow};
use crate::output::Output;
use crate::report;
use crate::tcp_dns;
use crate::workers::{Ended, Terminal};

/// Which of the engine's owners a completion belongs to.
pub(crate) enum Finished {
    Flow(Terminal<SocketHandle>),
    /// A flow whose worker finished cleanly earlier and whose client side has now finished too. Its exact
    /// identity rather than a terminal, because there is no task left to produce one - see [Engine::settled].
    Detached {
        handle: SocketHandle,
        worker: u64,
    },
    /// A resolver transaction that outlived the flow which asked for it. A value rather than a terminal
    /// message, because this owner polls its rows itself - see [crate::tcp_dns].
    Transaction(tcp_dns::Settlement),
}
impl Engine {
    /// The next of this engine's owners to have something finished. Selected on by the owning task, so it
    /// waits forever while nothing is running rather than answering at once.
    ///
    /// Three kinds, because they are settled differently. A flow's terminal *may* retire the flow, and on the
    /// clean path does not: it detaches it instead and the client's teardown carries on. A detached flow's
    /// client finishing is the second, and is the one that actually retires such a flow. A transaction's only
    /// refunds a resolver slot - it settles when the platform is actually done, which can be well after the
    /// config that swept its flow was acknowledged. Both awaited arms are cancellation-safe, which is what
    /// lets the ingress task abandon this for another and come back: `JoinSet::join_next_with_id` is, and so
    /// is the transaction table's own scan - see [tcp_dns::Transactions::finished].
    pub(crate) async fn finished(&mut self) -> Finished {
        // Answered before the select rather than as a third arm of it, for two reasons. It is not a wait:
        // what it looks at is state this owner already holds, and the only thing that can change it is work
        // this owner just did - every transition to `Closed` comes from a packet or a poll this loop
        // performed, and the loop re-enters here immediately afterwards, so there is no waker to register.
        // And both arms below borrow the tables it reads. Same shape as
        // [tcp_dns::Transactions::finished]: an owner polling its own rows rather than a task per row.
        if let Some(detached) = self.detached() {
            return detached;
        }
        tokio::select! {
            biased;
            terminal = self.flows.finished() => Finished::Flow(terminal),
            settlement = self.queries.finished() => Finished::Transaction(settlement),
        }
    }

    /// The next detached flow whose client side has finished, if any has.
    fn detached(&self) -> Option<Finished> {
        self.flows.iter().find_map(|(handle, held)| {
            (held.record.detached && self.sockets.get::<Socket>(*handle).state() == State::Closed)
                .then_some(Finished::Detached {
                    handle: *handle,
                    worker: held.record.worker,
                })
        })
    }

    /// Settles one detached flow whose client side has finished.
    ///
    /// The counterpart of [Engine::close] for a flow that has no worker left to report: everything physical
    /// it still owned - its socket and both stack buffers, its own channel endpoints, whatever its DNS state
    /// was holding - dies here, and only then is its grant released. Validated on both halves first, because
    /// smoltcp reuses handles and this identity was read a moment before the caller acted on it.
    pub(crate) fn settled(&mut self, handle: SocketHandle, worker: u64, admission: &mut Admission) {
        if !self.flows.current(&handle, worker) {
            self.counters.stale += 1;
            return;
        }
        drop(self.fair.begin_retire(FlowId::new(handle, worker)));
        self.reclaim(handle, worker, admission);
    }

    /// Takes one finished worker's terminal, which either detaches its flow or ends it.
    ///
    /// The order is the fence. The task is complete before this runs, so the upstream socket is closed - by
    /// the task's own drop, abortively when swept. What happens next depends on the ending: a clean terminal
    /// from a flow nobody cancelled whose client is still closing *detaches* it and returns, leaving the
    /// socket, the grant and the DNS state in place for the teardown to finish; anything else - a failure, a
    /// cancelled flow, a client already `Closed` - goes on to remove the flow, refund the reservation, and let
    /// the config the retirement belonged to be acknowledged. A detached flow gets no second terminal, so
    /// [Engine::settled] is what ends it later.
    pub(crate) fn close(
        &mut self,
        terminal: Terminal<SocketHandle>,
        admission: &mut Admission,
        output: &mut Output,
    ) {
        let Terminal { key, id, ended } = terminal;
        // The pair is validated before *anything* is done with it, and that is the whole of the ordering
        // change here. smoltcp reuses handles, so a terminal naming a handle whose flow has already been
        // replaced would otherwise reset, report on and remove the successor - a live client's connection torn
        // down by its predecessor's ending. Nothing below touches a socket, writes a packet or prints a line
        // until this has passed.
        if !self.flows.current(&key, id) {
            self.counters.stale += 1;
            return;
        }
        // Discarded before the worker is released and before anything else, per exact identity: a clean
        // terminal may not take a flow's unacknowledged bytes with it, and a cancelled one may only bypass
        // that wait once the owner has committed to dropping what the wait was for. Idempotent, because the
        // poll above may already have begun this exact retirement when it saw the socket close.
        let flow_id = FlowId::new(key, id);
        drop(self.fair.begin_retire(flow_id));
        // A clean terminal from a flow nobody asked to stop, whose client side is still finishing, hands the
        // flow on rather than ending it. The worker's own state is already gone - its task ran to completion,
        // so the upstream descriptor and everything the future owned went with it - and what stays is the
        // client's half of a teardown that has a FIN to retransmit and an acknowledgment to wait for.
        //
        // Two exclusions, and both are "there is no teardown here to protect". A cancelled worker also
        // reports `Expected`, and there the socket has already been aborted by whoever cancelled it. And a
        // socket that never got past its handshake - or is already `Closed` - has no connection whose closing
        // could be cut short, which is what [lifetime::opened] answers.
        if matches!(ended, Ended::Expected)
            && !self
                .flows
                .get(&key)
                .is_some_and(|held| held.cancel.is_cancelled())
            && lifetime::opened(self.sockets.get::<Socket>(key).state())
        {
            if let Some(held) = self.flows.get_mut(&key) {
                held.record.detached = true;
                // Nothing reads it any more - the receiver went with the task - and leaving it would make
                // every later pump try to send into a closed channel and count a stale event for it.
                held.record.downstream = None;
            }
            self.counters.detached += 1;
            return;
        }
        let reset = match ended {
            Ended::Expected => false,
            Ended::Reported(reason) => {
                // once per flow, not per packet, so this cannot flood
                report::stdout!("tcp flow closed: {reason}");
                self.reset(key)
            }
            Ended::Failed { context, error } => {
                match self.flows.get(&key) {
                    Some(flow) => report::io_with_details(
                        context,
                        error,
                        [
                            ("client", flow.record.client),
                            ("destination", flow.record.destination),
                        ],
                    ),
                    None => report::io(context, error),
                }
                self.reset(key)
            }
        };
        // Emitted before the socket is removed, because a reset is a packet the stack has not sent yet and a
        // smoltcp socket only advances when polled: removing it first would abort the connection and tell the
        // client nothing. Only when there was one, so a mass retirement does not poll once per flow.
        if reset {
            self.poll(output);
        }
        self.reclaim(key, id, admission);
    }

    /// Gives back everything one flow owns, once every physical owner covered by its grants is gone.
    ///
    /// The one place a flow leaves, reached from all four endings: a worker's terminal, a detached flow whose
    /// client side finished, a config retirement, and the session's own shutdown. One place because the order
    /// inside it is the accounting invariant - the socket and this flow's own endpoints die before the DNS
    /// state releases the grant covering them, and the connection's grant goes last of all.
    fn reclaim(&mut self, key: SocketHandle, id: u64, admission: &mut Admission) {
        self.fair.finish_retire(FlowId::new(key, id));
        self.outgoing.retain(|handle| *handle != key);
        let Some(flow) = self.flows.retire(&key, id) else {
            // Unreachable: every caller has validated this exact pair and nothing since has awaited.
            self.counters.stale += 1;
            return;
        };
        self.sockets.remove(key);
        let Flow {
            connection,
            chunks,
            consumed,
            serving,
            downstream,
            ..
        } = flow;
        // Physical before accounting, and this flow's own endpoints go first because one of them can still be
        // holding a buffer the DNS state is about to give the capacity back for. The mailbox is the one that
        // matters: [crate::mailbox::Mailbox::hand_over] puts a piece of an answer into it and *then* waits to
        // be acknowledged, so a transport cancelled inside that wait leaves a `Chunk::Payload` sitting here -
        // and that piece is one of the three buffers the parked delivery's grant covers. Releasing the
        // delivery first would give those bytes back while this receiver still owned them.
        //
        // The task is already complete, so dropping these is the close: the upstream descriptor goes with the
        // task, and both stack buffers go with the socket removed above.
        drop(chunks);
        drop(consumed);
        drop(downstream);
        // Then everything this transport's DNS state still owned, in the same order within itself: the parked
        // delivery nobody will acknowledge, the query still travelling back on the owner's own channel, both
        // control endpoints, and only then the reservation's grant. See [tcp_dns::Serving::close].
        let closed = serving.close(admission);
        let transaction = closed.transaction;
        // One call, which is what keeps the two halves of this from disagreeing: a DNS-over-TCP transport
        // whose question is still outstanding hands that question its own token rather than giving it back -
        // the platform's slot is still taken, and a moment where the token looked free would admit a second
        // query the limiter has no room for - while a transport that closed idle simply releases it with the
        // rest of its grant. Neither path touches the query's own bytes: the resolver still holds them.
        let mut connection = connection;
        connection.asking(transaction);
        let debt = transaction.and_then(|transaction| self.queries.debt(transaction));
        if let Err(stranded) = dns_debt::close(admission, connection, debt) {
            // The token did not reach the question this transport says is still outstanding, so it may not go
            // back into circulation: the platform's slot for that question is taken and nothing here can
            // observe its end. The grant came back rather than being released, and the one place a token
            // nobody can account for lives is the transaction table's own - see
            // [tcp_dns::Transactions::strand].
            self.counters.unsettled += 1;
            self.queries.strand(admission, stranded);
        }
        self.counters.closed += 1;
    }

    /// Tells the client its upstream half is gone, the one way a terminated flow can: a reset. This is also
    /// the unreachable-destination path, since the handshake already completed. `false` means there was no
    /// socket left to say it with.
    fn reset(&mut self, handle: SocketHandle) -> bool {
        match self.socket(handle) {
            Some(socket) => {
                socket.abort();
                self.counters.reset += 1;
                true
            }
            None => false,
        }
    }
}
