//! The engine's side of a DNS-over-TCP flow: what a transport may ask its owner for, and what a settled
//! transaction becomes.
//!
//! It owns one cohesive decision made in four places: admit a length, accept the query it was admitted for,
//! classify the answer against the config that query belongs to, and end the delivery when the client's stack
//! has really taken it.
//!
//! Every entry point below validates the flow on *both* halves of its identity first, because smoltcp reuses
//! handles: a request naming only a handle could be admitted against, published for, or released from
//! whatever flow reused it.

use std::io;

use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::dns_debt;

use super::{Engine, Flow};
use crate::report;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_dns::{self, Submitted};
use crate::shizuku::tcp_flow::Event;
use vpnhotspotd::shared::workers::Held;

impl Engine {
    /// Answers one thing a DNS-over-TCP transport asked its owner for.
    ///
    /// Three questions, and each of them is an accounting decision a worker cannot make for itself: may this
    /// announced length be stored, may this exact query be published, and may this delivery end.
    ///
    /// `admitting` is the session's current admission state, and only the first two questions read it. An
    /// admission-closed session may drain what it already owns and may create nothing: a reservation this
    /// owner has not accepted yet allocates nothing, a query it accepted while admission was open is answered
    /// from the capacity that reservation already holds rather than becoming platform work, and a delivery
    /// already parked is acknowledged and released exactly as it would have been.
    ///
    /// `Err` is the one answer that is not this transport's: this daemon's own wrapper around the descriptor
    /// Android returned failed, so the ingress task that owns this engine ends rather than publishing another
    /// query into it.
    pub(crate) fn ask(
        &mut self,
        ask: tcp_dns::Ask,
        admitting: bool,
        admission: &mut Admission,
    ) -> io::Result<()> {
        match ask {
            tcp_dns::Ask::Reserve { flow, length } => {
                self.reserve_query(flow, length, admitting, admission)
            }
            tcp_dns::Ask::Query(flow) => return self.commit_query(flow, admitting, admission),
            // The transport has written the whole answer into its bridge and dropped both of its own
            // buffers, so the delivery grant may end. Validated on both halves first: a report naming a
            // handle whose flow has been replaced would end the successor's delivery instead of its
            // predecessor's.
            tcp_dns::Ask::Delivered { flow, delivery } => {
                // Both identities, and both before anything is released. The flow says the transport is one
                // this owner still holds - handles are reused, so a report from a replaced flow would
                // otherwise reach its successor - and the delivery says *which answer* it is about, because a
                // transport asks one question after another and a late acknowledgment for a finished one
                // would release its successor's grant while those bytes are still being framed.
                let Some(held) = self.serving(flow) else {
                    self.counters.ingress.stale += 1;
                    return Ok(());
                };
                match held.record.serving.acknowledge(admission, delivery) {
                    dns_debt::Acked::Released => {}
                    // A duplicate, or one whose answer the flow's close already ended, or one naming a
                    // delivery that is not the parked one. None of them releases anything.
                    dns_debt::Acked::Mismatched | dns_debt::Acked::Absent => {
                        self.counters.ingress.stale += 1
                    }
                }
            }
        }
        Ok(())
    }

    /// The exact flow an identity names, or nothing when it names one this owner no longer holds.
    fn serving(&mut self, flow: Event) -> Option<&mut Held<Flow>> {
        if !self.flows.current(&flow.handle, flow.incarnation) {
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
            self.counters.ingress.stale += 1;
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
        // exactly once - see [crate::shizuku::tcp_dns::Serving].
        let Some(held) = self.flows.get_mut(&flow.handle) else {
            // Unreachable: the pair was validated above and nothing since has awaited.
            self.counters.ingress.stale += 1;
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
    fn commit_query(
        &mut self,
        flow: Event,
        admitting: bool,
        admission: &mut Admission,
    ) -> io::Result<()> {
        let Some(held) = self.serving(flow) else {
            self.counters.ingress.stale += 1;
            return Ok(());
        };
        let Some((reserved, query)) = held.record.serving.accept() else {
            // Nothing was admitted for this transport, so there is no query to publish and no grant to end.
            self.counters.ingress.stale += 1;
            return Ok(());
        };
        let Some(query) = query else {
            // Unreachable: the transport hands the buffer over before it says so. Ended rather than assumed
            // away, because a reservation nobody consumes is capacity nothing gives back.
            self.counters.ingress.stale += 1;
            reserved.end(admission);
            return Ok(());
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
            self.answer_here(flow, reserved, query, admission);
            return Ok(());
        };
        // `?` is the one outcome that is not this transport's: this daemon's own wrapper around the
        // descriptor Android returned failed, so everything the row held has already gone back - including
        // the token, since the flow's close now finds no question to hand one to - and the failure ends the
        // ingress task. Nothing is recorded on the flow for it, because there is no question to record.
        match self
            .queries
            .submit(network, stamp, flow, reserved, query, admission)?
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
        }
        Ok(())
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
            self.counters.ingress.stale += 1;
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
    /// parked on its flow. [tcp_dns::Answering::hand_over] makes the order structural: the answer is inside
    /// the settled delivery, classification happens before the park, and parking is the only way out of it.
    ///
    /// `Err` is this daemon's own wrapper around the transaction having failed while it was being watched.
    /// The table has already given everything that transaction held back; what is left is that this engine's
    /// owner may not publish another query, so the failure ends the ingress task rather than this one stream.
    pub(crate) fn settle(
        &mut self,
        settlement: tcp_dns::Settlement,
        admission: &mut Admission,
    ) -> io::Result<()> {
        let transaction = settlement.key();
        // Exact identity, both halves, rather than a scan for whichever flow claims this transaction id.
        // smoltcp reuses handles, so a predecessor's answer must never reach the flow that took its place -
        // and the incarnation is what tells those two apart. Read off the settlement rather than off the
        // delivery below, because the delivery does not exist for a settlement that ends the session.
        let asked = settlement.flow();
        let live = self.flows.current(&asked.handle, asked.incarnation);
        // The flow that asked has no question outstanding any more, whatever this settlement turns out to be:
        // its row leaves the table either way. So a close from here on releases its own token rather than
        // trying to hand it to a transaction that has already settled. Only when it is *this* transaction: a
        // flow whose question was replaced still owes the one it has now.
        if live {
            if let Some(flow) = self.flows.get_mut(&asked.handle) {
                if flow.record.serving.transaction() == Some(transaction) {
                    flow.record.serving.asking(None);
                }
            }
        }
        let mut delivered = self.queries.settle(settlement, admission)?;
        let stamp = delivered.stamp();
        // A flow that is absent, closed or reused is one there is nobody left to answer: the transport that
        // asked is gone, and a handle that has been handed to a successor belongs to a different client. Not
        // reported: an answer nobody is waiting for says nothing about this daemon, and what would have been
        // its own failure never reaches here - that ends the session above.
        if !live {
            self.counters.ingress.stale += 1;
            delivered.discard(admission);
            return Ok(());
        }
        // Resolved on a selection this session has stopped claiming. The transport itself survived the
        // handover untouched, so the client is told to try again rather than left waiting - but the answer
        // that came back over the retired network is dropped before that refusal is built. See
        // [tcp_dns::Delivered::stale].
        if stamp.generation != self.stamp.generation {
            report::stdout!(
                "discarding a DNS-over-TCP answer resolved on network {} at generation {}",
                delivered.network(),
                stamp.generation
            );
            if !delivered.stale() {
                // No SERVFAIL could be formed from that query, so there is nothing to send and nothing left
                // to park; this is the last owner that can end these bytes.
                delivered.discard(admission);
                return Ok(());
            }
        }
        if !delivered.has_answer() {
            // Nothing was produced at all, so nobody will ever acknowledge these bytes.
            delivered.discard(admission);
            return Ok(());
        }
        let Some(flow) = self.flows.get_mut(&asked.handle) else {
            delivered.discard(admission);
            return Ok(());
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
        Ok(())
    }
}
