use std::io;

use vpnhotspotd::shared::dns_debt;

use super::{Engine, Flow};
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_dns::{self, Submitted};
use crate::shizuku::tcp_flow::Event;
use vpnhotspotd::shared::workers::Held;

impl Engine {
    /// Answers one thing a DNS-over-TCP transport asked its owner for.
    pub(crate) fn ask(&mut self, ask: tcp_dns::Ask, admitting: bool) -> io::Result<()> {
        match ask {
            tcp_dns::Ask::Reserve { flow, length } => self.reserve_query(flow, length, admitting),
            tcp_dns::Ask::Query(flow) => return self.commit_query(flow, admitting),
            // The transport has written the whole answer into its bridge and dropped both of its own
            // buffers, so its delivery identity may be cleared. Validated on both halves first: a report naming a
            // handle whose flow has been replaced would end the successor's delivery instead of its
            // predecessor's.
            tcp_dns::Ask::Delivered { flow, delivery } => {
                // Both identities, and both before anything is released. The flow says the transport is one
                // this owner still holds - handles are reused, so a report from a replaced flow would
                // otherwise reach its successor - and the delivery says *which answer* it is about, because a
                // transport asks one question after another and a late acknowledgment for a finished one
                // would clear its successor's identity while those bytes are still being framed.
                let Some(held) = self.serving(flow) else {
                    self.counters.ingress.stale += 1;
                    return Ok(());
                };
                match held.record.serving.acknowledge(delivery) {
                    dns_debt::Acked::Released => {}
                    // A duplicate, or one whose answer the flow's close already ended, or one naming a
                    // delivery that is not the parked one. None of them clears current state.
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
    fn reserve_query(&mut self, flow: Event, length: usize, admitting: bool) {
        if self.serving(flow).is_none() {
            self.counters.ingress.stale += 1;
            return;
        }
        // Refuse new queries while admission is disabled so the transport drains the body and returns
        // SERVFAIL.
        if !admitting {
            self.counters.denied += 1;
            self.grant(flow, tcp_dns::Granted::Denied);
            return;
        }
        // DNS-over-TCP is sequential, so a flow can consume only one reservation at a time.
        if self
            .flows
            .get(&flow.handle)
            .is_some_and(|held| held.record.serving.reserving())
        {
            self.counters.denied += 1;
            self.grant(flow, tcp_dns::Granted::Denied);
            return;
        }
        let Some((reserved, query)) = self.queries.reserve(length) else {
            self.counters.denied += 1;
            self.grant(flow, tcp_dns::Granted::Denied);
            return;
        };
        // Held by this owner, on the flow that asked, so a sweep between here and the query arriving ends it
        // exactly once - see [crate::shizuku::tcp_dns::Serving].
        let Some(held) = self.flows.get_mut(&flow.handle) else {
            // Unreachable: the pair was validated above and nothing since has awaited.
            self.counters.ingress.stale += 1;
            drop((reserved, query));
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
    fn commit_query(&mut self, flow: Event, admitting: bool) -> io::Result<()> {
        let Some(held) = self.serving(flow) else {
            self.counters.ingress.stale += 1;
            return Ok(());
        };
        let Some((reserved, query)) = held.record.serving.accept() else {
            // Nothing was admitted for this transport, so there is no query to publish or reservation to end.
            self.counters.ingress.stale += 1;
            return Ok(());
        };
        let Some(query) = query else {
            // Unreachable: the transport hands the buffer over before it says so.
            self.counters.ingress.stale += 1;
            return Ok(());
        };
        if !admitting {
            // Keep the stream alive and answer this reserved query locally.
            return self.answer_here(flow, reserved, query);
        }
        // Only local wrapper failures escape via `?`; platform outcomes, including `EBUSY`, settle as query
        // failures.
        match self.queries.submit(flow, reserved, query)? {
            // Ownership transferred to the table; flow closure will not cancel it.
            Submitted::Outstanding => self.counters.resolved += 1,
            // No platform work started; return the reservation and answer locally.
            Submitted::Refused(reserved, query) => {
                self.counters.unadmitted += 1;
                self.answer_here(flow, reserved, query)?;
            }
        }
        Ok(())
    }

    /// Answers one query this daemon will not submit, and parks its delivery on the flow that asked.
    fn answer_here(
        &mut self,
        flow: Event,
        reserved: tcp_dns::Reserved,
        query: Owned,
    ) -> io::Result<()> {
        self.counters.answered_here += 1;
        let Some(held) = self.flows.get_mut(&flow.handle) else {
            // Unreachable: validated by the caller with nothing awaited since.
            self.counters.ingress.stale += 1;
            drop((reserved, query));
            return Ok(());
        };
        let serving = &mut held.record.serving;
        let Some(answering) = tcp_dns::answered_here(reserved, query, serving) else {
            // The transport was notified and will terminate the invalid message.
            return Ok(());
        };
        if answering.hand_over(serving) {
            self.counters.unsettled += 1;
        }
        Ok(())
    }

    /// Settles one completed transaction; table-invariant failures end the session.
    pub(crate) fn settle(&mut self, settlement: io::Result<tcp_dns::Settlement>) -> io::Result<()> {
        let settlement = settlement?;
        // Exact identity, both halves, rather than a scan for whichever flow claims this transaction id.
        // smoltcp reuses handles, so a predecessor's answer must never reach the flow that took its place -
        // and the incarnation is what tells those two apart. Read off the settlement rather than off the
        // delivery below, because the delivery does not exist for a settlement that ends the session.
        let asked = settlement.flow();
        let live = self.flows.current(&asked.handle, asked.incarnation);
        let delivered = self.queries.settle(settlement)?;
        // A flow that is absent, closed or reused is one there is nobody left to answer: the transport that
        // asked is gone, and a handle that has been handed to a successor belongs to a different client. Not
        // reported: an answer nobody is waiting for says nothing about this daemon, and what would have been
        // its own failure never reaches here - that ends the session above.
        if !live {
            self.counters.ingress.stale += 1;
            delivered.discard();
            return Ok(());
        }
        if !delivered.has_answer() {
            // Nothing was produced at all, so nobody will ever acknowledge these bytes.
            delivered.discard();
            return Ok(());
        }
        let Some(flow) = self.flows.get_mut(&asked.handle) else {
            delivered.discard();
            return Ok(());
        };
        // One call, and it classifies before it parks and parks before it hands anything over. At most one
        // delivery per flow, because a transport is sequential; anything already there would be a second
        // answer for a question that was never asked, replaced inside and counted here.
        if delivered.answering().hand_over(&mut flow.record.serving) {
            self.counters.unsettled += 1;
        }
        Ok(())
    }
}
