//! The engine's side of a DNS-over-TCP flow: what a transport may ask its owner for, and what a settled
//! transaction becomes.
//!
//! Here rather than in [crate::tcp] because it is one cohesive decision made in four places - admit a length,
//! accept the query it was admitted for, classify the answer against the config that query belongs to, and
//! end the delivery when the client's stack has really taken it - and because the engine file it came out of
//! had grown past the point where any of that could be read in one sitting.
//!
//! Every entry point below validates the flow on *both* halves of its identity first, because smoltcp reuses
//! handles: a request naming only a handle could be admitted against, published for, or released from
//! whatever flow reused it.

use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::dns_debt;
use vpnhotspotd::shared::failure::Failure;

use super::{Engine, Flow};
use crate::owned::Owned;
use crate::report;
use crate::tcp_dns::{self, Submitted};
use crate::tcp_flow::Event;
use crate::workers::Held;

impl Engine {
    /// Answers one thing a DNS-over-TCP transport asked its owner for.
    ///
    /// Three questions, and each of them is an accounting decision a worker cannot make for itself: may this
    /// announced length be stored, may this exact query be published, and may this delivery end.
    ///
    /// `admitting` is the session's current admission state, and only the first two questions read it. A
    /// `STOPPING` session may drain what it already owns and may create nothing: a reservation this owner has
    /// not accepted yet allocates nothing, a query it accepted before the stop is answered from the capacity
    /// that reservation already holds rather than becoming platform work, and a delivery already parked is
    /// acknowledged and released exactly as it would have been.
    pub(crate) fn ask(&mut self, ask: tcp_dns::Ask, admitting: bool, admission: &mut Admission) {
        match ask {
            tcp_dns::Ask::Reserve { flow, length } => {
                self.reserve_query(flow, length, admitting, admission)
            }
            tcp_dns::Ask::Query(flow) => self.commit_query(flow, admitting, admission),
            // The transport has acknowledged the last chunk and dropped its result and framing buffers, so
            // the delivery grant may end. Validated on both halves first: a report naming a handle whose flow
            // has been replaced would end the successor's delivery instead of its predecessor's.
            tcp_dns::Ask::Delivered { flow, delivery } => {
                // Both identities, and both before anything is released. The flow says the transport is one
                // this owner still holds - handles are reused, so a report from a replaced flow would
                // otherwise reach its successor - and the delivery says *which answer* it is about, because a
                // transport asks one question after another and a late acknowledgment for a finished one
                // would release its successor's grant while those bytes are still being framed.
                let Some(held) = self.serving(flow) else {
                    self.counters.stale += 1;
                    return;
                };
                match held.record.serving.acknowledge(admission, delivery) {
                    dns_debt::Acked::Released => {}
                    // A duplicate, or one whose answer the flow's close already ended, or one naming a
                    // delivery that is not the parked one. None of them releases anything.
                    dns_debt::Acked::Mismatched | dns_debt::Acked::Absent => {
                        self.counters.stale += 1
                    }
                }
            }
        }
    }

    /// The exact flow an identity names, or nothing when it names one this owner no longer holds.
    fn serving(&mut self, flow: Event) -> Option<&mut Held<Flow>> {
        if !self.flows.current(&flow.handle, flow.worker) {
            return None;
        }
        self.flows.get_mut(&flow.handle)
    }

    /// Admits one query at the length its client announced, before a byte of it has been stored.
    ///
    /// This is where the message a client can announce becomes an allocation the aggregate agreed to. What
    /// comes back to the transport is either a buffer of exactly that length - which the framing may fill and
    /// cannot grow - or a refusal, which the transport answers by skipping those bytes so the stream stays
    /// framed for the next question.
    ///
    /// A stale transport is answered with nothing at all: it is on its way out, its own close settles what it
    /// held, and reserving capacity for it would be capacity nothing will hand back. Its cancellation, not a
    /// reply, is what ends the wait it is in.
    fn reserve_query(
        &mut self,
        flow: Event,
        length: usize,
        admitting: bool,
        admission: &mut Admission,
    ) {
        if self.serving(flow).is_none() {
            self.counters.stale += 1;
            return;
        }
        // A session that has stopped serving admits no new exchange. Refused rather than ignored, which is
        // the difference between a stream that stays framed - the transport skips the announced bytes and the
        // client may ask again - and one left waiting on a question nobody took.
        if !admitting {
            self.counters.denied += 1;
            self.grant(flow, tcp_dns::Granted::Denied);
            return;
        }
        // One query at a time per transport, which is what its single logical token means. A second
        // reservation while one is outstanding would be a second exchange this connection never paid for.
        if self
            .flows
            .get(&flow.handle)
            .is_some_and(|held| held.record.serving.reserving())
        {
            self.counters.denied += 1;
            self.grant(flow, tcp_dns::Granted::Denied);
            return;
        }
        let Some((reserved, query)) = self.queries.reserve(length, admission) else {
            self.counters.denied += 1;
            self.grant(flow, tcp_dns::Granted::Denied);
            return;
        };
        // Held by this owner, on the flow that asked, so a sweep between here and the query arriving ends it
        // exactly once - see [crate::tcp_dns::Serving].
        let Some(held) = self.flows.get_mut(&flow.handle) else {
            // Unreachable: the pair was validated above and nothing since has awaited.
            self.counters.stale += 1;
            drop(query);
            reserved.end(admission);
            return;
        };
        held.record.serving.reserve(reserved);
        held.record.serving.grant(tcp_dns::Granted::Admitted(query));
    }

    /// Answers a reservation on a flow this owner still holds. Silent for one it does not, whose transport is
    /// already being cancelled.
    fn grant(&mut self, flow: Event, granted: tcp_dns::Granted) {
        if let Some(held) = self.flows.get_mut(&flow.handle) {
            held.record.serving.grant(granted);
        }
    }

    /// Accepts one exact validated query from the transport that framed it, and publishes it.
    ///
    /// **This is the commit boundary the per-query handoff turns on.** The stamp and the selected network are
    /// sampled here, together, from the config current at this moment - never inherited from whenever the flow
    /// was opened, and never read again afterwards. A request still queued when a config wins is therefore
    /// published under the successor, and one this owner has already accepted cannot be overtaken by that
    /// config's acknowledgement: both run in this owner's own serial order, and the acknowledgement is sent
    /// after the apply that preceded it returned.
    ///
    /// Nothing travels here but the identity. The query itself comes back on the depth-one channel this owner
    /// kept when it granted the capacity, so a buffer waiting to be accepted is one the flow's close can end
    /// before it refunds - rather than one sitting in a shared queue while its grant is given back.
    fn commit_query(&mut self, flow: Event, admitting: bool, admission: &mut Admission) {
        let Some(held) = self.serving(flow) else {
            self.counters.stale += 1;
            return;
        };
        let Some((reserved, query)) = held.record.serving.accept() else {
            // Nothing was admitted for this transport, so there is no query to publish and no grant to end.
            self.counters.stale += 1;
            return;
        };
        let Some(query) = query else {
            // Unreachable: the transport hands the buffer over before it says so. Ended rather than assumed
            // away, because a reservation nobody consumes is capacity nothing gives back.
            self.counters.stale += 1;
            return reserved.end(admission);
        };
        // Sampled together and now: which selection this query goes out on, and which config it belongs to.
        // A query with no descriptor behind it never had a transaction to open, so it takes the same path as
        // one with no network to resolve on - which is also what a transport opened before any config had
        // selected a network gets, on a stream that then resolves normally once one arrives.
        // `admitting` is read here rather than where the reservation was granted, because this is the commit
        // boundary: a query accepted before the session stopped serving is already this owner's to answer,
        // and one whose commit was still queued when the stop won must not become platform work a stopping
        // session cannot observe the end of. Either way the reservation covers the answer built below.
        let published = self
            .upstream
            .filter(|_| admitting && reserved.submittable())
            .map(|network| (network, self.stamp));
        let Some((network, stamp)) = published else {
            // No selected network, or no descriptor: the client is answered here rather than left waiting on
            // a question nobody took, and its stream carries on.
            return self.answer_here(flow, reserved, query, admission);
        };
        match self
            .queries
            .submit(network, stamp, flow, reserved, query, admission)
        {
            Submitted::Outstanding(transaction) => {
                // Remembered so that a transport closing over a question still in flight can hand that
                // question its own token rather than charging a second.
                if let Some(held) = self.flows.get_mut(&flow.handle) {
                    held.record.serving.asking(Some(transaction));
                }
                self.counters.resolved += 1;
            }
            // The table would have had to grow, which the room check at reservation makes unreachable. The
            // reservation and its query come back whole, the platform was never asked, and the client gets
            // that query's own SERVFAIL on a stream that carries on.
            Submitted::Refused(reserved, query) => {
                self.counters.unprepared += 1;
                self.answer_here(flow, reserved, query, admission);
            }
            // The platform took the question and this process can no longer watch it. Everything physical is
            // already back; what is left is the one logical token, which belongs to the *transport* rather
            // than to the query - so it is moved out of this flow's own grant and quarantined for the rest
            // of the session, and only then is the transport ended. It cannot carry on: its next question
            // would be one asked under a token that no longer exists.
            Submitted::Unobservable {
                transaction,
                failure,
            } => {
                self.counters.unsettled += 1;
                self.end_unobservable(flow, transaction, failure, admission);
            }
        }
    }

    /// Quarantines one transport's logical token and ends its stream, because the platform is holding a slot
    /// this process can no longer watch.
    ///
    /// The token is per transport for DNS-over-TCP, so the flow's own connection is the grant really holding
    /// it. If the move cannot be represented, the question is deliberately *left recorded* on the flow: its
    /// close then finds an outstanding question with no debt for it, which is exactly the state
    /// [vpnhotspotd::shared::dns_debt::close] refuses to release a token from, so it comes back here through
    /// [Engine::close] instead of going back into circulation.
    fn end_unobservable(
        &mut self,
        flow: Event,
        transaction: u64,
        failure: Failure,
        admission: &mut Admission,
    ) {
        let moved = self.quarantine(flow, admission);
        match self.flows.get_mut(&flow.handle) {
            Some(held) => {
                held.record.serving.asking((!moved).then_some(transaction));
                // The failure goes with the refusal rather than being reported here: it ends this transport,
                // and a transport ending on a local failure is reported once at its terminal - see
                // [Engine::close] - which is the same place a question lost later is reported from.
                held.record.serving.refuse(failure);
            }
            // No transport left to carry it, so this is the last owner that can say the platform is holding a
            // slot this process cannot watch. Without this the outcome would be silent in exactly the case
            // where nothing else will speak for it.
            None => crate::resolver::report_unobservable(transaction, &failure),
        }
    }

    /// Moves one flow's logical token out of its own grant and onto the transaction table's, for the session.
    ///
    /// `false` is counted rather than believed: it would be capacity this session goes on thinking it has.
    fn quarantine(&mut self, flow: Event, admission: &mut Admission) -> bool {
        // Destructured so the table holding the grant and the table holding the quarantine can be borrowed
        // at once: they are disjoint fields, which a `&mut self` helper would hide.
        let Engine {
            flows,
            queries,
            counters,
            ..
        } = self;
        let Some(held) = flows.get(&flow.handle) else {
            counters.stale += 1;
            return false;
        };
        if queries.quarantine(admission, held.record.connection.lease()) {
            return true;
        }
        counters.unsettled += 1;
        false
    }

    /// Answers one query this daemon will not submit, and parks its delivery on the flow that asked.
    ///
    /// The same shape as a settled transaction's answer, deliberately: it is parked before the transport can
    /// see it, acknowledged by the same identity, and released by the same path. What differs is only that no
    /// descriptor was opened and the platform was never asked.
    fn answer_here(
        &mut self,
        flow: Event,
        reserved: tcp_dns::Reserved,
        query: Owned,
        admission: &mut Admission,
    ) {
        self.counters.answered_here += 1;
        let Some(held) = self.flows.get_mut(&flow.handle) else {
            // Unreachable: validated by the caller with nothing awaited since. The query goes before the
            // grant that covered it, like every other buffer on this path.
            self.counters.stale += 1;
            drop(query);
            return reserved.end(admission);
        };
        let serving = &mut held.record.serving;
        let Some(answering) = tcp_dns::answered_here(reserved, query, serving, admission) else {
            // Too malformed for anything to be echoed back. The transport has been told, and it ends the
            // stream rather than leaving a client waiting on a question nothing can answer.
            return;
        };
        if answering.hand_over(admission, serving) {
            self.counters.unsettled += 1;
        }
    }

    /// Settles one finished resolver transaction, in the one order that does not lose an acknowledgment.
    ///
    /// Classify, park, *then* hand the answer over. The transport is awaiting an answer it cannot see until
    /// this has run, so the "delivered" report it sends afterwards necessarily finds the delivery already
    /// parked on its flow. The ordering used to be the other way round - the answer travelled straight from
    /// the resolver task to the transport - so a transport that framed and acknowledged promptly reported
    /// against a flow holding nothing, the report did nothing, and the grant stayed parked until the flow
    /// closed. [tcp_dns::Answering::hand_over] is what makes the wrong order unspellable rather than merely
    /// unwritten: the answer is inside the settled delivery, classification happens before the park, and
    /// parking is the only way out of it.
    pub(crate) fn settle(&mut self, settlement: tcp_dns::Settlement, admission: &mut Admission) {
        let transaction = settlement.key();
        let Some(mut delivered) = self.queries.settle(settlement, admission) else {
            // The table kept this query's whole grant because the token a closing transport had handed it
            // could not be moved anywhere it would not be reused. There is nothing to deliver and nothing to
            // tell a transport: a token on the debt means the transport that asked closed to put it there, so
            // no live flow can be holding this transaction. Counted *and* reported where it happened, in the
            // table itself, because that is the owner that destroys the state - see
            // [tcp_dns::Transactions::settle].
            return;
        };
        // Exact identity, both halves, rather than a scan for whichever flow claims this transaction id.
        // smoltcp reuses handles, so a predecessor's answer must never reach the flow that took its place -
        // and the worker is what tells those two apart.
        let asked = delivered.flow();
        let live = self.flows.current(&asked.handle, asked.worker);
        // Read once, because it decides two separate things below: where this query's token goes, and whether
        // the generation is allowed to have an opinion about the answer at all.
        let unobservable = delivered.unobservable();
        // The platform is holding a slot whose end this process stopped being able to watch. The table has
        // already moved whatever token *it* was holding; a live transport's own token is still on its
        // connection, so it is moved here. Only the token: what ends this stream is the classification below,
        // which is already an unanswerable local failure and sends exactly one refusal for it.
        if unobservable && live {
            let moved = self.quarantine(asked, admission);
            if let Some(flow) = self.flows.get_mut(&asked.handle) {
                // Left recorded when the move did not happen, so this flow's close refuses to hand the token
                // back and comes through [Engine::close]'s stranded path instead.
                flow.record.serving.asking((!moved).then_some(transaction));
            }
        } else if live {
            // The flow that asked has no question outstanding any more, so a close from here on releases its
            // own token rather than trying to hand it to a transaction that has already settled. Only when it
            // is *this* transaction: a flow whose question was replaced still owes the one it has now.
            if let Some(flow) = self.flows.get_mut(&asked.handle) {
                if flow.record.serving.transaction() == Some(transaction) {
                    flow.record.serving.asking(None);
                }
            }
        }
        let stamp = delivered.stamp();
        // An epoch change may have put a different device behind this client tuple, so there is nobody an
        // answer *or* a refusal about it can honestly be sent to. Absent, closed or reused flows are the
        // same silence for the same reason.
        if !live || stamp.epoch != self.stamp.epoch {
            self.counters.stale += 1;
            // Silent on the wire, not in the log. There is no transport left to carry this failure to a
            // terminal, so for the one outcome that is this daemon's own - the platform holding a slot
            // nothing here can watch - this is the last owner that can say it, and it says it before the
            // settlement is destroyed. An ordinary stale answer says nothing about the daemon and is not
            // reported: the client that asked is simply no longer there.
            if unobservable {
                if let Some(failure) = delivered.refusal() {
                    crate::resolver::report_unobservable(transaction, failure);
                }
            }
            delivered.discard(admission);
            return;
        }
        // Resolved on a selection this session has stopped claiming. The transport itself survived the
        // handover untouched, so the client is told to try again rather than left waiting - but the answer
        // that came back over the retired network is dropped before that refusal is built. See
        // [tcp_dns::Delivered::stale].
        //
        // Only for an outcome this owner could actually observe, and that condition is the whole of the fix.
        // The replacement below exists for a *predecessor's answer* on a transport that legitimately still
        // owns its logical token and may therefore ask again; an unobservable outcome is the opposite of that
        // on both counts. Its token has just been quarantined, so the transport can never carry another
        // query, and [tcp_dns::Delivered::stale] replaces every result it is given - including this one's
        // local failure - with an `Ok(SERVFAIL)`. Composed, the two would tell a client to retry on a
        // connection that has nothing left to retry with, and would overwrite the one refusal that ends it.
        //
        // Whether the two can compose in production is a question about Tokio's readiness internals rather
        // than about this owner: see the module note in [crate::resolver]. Making the classification total
        // costs one condition and stops it being a question at all.
        if stamp.generation != self.stamp.generation && !unobservable {
            report::stdout!(
                "discarding a DNS-over-TCP answer resolved on network {} at generation {}",
                delivered.network(),
                stamp.generation
            );
            if !delivered.stale() {
                // No SERVFAIL could be formed from that query, so there is nothing to send and nothing left
                // to park; this is the last owner that can end these bytes.
                delivered.discard(admission);
                return;
            }
        }
        if !delivered.has_answer() {
            // Nothing was produced at all, so nobody will ever acknowledge these bytes.
            delivered.discard(admission);
            return;
        }
        let Some(flow) = self.flows.get_mut(&asked.handle) else {
            delivered.discard(admission);
            return;
        };
        // One call, and it classifies before it parks and parks before it hands anything over. At most one
        // delivery per flow, because a transport is sequential; anything already there would be a second
        // answer for a question that was never asked, released inside and counted here.
        if delivered
            .answering()
            .hand_over(admission, &mut flow.record.serving)
        {
            self.counters.unsettled += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering::Relaxed;
    use std::sync::Arc;
    use std::time::Instant;

    use smoltcp::iface::SocketHandle;
    use tokio::sync::mpsc;
    use vpnhotspotd::shared::admission::{Admission, Class, Request};
    use vpnhotspotd::shared::dns_debt;
    use vpnhotspotd::shared::dns_wire;

    use super::*;
    use crate::owned;
    use crate::resolver;
    // One set of fixtures, shared with the engine's own tests rather than rebuilt here: a session built the
    // way a session is built, an aggregate solved for an exact prepared bound, and the gate that stands in
    // for an upstream a host cannot open.
    use crate::tcp::tests::{
        admission_for, client, question, servfail_for, session, Session, DESTINATION, MTU, RESOLVER,
    };
    use crate::tcp::{flow_footprint, Finished, Gate, FLOW_BUFFER};
    use crate::tcp_flow::{Chunk, READ_CHUNK};
    use crate::tun_writer::Stamp;

    /// A DNS-over-TCP flow, and everything a test needs to stand in for the platform and the client.
    struct Exchange {
        session: Session,
        answers: tokio::sync::mpsc::UnboundedReceiver<resolver::Asked>,
        /// Which of the three syscall-boundary outcomes the next submission takes. The only thing injected,
        /// because it is the only part of that boundary a host cannot reach.
        injected: Arc<resolver::Injected>,
        handle: SocketHandle,
        worker: u64,
    }

    async fn exchange(admission: &mut Admission, gate: &Arc<Gate>) -> Exchange {
        let mut session = session(admission, gate).await;
        let (answers, asked) = tokio::sync::mpsc::unbounded_channel();
        let injected = session.engine.queries.answered_by(answers);
        let exchange = Exchange {
            session,
            answers: asked,
            injected,
            handle: SocketHandle::default(),
            worker: 0,
        };
        exchange.opened(admission, 10_100)
    }

    impl Exchange {
        /// Opens one DNS-over-TCP flow through the engine's own registration and names it.
        fn opened(mut self, admission: &mut Admission, port: u16) -> Self {
            assert!(self.session.engine.open(
                client(port),
                RESOLVER,
                64,
                true,
                Instant::now(),
                admission
            ));
            let handle = *self
                .session
                .engine
                .flows
                .keys()
                .max()
                .expect("at least one flow");
            self.worker = self
                .session
                .engine
                .flows
                .get(&handle)
                .expect("held")
                .record
                .worker;
            self.handle = handle;
            self
        }

        /// What this flow currently has parked for it.
        fn parked(&self) -> Option<dns_debt::DeliveryId> {
            self.session
                .engine
                .flows
                .get(&self.handle)
                .expect("held")
                .record
                .serving
                .parked()
        }

        /// The client's half, as the engine reaches it when the stack has bytes to hand upstream.
        fn downstream(&self) -> mpsc::Sender<Owned> {
            self.session
                .engine
                .flows
                .get(&self.handle)
                .expect("held")
                .record
                .downstream
                .clone()
                .expect("the flow is open")
        }

        /// Puts one length-prefixed question on the client's half, in the quanta the engine copies out of the
        /// stack's receive buffer rather than in one piece - so nothing here allocates a whole message the
        /// daemon's own path never would.
        async fn ask(&mut self, message: &[u8]) {
            let framed = dns_wire::frame(message).expect("a question a prefix can describe");
            assert!(
                framed.len() <= READ_CHUNK,
                "one quantum is what the engine copies at a time - see [Exchange::feed]"
            );
            self.downstream()
                .send(Owned::new(framed))
                .await
                .expect("the transport is reading");
        }

        /// Takes whatever the transport asked its owner for, and gives it to the owner.
        async fn pump(&mut self, admission: &mut Admission) {
            let ask = self.session.asks.recv().await.expect("the transport asked");
            self.session.engine.ask(ask, true, admission);
        }

        /// Both owner steps one question takes: the length the client announced, admitted before anything is
        /// stored, and then the exact query, accepted and published.
        async fn published(&mut self, admission: &mut Admission) {
            self.pump(admission).await;
            self.pump(admission).await;
        }

        /// The same, for a question too large for one read quantum: the client's bytes go on their own task,
        /// because the depth-one channel between them is drained by the transport and the transport is
        /// waiting on the owner this test still has to drive.
        fn feed(&self, message: &[u8]) -> tokio::task::JoinHandle<()> {
            let framed = dns_wire::frame(message).expect("a question a prefix can describe");
            let downstream = self.downstream();
            tokio::spawn(async move {
                for piece in framed.chunks(READ_CHUNK) {
                    if downstream.send(Owned::new(piece.to_vec())).await.is_err() {
                        return;
                    }
                }
            })
        }

        /// Answers the outstanding platform question and takes the settlement its owner produces.
        ///
        /// A value rather than an event, which is what makes the boundary tests below deterministic: the
        /// settlement can be held across a config application and settled afterwards.
        async fn settlement(&mut self, answer: Vec<u8>) -> tcp_dns::Settlement {
            let asked = self.answers.recv().await.expect("a transaction asked");
            asked.answer.send(answer).expect("its transaction waits");
            self.finished().await
        }

        /// The next settlement this engine produces, through its own selection over both owners.
        async fn finished(&mut self) -> tcp_dns::Settlement {
            match self.session.engine.finished().await {
                Finished::Transaction(settlement) => settlement,
                Finished::Flow(_) | Finished::Detached { .. } => {
                    panic!("the flow is not the thing that finished")
                }
            }
        }

        /// Answers the outstanding platform question and settles it at once.
        async fn answer(&mut self, admission: &mut Admission, answer: Vec<u8>) {
            let settlement = self.settlement(answer).await;
            self.session.engine.settle(settlement, admission);
        }

        /// Consumes exactly `expected` framed bytes off the real mailbox, the way the engine's stack does,
        /// answering each piece with the real consumption acknowledgment - and reports the most this thread
        /// owned while it did.
        async fn consume(&mut self, expected: usize) -> (Vec<u8>, owned::Peak) {
            // Both halves the engine drains, because both are bounded: the payload the client's stack would
            // take, and the readiness marker that told it there was any. Leaving the markers fills their
            // channel and stalls the producer - which is the engine's own backpressure, not a quirk here.
            let Session {
                engine, markers, ..
            } = &mut self.session;
            let mut delivered = Vec::new();
            let mut bytes = 0usize;
            let mut peak = owned::Peak::default();
            while bytes < expected {
                let flow = engine.flows.get_mut(&self.handle).expect("held");
                let chunk = flow
                    .record
                    .chunks
                    .recv()
                    .await
                    .expect("the transport is still handing pieces over");
                let Chunk::Payload(piece) = chunk else {
                    panic!("only payload is handed over here")
                };
                bytes += piece.len();
                assert!(piece.len() <= READ_CHUNK, "one read quantum at most");
                delivered.extend_from_slice(&piece);
                // Read while the piece is still held, so anything built beside it counts.
                let (live, _) = owned::peak();
                peak.buffers = peak.buffers.max(live.buffers);
                peak.bytes = peak.bytes.max(live.bytes);
                drop(piece);
                assert_eq!(
                    markers.recv().await,
                    Some(Event {
                        handle: self.handle,
                        worker: self.worker
                    }),
                    "every piece is announced by its own identity"
                );
                engine
                    .flows
                    .get_mut(&self.handle)
                    .expect("held")
                    .record
                    .consumed
                    .send(())
                    .await
                    .expect("the transport waits for this");
            }
            assert_eq!(bytes, expected, "every byte, once");
            (delivered, peak)
        }

        /// The answer the client really received, with its length prefix taken off.
        async fn answered(&mut self, expected: usize) -> Vec<u8> {
            let (delivered, _) = self.consume(expected + dns_wire::PREFIX).await;
            assert_eq!(
                u16::from_be_bytes([delivered[0], delivered[1]]) as usize,
                expected,
                "the prefix describes what follows it"
            );
            delivered[dns_wire::PREFIX..].to_vec()
        }
    }

    /// The whole delivery, through the engine's own wrappers and nothing else.
    ///
    /// Every step below is a call the daemon makes: the transport is the real [tcp_dns::serve] worker, the
    /// question reaches the owner through [Engine::ask], the answer is joined and split by [Engine::settle],
    /// the framing is the real one, the mailbox is the flow's own, and the acknowledgment goes back through
    /// [Engine::ask] again. Only the platform resolver is replaced, by a channel - a host has no platform,
    /// and a fake one would be a second implementation of the thing under test.
    ///
    /// Two failures are closed at once, and both are invisible in an end-state check. The *lost
    /// acknowledgment*: the answer used to travel on a channel the transport already held, so a prompt
    /// transport could frame it, hand every chunk over and report "delivered" before its own worker's
    /// terminal had been read - and parking happens at that terminal, so the report found nothing on the flow
    /// and the grant sat there until the flow closed. The *early release*: the terminal used to give the
    /// whole grant back while the answer, its framed copy and each chunk still existed.
    #[tokio::test]
    async fn the_whole_delivery_runs_through_the_engines_own_wrappers() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        assert_eq!(admission.records_charged(), 1, "the flow's own");
        assert_eq!(admission.dns_tokens_charged(), 1, "one per transport");

        // The client asks. The transport hands its owner the announced length, then the exact query.
        let question = question(0x1234);
        exchange.ask(&question).await;
        exchange.published(&mut admission).await;
        assert_eq!(
            admission.records_charged(),
            2,
            "the flow's and the transaction's"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and still one token, not two"
        );
        let submitted = admission.bytes_charged();
        assert!(submitted > idle, "the exchange is owed from the submission");
        assert_eq!(exchange.parked(), None, "nothing is parked before the join");
        assert_eq!(
            owned::peak().0.buffers,
            1,
            "one buffer: the query, admitted at its announced length before it was stored, and nothing \
             the client's chunks left behind"
        );

        // The largest answer there is, so the reservation is tested against its own worst case.
        let answer = vec![0x5au8; 65_535];
        let framed = answer.len() + dns_wire::PREFIX;
        exchange.answer(&mut admission, answer).await;
        assert_eq!(
            admission.records_charged(),
            1,
            "the descriptor record went at the join"
        );
        let delivery = exchange
            .parked()
            .expect("parked by the settle, before the transport could see the answer");
        let held = admission.bytes_charged();
        assert!(held < submitted, "the query scratch went with the join");
        assert!(held > idle, "and the answer did not");

        // The transport wakes, frames and hands the answer over one piece at a time.
        let (_, peak) = exchange.consume(framed).await;
        assert_eq!(
            admission.bytes_charged(),
            held,
            "owed through the last piece"
        );
        // What really existed at the peak: the answer, the framed copy, and one chunk being handed over.
        // A build that framed every piece up front would show as many as the quantum divides into it.
        assert_eq!(
            peak.buffers, 3,
            "the answer, its framed copy and one piece - no more, and no fewer"
        );
        assert!(
            peak.bytes <= 65_535 + framed + READ_CHUNK,
            "{} bytes at the peak",
            peak.bytes
        );

        // And the transport reports it delivered, naming the delivery it was actually about.
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle, "released, and only now");
        assert_eq!(exchange.parked(), None);
        assert_eq!(owned::peak().0.buffers, 0, "and the transport owns nothing");

        // A duplicate of that report finds nothing, and a report naming a flow this engine no longer holds
        // is not acted on either.
        let stale = exchange.session.engine.counters.stale;
        for flow in [
            Event {
                handle: exchange.handle,
                worker: exchange.worker,
            },
            Event {
                handle: exchange.handle,
                worker: exchange.worker + 1,
            },
        ] {
            exchange.session.engine.ask(
                tcp_dns::Ask::Delivered { flow, delivery },
                true,
                &mut admission,
            );
        }
        assert_eq!(
            exchange.session.engine.counters.stale,
            stale + 2,
            "a duplicate delivery and a stale flow, both no-ops"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "and neither released a byte"
        );

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0, "and not one buffer outlived it");
    }

    /// A late acknowledgment for a finished answer cannot release the successor parked in its place.
    ///
    /// This is what a flow identity alone cannot catch: a transport asks one question after another on one
    /// connection, so both acknowledgments name the same flow, and only the delivery identity tells them
    /// apart. Without it the late one releases the successor's grant while the bytes that grant covers are
    /// still being framed and handed to the client's stack.
    #[tokio::test]
    async fn an_old_acknowledgment_cannot_release_the_delivery_that_replaced_it() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // The first question, answered, delivered and acknowledged.
        exchange.ask(&question(1)).await;
        exchange.published(&mut admission).await;
        exchange.answer(&mut admission, b"one".to_vec()).await;
        let old = exchange.parked().expect("parked");
        exchange.consume(3 + dns_wire::PREFIX).await;
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        // The same flow asks again, and the second answer is parked in its place.
        exchange.ask(&question(2)).await;
        exchange.published(&mut admission).await;
        exchange.answer(&mut admission, b"two".to_vec()).await;
        let new = exchange.parked().expect("parked");
        assert_ne!(old, new, "a table never reissues an identity");
        let owed = admission.bytes_charged();
        assert!(owed > idle, "the successor's bytes are owed");

        // The *first* acknowledgment arrives late - delayed, retried, whatever. It names a delivery that is
        // no longer parked, so it does nothing at all.
        let stale = exchange.session.engine.counters.stale;
        exchange.session.engine.ask(
            tcp_dns::Ask::Delivered {
                flow: Event {
                    handle: exchange.handle,
                    worker: exchange.worker,
                },
                delivery: old,
            },
            true,
            &mut admission,
        );
        assert_eq!(exchange.session.engine.counters.stale, stale + 1);
        assert_eq!(
            admission.bytes_charged(),
            owed,
            "and its bytes are still owed, because they still exist"
        );
        assert_eq!(
            exchange.parked(),
            Some(new),
            "the successor is still parked"
        );

        // The right one releases it, once.
        exchange.consume(3 + dns_wire::PREFIX).await;
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);
        assert_eq!(exchange.parked(), None);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0, "and not one buffer outlived it");
    }

    /// The consumer disappears with an answer already parked on it: the flow's own close is the last owner
    /// that can end that delivery, and it ends it exactly once.
    ///
    /// The window is real. An answer is parked at its transaction's terminal and released when the transport
    /// says the last chunk was acknowledged; between those two points the client can vanish, and nothing
    /// will ever send that acknowledgment.
    #[tokio::test]
    async fn a_flow_that_closes_over_a_parked_answer_ends_it_once() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        exchange.ask(&question(3)).await;
        exchange.published(&mut admission).await;
        exchange.answer(&mut admission, vec![0x5au8; 4_000]).await;
        assert!(
            exchange.parked().is_some(),
            "the answer is parked and nothing has acknowledged it"
        );
        let owed = admission.bytes_charged();
        assert!(owed > idle, "and its bytes are owed");

        // Not one chunk is consumed. The flow is swept holding the delivery.
        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        assert_eq!(exchange.session.engine.flows.len(), 0);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert!(
            admission.bytes_charged() < idle,
            "the delivery went with the flow, and so did the flow's own buffers"
        );
        exchange.session.release(&mut admission);
        assert_eq!(
            admission.bytes_charged(),
            baseline,
            "released once, and only once"
        );
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(
            owned::peak().0.buffers,
            0,
            "and the answer the cancelled transport was holding went with it"
        );
    }

    /// A generation change retires the flows that hold a selected-network socket and leaves the virtual-DNS
    /// transport exactly as it was - same flow, same worker, same socket, same token - without cancelling or
    /// waiting for the question that transport has outstanding.
    ///
    /// This is what the per-query handoff exists for. A transport swept with the generation gets its client a
    /// reset for a connection that was never bound to the network that changed, and the answer it was waiting
    /// for is thrown away; a config acknowledgement that waited for the resolver instead would make every
    /// handover as slow as a remote name server.
    #[tokio::test]
    async fn a_generation_change_keeps_the_dns_transport_and_never_waits_for_its_query() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        // An ordinary flow beside it, which is what a generation change really invalidates.
        assert!(exchange.session.engine.open(
            client(10_300),
            DESTINATION,
            64,
            false,
            Instant::now(),
            &mut admission
        ));
        tokio::task::yield_now().await;
        assert_eq!(
            gate.entered.load(Relaxed),
            1,
            "its worker is on the upstream"
        );

        let question = question(0x1234);
        exchange.ask(&question).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(
            asked.network, 1,
            "published on the selection current when the owner accepted it"
        );
        let (handle, worker) = (exchange.handle, exchange.worker);
        let transaction = exchange
            .session
            .engine
            .flows
            .get(&handle)
            .expect("held")
            .record
            .serving
            .transaction()
            .expect("its question is outstanding");
        let charged = admission.bytes_charged();

        // The generation advances while that question is in flight, and nothing answers it.
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;

        assert_eq!(
            gate.left.load(Relaxed),
            1,
            "the ordinary flow was cancelled and joined before the config could be acknowledged"
        );
        assert_eq!(exchange.session.engine.flows.len(), 1);
        assert!(
            exchange.session.engine.flows.current(&handle, worker),
            "the same transport, not a replacement that took its handle"
        );
        let held = exchange.session.engine.flows.get(&handle).expect("held");
        assert!(
            held.record.downstream.is_some(),
            "its client half is untouched"
        );
        assert!(
            !held.cancel.is_cancelled(),
            "and its worker was never cancelled"
        );
        assert_eq!(
            held.record.serving.transaction(),
            Some(transaction),
            "with its question still outstanding"
        );
        assert_eq!(
            exchange.session.engine.sockets.iter().count(),
            1,
            "its socket is still in the set"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and its logical token never moved"
        );
        assert_eq!(exchange.session.engine.counters.preserved, 1);
        assert_eq!(
            admission.bytes_charged(),
            charged - flow_footprint().expect("bounded"),
            "exactly the retired flow's own buffers, and nothing of the exchange"
        );
        assert!(
            !asked.answer.is_closed(),
            "the platform question was neither cancelled nor awaited"
        );
        assert!(
            exchange.session.engine.queries.debt(transaction).is_some(),
            "and the table still holds its debt"
        );

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// Which selection a query goes out on is decided when the ingress owner accepts *that query* - not when
    /// its flow was opened, and not when its transport framed it.
    ///
    /// Three cases, and the third is the one a handle comparison would get wrong: a config that keeps the
    /// same `Network` and advances the generation still makes an outstanding answer stale, because what an
    /// answer belongs to is the selection it was published under rather than the handle it happens to name.
    #[tokio::test]
    async fn a_query_takes_the_selection_current_when_its_owner_accepted_it() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // Accepted before any config change: the predecessor's selection.
        exchange.ask(&question(1)).await;
        exchange.published(&mut admission).await;
        let first = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(first.network, 1, "the selection current at its acceptance");
        assert_eq!(first.message, question(1), "and the exact query it framed");
        first
            .answer
            .send(b"one".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        exchange.consume(3 + dns_wire::PREFIX).await;
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        // Framed and admitted before the swap, accepted after it. The owner's acceptance is the boundary, so
        // this one belongs to the successor - a reservation is not a publication.
        exchange.ask(&question(2)).await;
        exchange.pump(&mut admission).await;
        assert!(
            exchange
                .session
                .engine
                .flows
                .get(&exchange.handle)
                .expect("held")
                .record
                .serving
                .reserving(),
            "its length is admitted and its bytes are not published yet"
        );
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        exchange.pump(&mut admission).await;
        let second = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(
            second.network, 2,
            "the successor, because that is what was current when the owner accepted it"
        );
        assert_eq!(second.message, question(2));

        // A third config keeps the same `Network` and advances the generation alone. The answer below was
        // published under the previous one, so it is stale by generation even though its handle is still
        // selected.
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 2,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        second
            .answer
            .send(b"two".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        let answer = exchange.answered(question(2).len()).await;
        servfail_for(&answer, &question(2));
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle, "and it was paid for once");

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// An answer resolved on a selection the session has left becomes that query's own SERVFAIL on the
    /// transport that asked - and the obsolete answer is dropped *before* the replacement is built.
    ///
    /// The two orders weigh the same in a balance and differ in what this process holds at once, which is
    /// what the delivery grant is sized against. Afterwards the same connection asks again and is answered
    /// on the successor's network, because nothing about the transport was disturbed.
    #[tokio::test]
    async fn an_old_generation_answer_becomes_that_querys_own_servfail() {
        const RESULT: usize = 4_000;
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        let asking = question(0x0505);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");

        // The handover lands while the answer is in flight. The transport is left alone; the answer is not.
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        asked
            .answer
            .send(vec![0x5au8; RESULT])
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);

        // Read at the settle, before the transport has framed anything, so the last buffer to exist is the
        // replacement.
        let lives = owned::lives();
        let obsolete = lives
            .iter()
            .find(|life| life.bytes == RESULT)
            .expect("the result the platform returned");
        let replacement = lives.last().expect("the SERVFAIL built in its place");
        assert!(
            obsolete.died.expect("the obsolete result is dropped") < replacement.born,
            "the obsolete answer must go before its replacement is built: died {:?} vs born {}",
            obsolete.died,
            replacement.born
        );
        assert_eq!(
            exchange.session.engine.counters.stale, 0,
            "nothing was lost"
        );
        assert!(
            exchange.parked().is_some(),
            "the replacement is parked before the transport can see it"
        );

        // The client is told to try again, on the connection it already has.
        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        exchange.pump(&mut admission).await;
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "charged through the last chunk and its acknowledgment, then released"
        );
        assert_eq!(exchange.parked(), None);

        // And the very next question on that same transport goes out on the successor's network.
        exchange.ask(&question(0x0606)).await;
        exchange.published(&mut admission).await;
        let next = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(next.network, 2, "the selection the transport outlived into");
        next.answer.send(b"two".to_vec()).expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(
            exchange.answered(3).await,
            b"two",
            "a real answer, delivered"
        );
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// An epoch change retires the transport with everything else, and the answer that arrives afterwards is
    /// silent: it cannot reach the flow that took its handle, it refunds exactly once, and the client that
    /// asked is not there to be told anything.
    ///
    /// Silent rather than a SERVFAIL, and that asymmetry is the point: a generation change leaves the client's
    /// transport intact, while an epoch change means the tuple it was keyed by may name a different device.
    #[tokio::test]
    async fn an_epoch_change_retires_the_transport_and_its_late_answer_is_silent() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        exchange.ask(&question(7)).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        let (gone, retired) = (exchange.handle, exchange.worker);

        // The epoch alone, with the generation unchanged: the transport goes anyway, because what it is
        // keyed by may name a different device now.
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 0,
                    epoch: 1,
                },
                Some(1),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        assert_eq!(exchange.session.engine.flows.len(), 0, "the transport went");
        assert_eq!(exchange.session.engine.counters.preserved, 0);
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and its token went to the question still in flight"
        );

        // A successor takes the slot the retired transport left, which is exactly what a handle-only match
        // would confuse the late answer with.
        let mut exchange = exchange.opened(&mut admission, 10_200);
        assert_eq!(
            exchange.handle, gone,
            "the set handed the slot straight back"
        );
        assert_ne!(exchange.worker, retired, "to a different worker");
        let successor = admission.bytes_charged();
        let stale = exchange.session.engine.counters.stale;

        asked
            .answer
            .send(b"late".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(
            exchange.session.engine.counters.stale,
            stale + 1,
            "counted, not delivered"
        );
        assert_eq!(
            exchange.parked(),
            None,
            "and nothing was parked on the flow that reused the handle"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "every byte the retired exchange owned, back exactly once - what is left is the successor"
        );
        assert!(
            successor > idle,
            "which is what it was holding a moment ago"
        );
        assert_eq!(admission.dns_tokens_charged(), 1, "the successor's own");
        assert_eq!(owned::peak().0.buffers, 0, "and its answer went with it");

        // And a swap that moves both axes at once - what a session replacement looks like - retires the
        // successor the same way, question and all.
        exchange.ask(&question(8)).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 2,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        assert_eq!(
            exchange.session.engine.flows.len(),
            0,
            "both axes retire it"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "its token went to the question, once"
        );
        asked
            .answer
            .send(b"late".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(admission.dns_tokens_charged(), 0, "and came back, once");

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A transport that falls idle ends the way a swept one does, and its question is untouched by it.
    ///
    /// Here rather than beside the rest of the idle-lifetime work because what it is really about is the
    /// separation [crate::tcp_dns::Transactions] exists for: expiring a transport is not a reason to cancel,
    /// await or refund a question the platform is still holding. The fixture that can hold a platform
    /// question open is this one.
    #[tokio::test]
    async fn a_transport_that_falls_idle_ends_without_touching_its_question() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let before = Instant::now();
        let mut exchange = exchange(&mut admission, &gate).await;
        let after = Instant::now();
        let idle = admission.bytes_charged();
        let (reused, retired) = (exchange.handle, exchange.worker);

        // A DNS-over-TCP transport is opened on the same transitory floor as any other flow: no client
        // packet has reached its socket, so it is still listening.
        let due = exchange
            .session
            .engine
            .flows
            .get(&reused)
            .expect("held")
            .record
            .deadline
            .expect("an opening floor");
        let floor = std::time::Duration::from_secs(240);
        assert!(due >= before + floor && due <= after + floor);

        exchange.ask(&question(0x1d15)).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(admission.dns_tokens_charged(), 1);

        // A nanosecond short is still alive; the boundary itself is not.
        exchange.session.engine.expire(
            due - std::time::Duration::from_nanos(1),
            &mut exchange.session.output,
        );
        assert_eq!(exchange.session.engine.counters.expired, 0);
        exchange
            .session
            .engine
            .expire(due, &mut exchange.session.output);
        assert_eq!(exchange.session.engine.counters.expired, 1);
        assert_eq!(
            exchange.session.engine.counters.reset, 0,
            "a transport still listening has no client to reset"
        );
        assert!(
            exchange.session.engine.flows.contains(&reused),
            "and nothing is removed before the join"
        );
        assert!(
            !asked.answer.is_closed(),
            "the platform's question is neither cancelled nor awaited by an expiry"
        );

        let Finished::Flow(terminal) = exchange.session.engine.finished().await else {
            panic!("the transport's own task is what finished")
        };
        exchange
            .session
            .engine
            .close(terminal, &mut admission, &mut exchange.session.output);
        assert_eq!(exchange.session.engine.flows.len(), 0);
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "its token went to the question still in flight rather than back into circulation"
        );

        // The set hands the slot straight back, which is exactly what a deadline or an answer matching on the
        // handle alone would confuse the successor with.
        let mut exchange = exchange.opened(&mut admission, 10_210);
        assert_eq!(exchange.handle, reused);
        assert_ne!(exchange.worker, retired);
        let successor_floor = exchange
            .session
            .engine
            .flows
            .get(&reused)
            .expect("held")
            .record
            .deadline
            .expect("its own opening floor");
        assert!(
            successor_floor > due,
            "with a floor of its own, not its predecessor's"
        );
        let successor_charge = admission.bytes_charged();
        assert!(
            successor_charge > idle,
            "which is one transport plus the question its predecessor left outstanding"
        );
        exchange
            .session
            .engine
            .expire(due, &mut exchange.session.output);
        assert_eq!(
            exchange.session.engine.counters.expired, 1,
            "the predecessor's deadline cannot retire the flow that took its slot"
        );

        let stale = exchange.session.engine.counters.stale;
        asked
            .answer
            .send(b"late".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(
            exchange.session.engine.counters.stale,
            stale + 1,
            "and neither can its answer"
        );
        assert_eq!(exchange.parked(), None);
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "every byte the expired transport and its question owned is back, once - what is left is the \
             successor's own"
        );

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// An answer the platform produced *before* a config change is still classified by the stamp its query
    /// was published under, not by whether it happened to arrive in time.
    ///
    /// The two are different questions and only one of them is answerable: what a settle sees is a
    /// settlement it takes when it gets round to it, so "the answer was ready first" says nothing about which
    /// selection it belongs to. The retained stamp does.
    ///
    /// Deterministic, and that is the point of the settlement being a value. The boundary under test is
    /// "terminal reached, config applied, then settled"; the test *takes* the terminal, applies the
    /// successor, and settles the one it is holding. Nothing here yields in the hope that a scheduler
    /// produces the interleaving, which is what a repeated `yield_now` was doing - and what would have gone
    /// on passing if the terminal had never been reached at all.
    #[tokio::test]
    async fn an_answer_ready_before_the_swap_follows_the_stamp_it_was_published_under() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        let asking = question(0x0909);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        // Obtained *before* the successor is applied, so the answer really was produced under the
        // predecessor and is held across the handover by this test rather than by a race.
        let settlement = exchange.settlement(b"early".to_vec()).await;

        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        // ...and settled afterwards, which is exactly the ordering the retained stamp is for.
        exchange.session.engine.settle(settlement, &mut admission);

        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        assert_ne!(
            answer, b"early",
            "the answer it may not send did not go out"
        );
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// A client that half-closes on a clean message boundary is finished asking, and one that half-closes
    /// mid-message has truncated its own request. The first ends the stream in order, after the answers
    /// already in the mailbox; the second is reset.
    #[tokio::test]
    async fn a_half_close_ends_cleanly_on_a_boundary_and_resets_inside_a_message() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // One whole exchange, then the client stops sending.
        exchange.ask(&question(0x0a0a)).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        asked
            .answer
            .send(b"yes".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(exchange.answered(3).await, b"yes");
        exchange.pump(&mut admission).await;

        // The half-close the engine performs when the client's stack stops sending.
        exchange
            .session
            .engine
            .flows
            .get_mut(&exchange.handle)
            .expect("held")
            .record
            .downstream = None;
        // The end of stream is ordered like any other chunk, and waited for: the transport does not return
        // until the client's stack has taken it.
        let flow = exchange
            .session
            .engine
            .flows
            .get_mut(&exchange.handle)
            .expect("held");
        assert!(
            matches!(flow.record.chunks.recv().await, Some(Chunk::Finished)),
            "an ordered end of stream, after the answer"
        );
        assert_eq!(
            exchange.session.markers.recv().await,
            Some(Event {
                handle: exchange.handle,
                worker: exchange.worker
            })
        );
        exchange
            .session
            .engine
            .flows
            .get_mut(&exchange.handle)
            .expect("held")
            .record
            .consumed
            .send(())
            .await
            .expect("the transport waits for this");
        let reset = exchange.session.engine.counters.reset;
        match exchange.session.engine.finished().await {
            Finished::Flow(terminal) => exchange.session.engine.close(
                terminal,
                &mut admission,
                &mut exchange.session.output,
            ),
            Finished::Transaction(_) | Finished::Detached { .. } => {
                panic!("nothing is outstanding")
            }
        }
        assert_eq!(
            exchange.session.engine.counters.reset, reset,
            "a client that finished asking is not reset"
        );

        // A second transport, half-closed inside a message it never completed.
        let mut exchange = exchange.opened(&mut admission, 10_400);
        let framed = dns_wire::frame(&question(0x0b0b)).expect("framed");
        exchange
            .downstream()
            .send(Owned::new(framed[..framed.len() - 4].to_vec()))
            .await
            .expect("the transport is reading");
        exchange.pump(&mut admission).await;
        assert!(
            exchange
                .session
                .engine
                .flows
                .get(&exchange.handle)
                .expect("held")
                .record
                .serving
                .reserving(),
            "its length was admitted and its bytes never arrived"
        );
        exchange
            .session
            .engine
            .flows
            .get_mut(&exchange.handle)
            .expect("held")
            .record
            .downstream = None;
        let reset = exchange.session.engine.counters.reset;
        match exchange.session.engine.finished().await {
            Finished::Flow(terminal) => exchange.session.engine.close(
                terminal,
                &mut admission,
                &mut exchange.session.output,
            ),
            Finished::Transaction(_) | Finished::Detached { .. } => {
                panic!("nothing is outstanding")
            }
        }
        assert_eq!(
            exchange.session.engine.counters.reset,
            reset + 1,
            "a truncated request is reset"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle - flow_footprint().expect("bounded"),
            "an engine with no flows: the query admitted for the message that never came ended with the \
             transport, and so did its descriptor"
        );
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// A transport taken back while a piece of its answer is still in its mailbox drops that piece before
    /// the grant covering it can be released.
    ///
    /// The order this is about is invisible in a balance, which is why it is dated rather than counted.
    /// [crate::mailbox::Mailbox::hand_over] puts a piece into the flow's mailbox and *then* waits to be
    /// acknowledged, so a transport cancelled inside that wait leaves a `Chunk::Payload` there - and that
    /// piece is one of the three buffers the parked delivery's grant covers. [Engine::close] used to release
    /// the DNS state first and drop the mailbox afterwards, which is capacity given back for memory this
    /// process still held.
    ///
    /// `refundable_at` is taken inside [tcp_dns::Serving::close], after the delivery has been released, and
    /// nothing inside that call can touch the mailbox - so "every buffer died before it" says exactly that
    /// the mailbox went first. The reversed order dates the piece's death after the whole call.
    #[tokio::test]
    async fn a_transport_cancelled_holding_a_piece_drops_it_before_the_grant_covering_it() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;

        let asking = question(0x7801);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        exchange.answer(&mut admission, vec![0x33; 200]).await;
        // The transport framed that answer and handed its first piece to the flow's mailbox. Nothing consumes
        // it, so the transport is parked on an acknowledgment that will never come and the piece is still
        // there - which is the state the close below has to unwind in the right order.
        let marker = exchange
            .session
            .markers
            .recv()
            .await
            .expect("a piece was handed over");
        assert_eq!(marker.handle, exchange.handle);
        assert!(
            exchange.parked().is_some(),
            "with the delivery whose grant covers that piece still parked"
        );
        assert!(
            owned::peak().0.buffers > 0,
            "and real buffers alive to be ordered against it"
        );

        let due = exchange
            .session
            .engine
            .flows
            .get(&exchange.handle)
            .expect("held")
            .record
            .deadline
            .expect("an opening floor");
        exchange
            .session
            .engine
            .expire(due, &mut exchange.session.output);
        let Finished::Flow(terminal) = exchange.session.engine.finished().await else {
            panic!("the transport's own task is what finished")
        };
        exchange
            .session
            .engine
            .close(terminal, &mut admission, &mut exchange.session.output);

        let refundable = exchange
            .session
            .engine
            .refundable_at
            .expect("the close dated its own grant");
        for life in owned::lives() {
            let died = life
                .died
                .expect("every buffer this transport owned is gone by now");
            assert!(
                died < refundable,
                "a {} byte buffer born at {} outlived the grant that covered it, released at {refundable}",
                life.bytes,
                life.born
            );
        }
        assert_eq!(owned::peak().0.buffers, 0);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A session that has stopped serving starts no exchange and finishes the one it already accepted.
    ///
    /// Both halves matter and they are different boundaries. A reservation the owner has not accepted is
    /// refused before a byte is allocated, and refused rather than dropped, so the stream stays framed. A
    /// query the owner *had* accepted - whose payload-free commit was still queued when the stop won - is
    /// answered from the capacity that reservation already covers, because submitting it would hand Android
    /// a slot a stopping session cannot watch the end of.
    #[tokio::test]
    async fn stopping_admits_no_new_exchange_and_answers_the_one_already_accepted() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // A length the transport framed and the owner has not admitted yet.
        exchange.ask(&question(0x5701)).await;
        let reserve = exchange
            .session
            .asks
            .recv()
            .await
            .expect("the transport announced a length");
        let denied = exchange.session.engine.counters.denied;
        exchange.session.engine.ask(reserve, false, &mut admission);
        assert_eq!(
            exchange.session.engine.counters.denied,
            denied + 1,
            "refused rather than ignored, so the transport skips the bytes it announced"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "and nothing at all was allocated for it"
        );
        assert_eq!(admission.records_charged(), 1, "the flow's, and no query's");

        // The stream is still framed, which is what "refused" buys over "dropped": the next question is read
        // as though the last had been answered.
        let asking = question(0x5702);
        exchange.ask(&asking).await;
        exchange.pump(&mut admission).await;
        let committing = exchange
            .session
            .asks
            .recv()
            .await
            .expect("the transport filled the buffer it was granted");

        // ...and the stop wins between the owner granting that capacity and the commit reaching it.
        exchange
            .session
            .engine
            .ask(committing, false, &mut admission);
        assert!(
            exchange.answers.try_recv().is_err(),
            "the platform was never asked"
        );
        assert_eq!(exchange.session.engine.counters.answered_here, 1);
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "the transport's own token, and no submitted query's"
        );
        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        exchange.pump(&mut admission).await;

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// A query with nothing to resolve on is answered here, and so is one the descriptor floor has no room
    /// for: both get that query's own SERVFAIL, neither reaches the platform, and the stream is usable
    /// afterwards.
    ///
    /// The stream surviving is half the property. A client told nothing waits out its own timeout, and one
    /// whose connection is reset has to open another before it can ask anything else.
    #[tokio::test]
    async fn a_query_that_cannot_be_submitted_is_answered_here_and_the_stream_carries_on() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // No selected network at all: the config named none, which is a shape a session goes through.
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 0,
                },
                None,
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        let asking = question(0x1111);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        assert!(
            exchange.answers.try_recv().is_err(),
            "the platform was never asked"
        );
        assert_eq!(exchange.session.engine.counters.answered_here, 1);
        assert_eq!(
            admission.records_charged(),
            1,
            "the flow's, and no transaction's"
        );
        assert!(
            exchange.parked().is_some(),
            "with a real delivery parked for it, not state nothing can acknowledge"
        );
        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        exchange.pump(&mut admission).await;
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "released on its acknowledgment"
        );
        assert_eq!(exchange.parked(), None);

        // A network again, and every descriptor this session may hold held by something else: the exchange is
        // denied, the second tier admits an answer instead, and the client is told the same way.
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 2,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        let records = admission
            .reserve(Request::records(
                admission.record_total() - admission.records_charged(),
                Class::Reserved,
            ))
            .expect("every record that is left");
        let asking = question(0x2222);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        assert!(
            exchange.answers.try_recv().is_err(),
            "no descriptor means no transaction, so the platform is not asked"
        );
        assert_eq!(exchange.session.engine.counters.answered_here, 2);
        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        exchange.pump(&mut admission).await;
        admission.release(records);
        assert_eq!(admission.bytes_charged(), idle);

        // The platform failing at a query is the same kind of outcome: this one is answered rather than
        // reported, and the connection is untouched. The answering side going away is what an expected
        // resolver failure looks like from here - a refusal, a timeout, its own per-UID limiter.
        let asking = question(0x2323);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        drop(asked);
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        // And with room again the same connection resolves as usual, which is what "the stream carries on"
        // has to mean.
        exchange.ask(&question(0x3333)).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(asked.network, 2);
        asked
            .answer
            .send(b"yes".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(exchange.answered(3).await, b"yes");
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// A length the aggregate cannot admit allocates nothing at all: not the message, not a fraction of it,
    /// and no platform call - and the transport skips exactly those bytes, so the next question is read as
    /// though the refused one had been answered.
    ///
    /// The largest message a client may announce is 65535 bytes, and this is the whole reason the length is
    /// admitted before it is stored: the ordinary shape - read the message, then ask whether it was allowed -
    /// hands that allocation to whoever asks for it.
    #[tokio::test]
    async fn a_query_the_aggregate_cannot_admit_is_skipped_and_the_stream_carries_on() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // Everything but a sliver of the aggregate belongs to something else.
        let held = admission
            .reserve(Request::bytes(
                admission.byte_total() - admission.bytes_charged() - 4_096,
                Class::Reserved,
            ))
            .expect("the rest of the aggregate");
        let charged = admission.bytes_charged();

        // The largest message a length prefix can describe, dribbled in the quanta the engine reads.
        let oversize = vec![0x5au8; dns_wire::MAX_MESSAGE];
        let feeding = exchange.feed(&oversize);
        exchange.pump(&mut admission).await;
        feeding.await.expect("the client's bytes were all taken");
        assert_eq!(
            exchange.session.engine.counters.denied, 1,
            "refused at its announced length"
        );
        assert_eq!(
            admission.bytes_charged(),
            charged,
            "and charged for nothing"
        );
        assert!(
            exchange.answers.try_recv().is_err(),
            "the platform was never asked"
        );
        assert_eq!(exchange.parked(), None, "and nothing was parked for it");
        let peak = owned::peak().1;
        assert!(
            peak.bytes < 4 * READ_CHUNK,
            "no whole message was ever allocated: {} bytes at the peak",
            peak.bytes
        );

        // Room again, and the very next question on that same stream is admitted and answered: the framing
        // never lost its place.
        admission.release(held);
        exchange.ask(&question(0x4444)).await;
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(
            asked.message,
            question(0x4444),
            "framed exactly, after a skip"
        );
        asked
            .answer
            .send(b"ok!".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(exchange.answered(3).await, b"ok!");
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// Every buffer one DNS-over-TCP transport owns at once belongs to one of its two grants, and both are
    /// observed where the allocations happen rather than argued for.
    ///
    /// Five at the peak, and each is named: the client chunk the transport is still framing out of, the next
    /// one queued behind it in the depth-one channel - both charged to the flow, which is part of why
    /// [crate::tcp]'s per-flow footprint counts three chunks - and the answer, its framed copy and the piece
    /// in flight, which the delivery grant covers. The query's own storage is charged to the exchange and is
    /// gone by then.
    #[tokio::test]
    async fn every_buffer_one_transport_owns_at_once_is_charged_to_one_of_its_two_grants() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // One long question, and the beginning of the next behind it in the same chunk - which is what leaves
        // the transport holding a chunk it has not finished with while it waits for an answer.
        let first = vec![0x11u8; 1_400];
        let second = question(0x7777);
        let mut stream = dns_wire::frame(&first).expect("a question fits its prefix");
        stream.extend_from_slice(&dns_wire::frame(&second).expect("and so does the next"));
        let split = stream.len() - 20;
        let downstream = exchange.downstream();
        downstream
            .send(Owned::new(stream[..split].to_vec()))
            .await
            .expect("the transport is reading");
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(asked.message, first);

        // The transport is parked on the answer, holding the chunk whose tail is the next question; the
        // engine may queue one more behind it, and that pair is what a flow is charged twice a chunk for.
        downstream
            .send(Owned::new(stream[split..].to_vec()))
            .await
            .expect("the depth-one channel takes the next");
        let (live, _) = owned::peak();
        assert_eq!(
            live.buffers, 3,
            "the chunk being framed, the one queued behind it, and the query the resolver holds"
        );
        assert!(
            (split + (stream.len() - split)) as u64
                <= flow_footprint().expect("bounded") - 2 * FLOW_BUFFER as u64,
            "both client chunks have to fit inside what one flow is charged"
        );

        asked
            .answer
            .send(vec![0x5au8; 8_000])
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        let (_, peak) = exchange.consume(8_000 + dns_wire::PREFIX).await;
        assert_eq!(
            peak.buffers, 5,
            "two client chunks and the answer, its framed copy and one piece - no more, and no fewer"
        );
        exchange.pump(&mut admission).await;

        // The second question was behind the first all along, and is served from the two chunks it spans.
        exchange.published(&mut admission).await;
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(
            asked.message, second,
            "reassembled across the chunk boundary, exactly"
        );
        asked
            .answer
            .send(b"two".to_vec())
            .expect("its worker waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(exchange.answered(3).await, b"two");
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// The consumer disappeared while its question was still outstanding: the answer arrives at a flow that
    /// is gone, and the settle is the last owner that can end it.
    ///
    /// The successor is the point. A closed flow's slot is handed straight back to the next one, so a settle
    /// that matched on the handle alone would park a dead transport's answer on a live one - and the client
    /// holding that flow would be sent an answer to a question it never asked.
    #[tokio::test]
    async fn an_answer_for_a_flow_that_is_gone_ends_where_it_is() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        exchange.ask(&question(4)).await;
        exchange.published(&mut admission).await;
        let submitted = admission.bytes_charged();
        assert!(submitted > idle);
        let gone = exchange.handle;

        // Only this flow goes: cancelled, and closed on its own terminal. Its transaction is untouched and
        // still holds the question, the token and every byte of the exchange.
        exchange
            .session
            .engine
            .flows
            .get_mut(&gone)
            .expect("held")
            .cancel
            .cancel();
        exchange
            .session
            .engine
            .flows
            .get_mut(&gone)
            .expect("held")
            .record
            .downstream = None;
        match exchange.session.engine.finished().await {
            Finished::Flow(terminal) => {
                assert_eq!(terminal.key, gone);
                exchange.session.engine.close(
                    terminal,
                    &mut admission,
                    &mut exchange.session.output,
                )
            }
            Finished::Transaction(_) | Finished::Detached { .. } => {
                panic!("the transaction is still outstanding")
            }
        }
        assert_eq!(exchange.session.engine.flows.len(), 0, "the flow is gone");
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and its token went to the question still in flight"
        );
        // The flow's own grant went with it, and the question's did not: what is charged now is an engine
        // with no flow at all, plus the exchange the resolver still holds.
        let orphaned = admission.bytes_charged();
        assert!(orphaned < idle, "the transport's own buffers went");
        assert!(
            orphaned > idle - flow_footprint().expect("bounded"),
            "and the question it left behind is still owed"
        );

        // A successor takes the slot the closed flow left, which is exactly what a handle-only match would
        // confuse it with.
        let mut exchange = exchange.opened(&mut admission, 10_200);
        assert_eq!(
            exchange.handle, gone,
            "the set handed the slot straight back"
        );
        let successor = admission.bytes_charged();

        // Now the answer arrives. There is no flow that asked it, so the settle ends it here.
        let asked = exchange.answers.recv().await.expect("a transaction asked");
        asked
            .answer
            .send(vec![0x5au8; 4_000])
            .expect("its worker is waiting");
        let settlement = exchange.finished().await;
        // The answer is the daemon's from the moment the resolver produced it: held across the terminal, the
        // retirement and the settle below, and counted for every step of it. An accounting that began where
        // a transport receives an answer would see nothing at all here, because no transport ever will.
        //
        // Two, not one: the query travels back beside the answer, because settlement may have to build that
        // query's own SERVFAIL out of it when the generation moved under an outstanding question. Both are
        // inside the same grant this exchange reserved.
        assert_eq!(
            owned::peak().0.buffers,
            2,
            "the answer and the query it was asked about are owned before anything settles them"
        );
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(
            owned::peak().0.buffers,
            0,
            "and the settle dropped it, exactly once"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "every byte the orphaned exchange owned, given back exactly once - and what is left is \
             one open flow, which is what the successor is"
        );
        assert!(orphaned < successor, "the successor took its own grant");
        assert_eq!(
            exchange.parked(),
            None,
            "and nothing was parked on the successor"
        );
        assert_eq!(admission.records_charged(), 1, "only the successor's");

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0, "and not one buffer outlived it");
    }

    /// A question that outlives its transport and then stops being observable is described exactly once.
    ///
    /// The gap this closes was silence in the one direction that matters. A DNS-over-TCP transaction can
    /// outlive the transport that asked for it - the transport closes and hands its logical token to the
    /// question still in flight - and if the platform then keeps a slot this process can no longer watch,
    /// there is no transport left to carry that failure to a terminal of its own. Discarding the settlement
    /// is right on the wire, because there is nobody to answer; being silent about it in the log was not,
    /// because a lost resolver slot is this daemon's own failure rather than a client's.
    ///
    /// Once, not twice: while there *is* a transport its terminal reports and this owner says nothing - see
    /// [Engine::settle] - so the two branches are exclusive by construction rather than by a flag.
    #[tokio::test]
    async fn a_question_that_outlives_its_transport_and_is_lost_is_reported_once() {
        let _reporting = crate::report::exclusive().await;
        let (control, mut published) = mpsc::unbounded_channel();
        let reporter = crate::report::init_owned(control.clone(), |_, _| Vec::new())
            .expect("no other conversation owns reporting");
        let collector = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(message) = published.recv().await {
                let crate::report::ControllerMessage::Nonfatal { report, .. } = message else {
                    continue;
                };
                // Both halves: the context alone also matches the ordinary terminal report a *live*
                // transport's failure produces, and what is being counted here is the one sentence only
                // [crate::resolver::report_unobservable] writes.
                if report.context == "resolver.register"
                    && report.message.contains("can no longer observe")
                {
                    seen.push(report);
                }
            }
            seen
        });

        owned::reset();
        let mut admission = admission_for(4);
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        exchange
            .injected
            .force(resolver::Outcome::LostWhileWatching);
        exchange.ask(&question(0x7601)).await;
        exchange.published(&mut admission).await;
        drop(
            exchange
                .answers
                .recv()
                .await
                .expect("the platform was asked"),
        );

        // A lost transaction is ready the instant it is submitted, so it is what the owner offers first. The
        // settlement is taken as a *value* and held while the transport goes - which is what a settlement is
        // for - so the ordering under test is this test's rather than the scheduler's.
        let settlement = exchange.finished().await;

        // The transport closes with its question still recorded, so its token is moved out of the connection
        // rather than released: the table's own grant takes it, and the transport is gone.
        let gone = exchange.handle;
        {
            let held = exchange.session.engine.flows.get_mut(&gone).expect("held");
            held.cancel.cancel();
            held.record.downstream = None;
        }
        match exchange.session.engine.finished().await {
            Finished::Flow(terminal) => exchange.session.engine.close(
                terminal,
                &mut admission,
                &mut exchange.session.output,
            ),
            Finished::Transaction(_) | Finished::Detached { .. } => {
                panic!("the transport is what finished")
            }
        }
        assert_eq!(
            exchange.session.engine.flows.len(),
            0,
            "no transport is left to be told anything"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and its token is still charged, on the table rather than on the connection"
        );

        // And now the question that outlived it settles, with nobody to tell.
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(
            exchange.session.engine.queries.quarantined(),
            1,
            "the token moved from the debt onto the table, which is where it stays"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "charged for the rest of the session rather than handed back"
        );
        assert_eq!(
            owned::peak().0.buffers,
            0,
            "and no buffer outlived the discard"
        );

        reporter.finish().await.expect("the flush completes");
        drop(control);
        let reports = collector.await.expect("the collector joined");
        let [reported] = &reports[..] else {
            panic!("one report for one lost slot, not {}", reports.len())
        };
        assert!(
            reported.message.contains("can no longer observe"),
            "and it says what was lost: {reported:?}"
        );

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A brand-new DNS-over-TCP transport opens with no selected network at all, answers its first question
    /// itself, and resolves the next one on the successor - on the same exact flow.
    ///
    /// This is the correction. A virtual-DNS transport holds no socket bound to the selection, so requiring
    /// one to *open* it refused a client's SYN for exactly as long as the session had no upstream, which is
    /// the window a client is most likely to be resolving in. The flow that survives here is the same flow,
    /// checked on both halves of its identity, and its second question reaches the platform on the successor's
    /// handle - observed at the submission boundary rather than inferred from a config field.
    #[tokio::test]
    async fn a_dns_transport_opens_with_no_network_and_resolves_on_the_successor() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut session = session(&mut admission, &gate).await;
        // The one thing under test: nothing is selected when this transport opens.
        session
            .engine
            .apply(
                Stamp::default(),
                None,
                MTU,
                &mut admission,
                &mut session.output,
            )
            .await;
        let (answers, asked) = tokio::sync::mpsc::unbounded_channel();
        let injected = session.engine.queries.answered_by(answers);
        let mut exchange = Exchange {
            session,
            answers: asked,
            injected,
            handle: SocketHandle::default(),
            worker: 0,
        }
        .opened(&mut admission, 10_100);
        let idle = admission.bytes_charged();
        assert_eq!(
            exchange.session.engine.counters.no_upstream, 0,
            "a transport that owns no selected-network socket needs no selected network"
        );
        assert_eq!(admission.dns_tokens_charged(), 1, "it holds its own token");
        let (handle, worker) = (exchange.handle, exchange.worker);

        // Its first question: nothing to resolve on, so it is answered here and the stream carries on.
        let first = question(0x7001);
        exchange.ask(&first).await;
        exchange.published(&mut admission).await;
        assert_eq!(exchange.session.engine.counters.answered_here, 1);
        assert!(
            exchange.answers.try_recv().is_err(),
            "the platform was never asked"
        );
        let answer = exchange.answered(first.len()).await;
        servfail_for(&answer, &first);
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle, "and it refunded whole");

        // A successor config supplies one. The transport is untouched - it holds no socket the generation
        // invalidated - and it is the same flow by both halves of its identity.
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        assert_eq!(exchange.handle, handle, "the same slot");
        assert!(
            exchange.session.engine.flows.current(&handle, worker),
            "and the same worker: nothing retired it"
        );

        // The next question on that same stream reaches the platform on the successor's handle, and its real
        // answer is delivered.
        let second = question(0x7002);
        exchange.ask(&second).await;
        exchange.published(&mut admission).await;
        let submitted = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(submitted.message, second, "the exact query, framed once");
        assert_eq!(
            submitted.network, 2,
            "on the network the successor selected"
        );
        submitted
            .answer
            .send(b"real".to_vec())
            .expect("its row waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(exchange.answered(4).await, b"real");
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// A transport swept while its request is still queued at the owner leaves nothing behind, at either of
    /// the two points where something could be left.
    ///
    /// Both are windows the owner's own serialization creates: a `Reserve` sitting in the shared ask channel,
    /// and a filled query sitting in the flow's own depth-one channel with its `Query` behind it. Neither is
    /// reachable from inside the transport - it is parked on a reply - so the flow's close is the only thing
    /// that can end them, and it has to end both.
    #[tokio::test]
    async fn a_sweep_ends_a_queued_reservation_and_a_filled_query_alike() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        // First window: the length prefix is framed and the owner has not looked at the request yet.
        exchange.ask(&question(0x7101)).await;
        let queued = exchange
            .session
            .asks
            .recv()
            .await
            .expect("the transport asked");
        assert_eq!(exchange.parked(), None);
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 0,
                    epoch: 1,
                },
                Some(1),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        assert_eq!(
            admission.bytes_charged(),
            idle - flow_footprint().expect("bounded"),
            "the flow went whole, and nothing was admitted for the question it never asked"
        );
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(owned::peak().0.buffers, 0, "and no buffer outlived it");
        // The stale request, handed to the owner afterwards, does nothing at all.
        let stale = exchange.session.engine.counters.stale;
        exchange.session.engine.ask(queued, true, &mut admission);
        assert_eq!(exchange.session.engine.counters.stale, stale + 1);
        assert_eq!(
            admission.bytes_charged(),
            idle - flow_footprint().expect("bounded")
        );

        // Second window: the buffer is admitted, filled, and handed back on the flow's own channel, with the
        // owner yet to accept it.
        let mut exchange = exchange.opened(&mut admission, 10_200);
        let idle = admission.bytes_charged();
        exchange.ask(&question(0x7102)).await;
        exchange.pump(&mut admission).await;
        let publishing = exchange
            .session
            .asks
            .recv()
            .await
            .expect("the transport published its query");
        assert!(
            exchange
                .session
                .engine
                .flows
                .get(&exchange.handle)
                .expect("held")
                .record
                .serving
                .reserving(),
            "the reservation is the owner's and the query is on its way back"
        );
        assert_eq!(
            owned::peak().0.buffers,
            1,
            "exactly the admitted buffer, filled and in flight"
        );
        let lives = owned::lives();
        let query = lives
            .iter()
            .rposition(|life| life.died.is_none())
            .expect("the live query");

        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 0,
                    epoch: 2,
                },
                Some(1),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        // The query died, and it died *before* the grant covering it became refundable - which is what the
        // close's own type-state enforces and what no balance could show.
        let died = owned::lives()[query]
            .died
            .expect("the admitted query is dropped");
        let refundable = exchange
            .session
            .engine
            .refundable_at
            .expect("the flow closed through the owner's own path");
        assert!(
            died < refundable,
            "the query must die before its grant is refundable: died {died} vs refundable {refundable}"
        );
        assert_eq!(owned::peak().0.buffers, 0);
        assert_eq!(
            admission.bytes_charged(),
            idle - flow_footprint().expect("bounded"),
            "the flow and the reservation both, exactly once"
        );
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        let stale = exchange.session.engine.counters.stale;
        exchange
            .session
            .engine
            .ask(publishing, true, &mut admission);
        assert_eq!(exchange.session.engine.counters.stale, stale + 1);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// The one refusal left after a reservation returns the query to the local-answer path: one same-ID
    /// SERVFAIL, no platform call, an exact refund, and a stream that carries on.
    ///
    /// Unreachable in the daemon, because the room check at reservation is what makes the insertion
    /// infallible. Forced here because "infallible after the grant" has to be a claim about a path that
    /// exists: a refusal that dropped the reservation would strand a descriptor record and an exchange's
    /// worth of bytes on a connection the client is still using.
    #[tokio::test]
    async fn a_table_refusal_after_the_reservation_answers_the_query_here() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        exchange.session.engine.queries.refuse_next_insert();
        let asking = question(0x7201);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        assert_eq!(
            exchange.session.engine.counters.unprepared, 1,
            "the table refused rather than grew"
        );
        assert_eq!(
            exchange.session.engine.counters.answered_here, 1,
            "and the query went to the local-answer path"
        );
        assert!(
            exchange.answers.try_recv().is_err(),
            "the platform was never asked"
        );
        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        exchange.pump(&mut admission).await;
        assert_eq!(
            admission.bytes_charged(),
            idle,
            "refunded exactly once, back to an idle transport"
        );
        assert_eq!(
            admission.records_charged(),
            1,
            "the flow's own, and no more"
        );

        // ...and the very next question on the same stream is published as usual.
        let next = question(0x7202);
        exchange.ask(&next).await;
        exchange.published(&mut admission).await;
        let submitted = exchange.answers.recv().await.expect("a transaction asked");
        assert_eq!(submitted.message, next);
        submitted
            .answer
            .send(b"ok!".to_vec())
            .expect("its row waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(exchange.answered(3).await, b"ok!");
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// An expected platform outcome is one same-ID SERVFAIL through the real shared resolver seam, and the
    /// stream carries on - with nothing left parked once it has been acknowledged.
    ///
    /// Driven at the seam rather than at a helper: the answering side is dropped, which is exactly what a
    /// refusal, a timeout or a full per-UID limiter arrives as. What this pins is the ordering the delivery
    /// grant depends on - one delivery parked, that delivery named in what the transport received, and
    /// nothing parked after the acknowledgment.
    #[tokio::test]
    async fn an_expected_resolver_outcome_is_one_servfail_and_the_stream_carries_on() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        let asking = question(0x7301);
        exchange.ask(&asking).await;
        exchange.published(&mut admission).await;
        // Dropped, which closes the answer channel: the platform's own failure, arriving the way one does.
        drop(exchange.answers.recv().await.expect("a transaction asked"));
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);

        let parked = exchange.parked().expect("one delivery, parked for it");
        let answer = exchange.answered(asking.len()).await;
        servfail_for(&answer, &asking);
        assert_eq!(
            exchange.parked(),
            Some(parked),
            "still parked until the client's stack has taken every byte"
        );
        exchange.pump(&mut admission).await;
        assert_eq!(
            exchange.parked(),
            None,
            "and gone once the transport acknowledged it"
        );
        assert_eq!(admission.bytes_charged(), idle, "refunded whole");
        assert_eq!(admission.records_charged(), 1, "the flow's own");
        assert_eq!(
            exchange.session.engine.counters.stale, 0,
            "nothing was lost"
        );

        // Continuation is the other half: the same connection resolves the next question for real.
        let next = question(0x7302);
        exchange.ask(&next).await;
        exchange.published(&mut admission).await;
        let submitted = exchange.answers.recv().await.expect("a transaction asked");
        submitted
            .answer
            .send(b"real".to_vec())
            .expect("its row waits");
        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(exchange.answered(4).await, b"real");
        exchange.pump(&mut admission).await;
        assert_eq!(admission.bytes_charged(), idle);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// A question the platform took and this process can no longer watch quarantines that transport's logical
    /// token, parks nothing at all, ends the stream, and never lets the token be reused.
    ///
    /// Two corrections meet here. The token is Android's until Android is done with it, so refunding it would
    /// admit a second query against a limiter with no room for one - and the *reason* there is nothing to
    /// deliver is this daemon's own wrapper failing, which is precisely the value that used to be parked with
    /// no identity anyone would ever name. Nothing is parked now: what the transport gets is the refusal that
    /// ends its stream, and the grant behind it was never taken.
    #[tokio::test]
    async fn a_platform_slot_this_process_cannot_watch_is_quarantined_and_ends_its_stream() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        assert_eq!(admission.dns_tokens_charged(), 1);

        exchange.injected.force(resolver::Outcome::Unobservable);
        exchange.ask(&question(0x7401)).await;
        exchange.published(&mut admission).await;
        // The platform really did receive it, which is the whole reason the token may not come back.
        let reached = exchange
            .answers
            .recv()
            .await
            .expect("the platform was asked");
        drop(reached);
        assert_eq!(
            exchange.session.engine.queries.quarantined(),
            1,
            "its token is quarantined rather than refunded"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "moved, never released and re-reserved: the platform's slot is taken throughout"
        );
        assert_eq!(exchange.parked(), None, "and nothing was parked for it");
        assert_eq!(
            admission.records_charged(),
            1,
            "the exchange's descriptor came back; only the flow's own is left"
        );

        // The transport ends its own stream, because it cannot ask again under a token that no longer exists.
        let reset = exchange.session.engine.counters.reset;
        match exchange.session.engine.finished().await {
            Finished::Flow(terminal) => exchange.session.engine.close(
                terminal,
                &mut admission,
                &mut exchange.session.output,
            ),
            Finished::Transaction(_) | Finished::Detached { .. } => {
                panic!("no transaction survived it")
            }
        }
        assert_eq!(
            exchange.session.engine.counters.reset,
            reset + 1,
            "a client that cannot be served again is told so"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle - flow_footprint().expect("bounded"),
            "the flow went whole, and the exchange with it"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and the quarantined token is still gone from circulation"
        );
        assert_eq!(owned::peak().0.buffers, 0);

        // What that costs is exactly one connection: three more transports fit under the cap of four, and
        // the fourth is refused because the token it would need is the one Android is still holding.
        let mut opened = 0;
        for port in 0..4u16 {
            if exchange.session.engine.open(
                client(10_800 + port),
                RESOLVER,
                64,
                true,
                Instant::now(),
                &mut admission,
            ) {
                opened += 1;
            }
        }
        assert_eq!(
            opened, 3,
            "the quarantined token is never handed to another transport"
        );
        assert_eq!(admission.dns_tokens_charged(), 4, "the cap, exactly");

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(
            admission.dns_tokens_charged(),
            0,
            "released once, at the session's end and not before"
        );
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// Losing the registration a live transport's question was being watched with quarantines that
    /// transport's token and ends its stream - the same ownership as a submission that could never be
    /// watched, reached at the other end of the same transaction.
    ///
    /// The token is in a different place here, and that is the whole reason this needs its own test. A
    /// submission that failed at registration is refused before any row exists, so the connection is the only
    /// grant holding a token. This one has a row, settles through it, and the table's own attempt to move the
    /// token finds nothing - because for DNS-over-TCP the debt never took one while its transport was alive.
    /// Exactly one move happens, on the connection, and it happens once.
    #[tokio::test]
    async fn losing_the_watch_on_a_live_transports_question_quarantines_its_token() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();
        assert_eq!(admission.dns_tokens_charged(), 1);

        exchange
            .injected
            .force(resolver::Outcome::LostWhileWatching);
        exchange.ask(&question(0x7501)).await;
        exchange.published(&mut admission).await;
        // Accepted, so a row really exists and the platform really has the question.
        let reached = exchange
            .answers
            .recv()
            .await
            .expect("the platform was asked");
        drop(reached);
        assert_eq!(
            exchange.session.engine.queries.quarantined(),
            0,
            "nothing is at risk yet: this submission was accepted and is being watched"
        );
        assert_eq!(
            admission.records_charged(),
            2,
            "the flow's own and the transaction's"
        );

        let settlement = exchange.finished().await;
        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(
            exchange.session.engine.queries.quarantined(),
            1,
            "moved once, off the connection that was really holding it"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "moved, never released and re-reserved"
        );
        assert_eq!(
            exchange.parked(),
            None,
            "and nothing was parked for a value no acknowledgment could name"
        );
        assert_eq!(
            admission.records_charged(),
            1,
            "the transaction's descriptor came back; only the flow's own is left"
        );

        // The transport ends its own stream, because it cannot ask again under a token that no longer exists -
        // and its close finds no question outstanding, so it releases a grant the token has already left.
        let reset = exchange.session.engine.counters.reset;
        match exchange.session.engine.finished().await {
            Finished::Flow(terminal) => exchange.session.engine.close(
                terminal,
                &mut admission,
                &mut exchange.session.output,
            ),
            Finished::Transaction(_) | Finished::Detached { .. } => {
                panic!("no transaction survived it")
            }
        }
        assert_eq!(
            exchange.session.engine.counters.reset,
            reset + 1,
            "a client that cannot be served again is told so"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle - flow_footprint().expect("bounded"),
            "the flow went whole"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and the quarantined token is still gone from circulation, refunded by nothing"
        );
        assert_eq!(owned::peak().0.buffers, 0);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(
            admission.dns_tokens_charged(),
            0,
            "released once, at the session's end and not before"
        );
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }

    /// An unobservable outcome ends its stream whatever the generation says, and the generation-stale
    /// replacement does not get to overrule it.
    ///
    /// The two rules are individually right and compose wrongly, which is why neither of the tests above
    /// catches this. A stale generation replaces a *predecessor's answer* with that query's own SERVFAIL and
    /// keeps the connection usable, because such a transport still owns its logical token and may ask again.
    /// An unobservable outcome has just had that token quarantined. Run the replacement over it and the client
    /// is told to try again on a connection with nothing left to try with - and the refusal that would have
    /// ended the stream is the very value the replacement overwrites.
    ///
    /// So this drives both at once: a query committed on one generation, its watch lost, a successor
    /// generation applied while the transport is deliberately preserved, and only then the settle.
    #[tokio::test]
    async fn an_unobservable_outcome_ends_its_stream_even_across_a_generation_change() {
        owned::reset();
        let mut admission = admission_for(4);
        let baseline = admission.bytes_charged();
        let gate = Arc::new(Gate::default());
        let mut exchange = exchange(&mut admission, &gate).await;
        let idle = admission.bytes_charged();

        exchange
            .injected
            .force(resolver::Outcome::LostWhileWatching);
        exchange.ask(&question(0x7601)).await;
        exchange.published(&mut admission).await;
        let reached = exchange
            .answers
            .recv()
            .await
            .expect("the platform was asked");
        drop(reached);
        // Taken before the successor is applied, so the terminal really belongs to the predecessor and is held
        // across the handover by this test rather than by a race.
        let settlement = exchange.finished().await;

        // The handover preserves the transport - it holds no selected-network socket - which is exactly the
        // state that makes the composition reachable at all.
        let (handle, worker) = (exchange.handle, exchange.worker);
        exchange
            .session
            .engine
            .apply(
                Stamp {
                    generation: 1,
                    epoch: 0,
                },
                Some(2),
                MTU,
                &mut admission,
                &mut exchange.session.output,
            )
            .await;
        assert!(
            exchange.session.engine.flows.current(&handle, worker),
            "the transport survived the generation change, as a virtual-DNS transport must"
        );

        exchange.session.engine.settle(settlement, &mut admission);
        assert_eq!(
            exchange.session.engine.queries.quarantined(),
            1,
            "exactly one token is quarantined, off the connection that was holding it"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "moved, not refunded and not doubled"
        );
        assert_eq!(
            exchange.parked(),
            None,
            "and no SERVFAIL was parked in place of the refusal that ends this stream"
        );

        // The stream is refused rather than continued: the transport's own terminal arrives and the engine
        // resets it. A generation-stale replacement would have delivered an answer here instead, and this
        // flow would still be open afterwards.
        let reset = exchange.session.engine.counters.reset;
        match exchange.session.engine.finished().await {
            Finished::Flow(terminal) => exchange.session.engine.close(
                terminal,
                &mut admission,
                &mut exchange.session.output,
            ),
            Finished::Transaction(_) | Finished::Detached { .. } => {
                panic!("no transaction survived it")
            }
        }
        assert_eq!(
            exchange.session.engine.counters.reset,
            reset + 1,
            "the client is told, on a connection that can never carry another query"
        );
        assert!(
            !exchange.session.engine.flows.current(&handle, worker),
            "and no second query can be submitted on it, because it is gone"
        );
        assert_eq!(
            admission.bytes_charged(),
            idle - flow_footprint().expect("bounded"),
            "back to the quarantine-only baseline: the flow went whole"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "with only the quarantined token outstanding"
        );
        assert_eq!(owned::peak().0.buffers, 0);

        exchange
            .session
            .engine
            .shutdown(&mut admission, &mut exchange.session.output)
            .await;
        exchange.session.release(&mut admission);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
        assert_eq!(owned::peak().0.buffers, 0);
    }
}
