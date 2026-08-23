//! The resolver transactions an engine's DNS-over-TCP flows have outstanding, and the slots they hold.
//!
//! Apart from the engine's flow table because a retirement joins that one and may not touch this one, and
//! keyed by its own identity rather than by the flow's handle, which the client-side stack reuses once the
//! flow is gone. That independence is the invariant: a transaction outlives the transport that asked for it,
//! survives a config handover, and ends when the platform is really done - never when a client's connection
//! goes away.
//!
//! # A lifetime, not a task
//!
//! The independence used to be spelled as a spawned task per query, and the spelling cost more than it
//! bought. What the task actually did was await one descriptor and hand the answer back to the owner, which
//! the owner can do itself - so the spawn, its `JoinSet` row, its cancellation token and, as it was written,
//! *three* oneshot channels were plumbing around a wait, and one of those oneshots appeared **before** the
//! query it belonged to had any grant at all. That last part was a real ordering fault and this table fixed
//! it.
//!
//! The rest of it is a simplification rather than an accounting fix, and the distinction matters now that the
//! policy is explicit about it. A task cell, a `JoinSet` row and a cancellation node are **count-bounded
//! rather than byte-charged** - see the Shizuku design's *What Is Byte-Charged And What Is Count-Bounded* -
//! so one of each per admitted query was within the model, not heap the aggregate had missed. What this table
//! buys is a query lifetime that does not depend on a transport's, an ordering that cannot be got wrong, and
//! three fewer moving parts per question.
//!
//! So this is a fixed-capacity table the ingress owner polls. [Transactions::finished] scans the prepared
//! rows, takes exactly one whose platform transaction has reached its terminal, and removes it. Nothing is
//! spawned, nothing is cancelled to reclaim capacity, and dropping a row is what returns this process's
//! descriptor - the same thing cancelling a task did, minus the task. The ownership rule is unchanged and is
//! what the tests check: a retirement does not touch these rows, and a row settles into whatever its own
//! stamp says it is.
//!
//! # The commit order
//!
//! A row is inserted *irrevocably* and then the platform is called, synchronously, on the owner's own task.
//! There is no await, no allocation and no refusal between the two, so there is no window in which
//! `android_res_nsend` has a question this table is not accounting for. Everything that can refuse - room,
//! the identity, both grants, the buffer - happens at [Transactions::reserve], before a byte of the client's
//! message is stored.

use std::collections::HashMap;
use std::future::poll_fn;
use std::io;
use std::task::{Context, Poll};

use vpnhotspotd::shared::admission::{logical_footprint, Admission, Class, Denied, Lease, Request};
use vpnhotspotd::shared::dns_debt::{self, Delivery, Quarantine, QueryDebt};
use vpnhotspotd::shared::dns_wire::resolved;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::model::Network;

use crate::owned::Owned;
use crate::resolver::{Answers, Completed, Resolving, Submission};
use crate::tcp_flow::Event;
use crate::tun_writer::Stamp;

use super::{answered_here_bytes, exchange_bytes, Delivered, Resolved, DELIVERY_BYTES};

/// What one row is waiting on.
enum Awaiting {
    /// Inserted, and not yet handed to the platform. Never observed: [Transactions::submit] replaces this
    /// before it returns, with no await in between.
    Unsubmitted,
    /// A live platform transaction, polled by this owner until it is terminal. Dropping it is what returns
    /// this process's descriptor - and nothing of Android's work, which is why nothing here cancels one to
    /// reclaim capacity.
    Resolver(Resolving),
    /// The submission never reached Android, so this row's terminal is already decided and is yielded at the
    /// next poll. Nothing of the platform's is held.
    Failed(Option<Failure>),
}

/// The failure a row that never reached the platform settles as.
///
/// An ordinary expected outcome, so the client is told to try again rather than left waiting. Allocation-free
/// on purpose: this is a per-query path, and `io::Error::other` on a string literal is a boxed allocation
/// nothing charged for.
fn unreached() -> Failure {
    Failure::platform(io::Error::from(io::ErrorKind::NotConnected))
}

impl Awaiting {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Completed> {
        match self {
            Self::Resolver(resolving) => resolving.poll_result(cx),
            Self::Failed(failure) => {
                let failure = failure.take().unwrap_or_else(unreached);
                Poll::Ready(Completed::Answered(Err(failure)))
            }
            // Unreachable: `submit` replaces it before it returns. Answered rather than left pending, because
            // a row nothing can ever settle is a grant nothing gives back.
            Self::Unsubmitted => Poll::Ready(Completed::Answered(Err(unreached()))),
        }
    }
}

/// One outstanding query: everything it owes, everything it retained, and the platform transaction it is
/// waiting on.
///
/// No logical token of its own, and that is the whole distinction between this and a UDP query: the *flow*
/// holds the token for its transport's whole life, and a transport that closes while its question is still
/// outstanding transfers that same token to this debt rather than charging a second one. What it does *not*
/// transfer is these bytes, because they never belonged to it. See [vpnhotspotd::shared::dns_debt].
struct Pending {
    /// One DNS-class descriptor record and every byte this submission owns: the query, the answer the
    /// platform returns, and the framed copy on its way into the fair mailbox.
    debt: QueryDebt,
    /// The message as it was framed off the client's stream, at exactly the capacity that was admitted for
    /// it. Retained because settlement may have to build this query's own SERVFAIL from it.
    message: Owned,
    /// The config this query was actually accepted and handed to the resolver under.
    ///
    /// Retained rather than read from the engine at settlement, because those are different questions: what
    /// matters is which selection this answer belongs to, not which one is current when it happens to come
    /// back. A handover leaves the client's transport alone, so an answer from the predecessor arrives on a
    /// flow that is still live and has to be told apart from a current one.
    stamp: Stamp,
    /// The exact transport that asked, both halves. Handles are reused, so a handle alone could deliver a
    /// predecessor's answer to whichever flow took its place.
    flow: Event,
    /// The handle it really went out on, so what is reported is the selection that produced this answer
    /// rather than whichever one is current.
    network: Network,
    awaiting: Awaiting,
}

impl Pending {
    /// The session is over. Everything physical goes first - the platform transaction's descriptor, whatever
    /// the resolver left behind, the query - and only then the grant that accounted for them.
    fn drain(self, admission: &mut Admission) {
        let Self {
            debt,
            message,
            awaiting,
            ..
        } = self;
        drop(awaiting);
        drop(message);
        dns_debt::abandon(admission, debt);
    }
}

/// One transaction that has reached its terminal, taken out of the table and not yet settled.
///
/// A value rather than a borrow, and that is what makes the ordering checkable: the owner can hold one across
/// a config application - which is exactly the race the retained stamp exists for - and settle it afterwards,
/// deterministically, rather than hoping a scheduler produced the interleaving under test.
pub(crate) struct Settlement {
    key: u64,
    pending: Pending,
    /// What the platform answered, already classified against the query it was for: an expected outcome is
    /// that query's own SERVFAIL by this point, and only this daemon's own wrapper failing or a query too
    /// malformed to echo is still an error.
    result: Result<Owned, Failure>,
    /// Whether this process lost the ability to watch a transaction the platform had accepted, so the logical
    /// token that named it may never be reused. Carried rather than folded into the failure, because a
    /// `Failure` says which step went wrong and this says who still owns a resolver slot.
    unobservable: bool,
}

impl Settlement {
    /// Which transaction this is, for an owner that keyed something else by it.
    pub(crate) fn key(&self) -> u64 {
        self.key
    }
}

/// What publishing one accepted query came to.
pub(crate) enum Submitted {
    /// The row is in the table and the platform has the question.
    Outstanding(u64),
    /// The platform took the question and its completion can never be observed: this process's own wrapper
    /// around the descriptor failed after `android_res_nsend` had already succeeded. Everything else is given
    /// back, and this transport may not carry on - it cannot ask again under a token that no longer exists.
    ///
    /// The token itself is *not* dealt with here, because for a DNS-over-TCP query it belongs to the
    /// transport rather than to this table: the caller owns that grant and moves the token into the
    /// quarantine. `transaction` is named so the caller can leave the question recorded on its flow if that
    /// move does not happen, which is what makes the flow's own close refuse to hand the token back.
    Unobservable { transaction: u64, failure: Failure },
    /// The table refused the row, so the platform was never asked. The reservation and its query come back
    /// whole for the local-answer path.
    Refused(Reserved, Owned),
}

/// What the ingress owner reserved for a query whose length the client has announced.
///
/// Everything that can refuse happens before one of these exists, which is what makes the insertion that
/// follows it infallible in every ordinary case: room in the table, an identity, the grant, and the buffer.
pub(crate) struct Reserved {
    /// The identity this query is named by: the transaction it becomes if the platform is asked, and the
    /// delivery its answer is acknowledged as either way.
    id: u64,
    debt: QueryDebt,
    /// Whether a DNS-class descriptor was granted with it. Without one the platform cannot be asked, so this
    /// query can only be answered here.
    submittable: bool,
}

impl Reserved {
    /// Whether the platform may be asked about this query at all.
    pub(crate) fn submittable(&self) -> bool {
        self.submittable
    }

    /// Reconciles this reservation down to what physically survives answering the query here, and names the
    /// delivery by the same identity a submitted query's would have used.
    pub(crate) fn settle(self, admission: &mut Admission, delivery_bytes: u64) -> Delivery {
        dns_debt::settle(admission, self.debt, delivery_bytes)
    }

    /// Nobody will submit or answer this: the transport is gone, or nothing could be built for it.
    ///
    /// Whatever buffer it covered must already be dropped - a release while those bytes are alive is capacity
    /// refunded for memory this process is still holding. No descriptor was opened and no platform slot was
    /// taken, so there is nothing else to settle.
    pub(crate) fn end(self, admission: &mut Admission) {
        dns_debt::abandon(admission, self.debt);
    }
}

pub(crate) struct Transactions {
    /// One row per outstanding transaction, prepared for the logical-token cap and never grown.
    rows: HashMap<u64, Pending>,
    /// How many rows this table may hold, which is the cap it enforces rather than the map's own capacity: a
    /// `HashMap` may round its request up, and admitting into that slack would be rows nobody charged for.
    prepared: usize,
    /// The next transaction identity. Checked and never reused, because a terminal, a delivery and an
    /// acknowledgment are all matched against it.
    next: u64,
    /// The table's own retained capacity, charged once for the session - and the grant every quarantined
    /// logical token is moved onto, which is why releasing it is what ends them.
    tables: Lease,
    /// Logical tokens the platform took and this process can no longer watch. A count and nothing else: the
    /// tokens themselves live on [Transactions::tables], because a grant per token would consume ledger rows
    /// the aggregate's derivation never budgeted - see [Quarantine].
    quarantined: Quarantine,
    /// Framed queries nothing could be granted for, which the transport skips.
    skipped: u64,
    /// Submissions the platform accepted and this process cannot observe. Counted, because each is capacity
    /// that is gone for the rest of the session.
    unobservable: u64,
    /// A token that could not be moved into the quarantine, which would be capacity this session goes on
    /// believing it has. Counted rather than assumed away.
    unquarantined: u64,
    /// Where a submitted question's answer comes from.
    answers: Answers,
    /// Set by a test to make the next insertion - and only the next one - refuse. Never true in a build that
    /// is not a test harness.
    #[cfg(test)]
    refuse_next_insert: bool,
    /// Set by a test to make the next quarantine - and only the next one - answer as though the ledger had
    /// lost one of the two rows its move needs. Never true in a build that is not a test harness.
    #[cfg(test)]
    refuse_next_quarantine: bool,
}

impl Transactions {
    /// What a table prepared for `tokens` transactions owns, whatever is in it.
    ///
    /// Charged once by its owner and kept charged until the table is dropped, because it is a charge on the
    /// prepared bound rather than on the rows currently in it. Checked throughout: a figure that would wrap is
    /// a capacity that cannot be accounted for and therefore must not be prepared.
    ///
    /// The quarantine adds nothing to this, and that is deliberate: it allocates nothing, holding its tokens
    /// on the very grant this figure covers.
    pub(crate) fn footprint(tokens: usize) -> Option<u64> {
        logical_footprint::<(u64, Pending)>(tokens)?.checked_add(std::mem::size_of::<Self>() as u64)
    }

    pub(crate) fn new(admission: &mut Admission) -> Result<Self, Denied> {
        // One row per logical token, because a transport cannot open a transaction without holding one.
        let prepared = admission.dns_token_cap() as usize;
        // Charged before either collection exists, which is the ordering this whole path turns on.
        let bytes = Self::footprint(prepared).ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Request::bytes(bytes, Class::Reserved))?;
        Ok(Self {
            // Requested at the token cap, the number [Transactions::footprint] charged for, so the common
            // case allocates nothing. The bound is what [Transactions::reserve] and [Transactions::submit]
            // refuse on; the map's own backing is count-bounded overhead rather than accounted state.
            rows: HashMap::with_capacity(prepared),
            prepared,
            next: 0,
            tables,
            quarantined: Quarantine::default(),
            skipped: 0,
            unobservable: 0,
            unquarantined: 0,
            answers: Answers::Platform,
            #[cfg(test)]
            refuse_next_insert: false,
            #[cfg(test)]
            refuse_next_quarantine: false,
        })
    }

    /// Takes answers from a channel rather than from the platform, which a host has none of.
    ///
    /// The table itself is untouched: what changes is where the one synchronous call inside
    /// [Transactions::submit] goes, and what the returned handle can force it to answer.
    #[cfg(test)]
    pub(crate) fn answered_by(
        &mut self,
        answers: tokio::sync::mpsc::UnboundedSender<crate::resolver::Asked>,
    ) -> std::sync::Arc<crate::resolver::Injected> {
        let injected = std::sync::Arc::new(crate::resolver::Injected::default());
        self.answers = Answers::Channel {
            answers,
            injected: injected.clone(),
        };
        injected
    }

    /// Makes the next insertion refuse, once.
    ///
    /// The refusal that arrives *after* a query has been reserved is the one a single thread cannot otherwise
    /// produce, and it is the one whose unwind matters: everything the reservation took is in hand by then,
    /// and what the client must still get is that query's own SERVFAIL on a stream that carries on.
    #[cfg(test)]
    pub(crate) fn refuse_next_insert(&mut self) {
        self.refuse_next_insert = true;
    }

    /// Makes the next quarantine fail, once.
    ///
    /// [Quarantine::take] moves onto a row that already exists, so it cannot be refused for capacity: the
    /// only ways it fails are the ledger having lost one of the two rows, or the grant not holding the token
    /// it was asked for. The second is guarded against before the move is attempted, and the first is a state
    /// no sequence of this owner's own calls can produce - so this stands in for it, once, leaving every
    /// other half of the composition real.
    #[cfg(test)]
    pub(crate) fn refuse_next_quarantine(&mut self) {
        self.refuse_next_quarantine = true;
    }

    /// The logical maximum: how many transactions may be outstanding at once, which is the whole of what
    /// [Transactions::reserve] gates a new row on.
    #[cfg(test)]
    pub(crate) fn prepared(&self) -> usize {
        self.prepared
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// How many logical tokens this session has had to give up on. Reported through [Transactions::describe]
    /// in the daemon; read directly by a test.
    #[cfg(test)]
    pub(crate) fn quarantined(&self) -> u32 {
        self.quarantined.len()
    }

    /// Releases the table's own capacity, after every row is settled - and with it every logical token this
    /// session had to quarantine, because those were moved onto this very grant.
    ///
    /// The tokens are the one thing here released *because the process is ending* rather than because the work
    /// they stood for finished: Android's slot is its own, and nothing in this process can observe or wait for
    /// it. One release, so there is no second path to get the count wrong on.
    pub(crate) fn release(self, admission: &mut Admission) {
        drop(self.rows);
        admission.release(self.tables);
    }

    /// Admits one framed query before a byte of it is stored, and hands back the buffer it may be stored in.
    ///
    /// Called when the client's length prefix is whole and nothing of the message itself has arrived, which is
    /// the ordering the whole path turns on: the announced length is charged, the buffer is allocated at
    /// exactly that length, and the framing may then fill it and may never grow it. A query copied first and
    /// admitted afterwards is an allocation the aggregate was told about rather than one it agreed to, and
    /// 65535 of them is what a client can announce.
    ///
    /// Two tiers, in this order. A full exchange - one DNS-class descriptor record for the transaction and
    /// every byte it will own - is what a query that can reach the platform needs. When *that* is denied, a
    /// query this daemon answers itself is offered instead: the query, the SERVFAIL built from it and its
    /// framing, with no record and no token, because nothing leaves this process for it. Only a query whose
    /// bytes do not fit either is refused outright, which leaves the transport to skip it and keep the stream
    /// framed for the next question.
    ///
    /// Zero tokens in both tiers. The transport holds one for its whole life, and charging a second per query
    /// would halve the number of connections the nested cap allows - thirty-two token-holding connections
    /// would become sixteen with a query each, which is an artifact of the accounting rather than a limit
    /// anyone chose.
    ///
    /// What a client can hold this way is bounded by the same cap. A connection that sends a length prefix
    /// and then stalls holds one reservation, and it can hold no second one - so the exposure is one
    /// reservation per token-holding transport, which is the ceiling the aggregate is already sized for and
    /// the same shape as a client that completed its query and is waiting for an answer. There is deliberately
    /// no deadline on it: a timer here would be a second retirement policy for state the flow's own close
    /// already ends.
    pub(crate) fn reserve(
        &mut self,
        length: usize,
        admission: &mut Admission,
    ) -> Option<(Reserved, Owned)> {
        // The token cap, which is the logical maximum this table was charged row state for. A settled
        // transaction frees its slot for the next question; the map's own backing is opaque count-bounded
        // overhead and is not consulted. A query skipped here gets its own SERVFAIL and the stream carries on.
        if self.rows.len() >= self.prepared {
            self.skipped += 1;
            return None;
        }
        // Before either grant and before the buffer: an identity that cannot be issued is a query that must
        // not be admitted, and refusing here leaves nothing to unwind. The same identity names the transaction
        // if the platform is asked and the delivery whichever way the query is answered, so an acknowledgment
        // names one delivery for the life of the process.
        let Some(next) = self.next.checked_add(1) else {
            self.skipped += 1;
            return None;
        };
        let id = self.next;
        let submitted =
            exchange_bytes(length).and_then(|bytes| dns_debt::submit(admission, id, bytes).ok());
        let (debt, submittable) = match submitted {
            Some(debt) => (debt, true),
            None => {
                let held = answered_here_bytes(length)
                    .and_then(|bytes| dns_debt::hold(admission, id, bytes).ok());
                match held {
                    Some(debt) => (debt, false),
                    // Not even the answer fits. Nothing is charged, nothing is allocated, and the platform is
                    // never asked - the transport skips these bytes and the client may ask again.
                    None => {
                        self.skipped += 1;
                        return None;
                    }
                }
            }
        };
        // Only now is the identity really spent, so a refused query does not consume one.
        self.next = next;
        // Allocated only now, at exactly the length that was admitted for it.
        let query = Owned::with_capacity(length);
        Some((
            Reserved {
                id,
                debt,
                submittable,
            },
            query,
        ))
    }

    /// Publishes one *accepted* query: takes the row irrevocably, then asks the platform, synchronously.
    ///
    /// `network` and `stamp` are the caller's sample of the config current at this acceptance, and they are
    /// retained rather than looked up again: what an answer belongs to is the selection it went out on, not
    /// whichever one is current when it comes back.
    ///
    /// Nothing awaits and nothing allocates between the insertion and the call, so there is no state in which
    /// `android_res_nsend` holds a question this table is not accounting for. The one refusal left is the
    /// table itself being full, which the room check at [Transactions::reserve] makes unreachable and which
    /// hands the reservation back whole for the local-answer path rather than dropping it.
    pub(crate) fn submit(
        &mut self,
        network: Network,
        stamp: Stamp,
        flow: Event,
        reserved: Reserved,
        query: Owned,
        admission: &mut Admission,
    ) -> Submitted {
        #[cfg(test)]
        let refuse = std::mem::take(&mut self.refuse_next_insert);
        #[cfg(not(test))]
        let refuse = false;
        // Re-checked here because the reservation above and this insertion are separate owner steps and the
        // table may have taken a row in between.
        if refuse || self.rows.len() >= self.prepared {
            return Submitted::Refused(reserved, query);
        }
        let Reserved { id, debt, .. } = reserved;
        self.rows.insert(
            id,
            Pending {
                debt,
                message: query,
                stamp,
                flow,
                network,
                awaiting: Awaiting::Unsubmitted,
            },
        );
        // Committed. Only now does anything leave this process, and it leaves synchronously on the owner - no
        // await and no config application can interleave between the insertion above and the call below, so a
        // query cannot first reach the resolver on a `Network` a successor config has already replaced.
        let submission = match self.rows.get(&id) {
            Some(pending) => self.answers.submit(network, &pending.message),
            // Unreachable: it was inserted immediately above and nothing since has removed it.
            None => Submission::NeverReached(unreached()),
        };
        let failure = match submission {
            Submission::Accepted(resolving) => {
                self.set(id, Awaiting::Resolver(resolving));
                return Submitted::Outstanding(id);
            }
            // Nothing of the platform's is held, so this is one query's own expected failure: the row settles
            // at the next poll and the client gets its SERVFAIL on a stream that carries on.
            Submission::NeverReached(failure) => {
                self.set(id, Awaiting::Failed(Some(failure)));
                return Submitted::Outstanding(id);
            }
            Submission::Unobservable(failure) => failure,
        };
        self.unobservable += 1;
        // Not reported here. The failure travels out with [Submitted::Unobservable] and ends the transport
        // that asked, and a transport ending on a local failure is reported once by [crate::tcp] where every
        // other worker's ending is - which is also where a question that stopped being observable *later*
        // arrives. Reporting here as well made one failure two reports from two sites.
        // Everything physical goes back - the descriptor was returned by the dropped submission, and the
        // query and the answer allowance are this process's - while the logical token does not: the caller
        // moves that into the quarantine, because for a DNS-over-TCP query the token belongs to the transport
        // rather than to this debt. See [Transactions::quarantine].
        if let Some(pending) = self.rows.remove(&id) {
            pending.drain(admission);
        }
        Submitted::Unobservable {
            transaction: id,
            failure,
        }
    }

    /// Stores what a freshly inserted row is waiting on. Separate only because the borrow that read the
    /// query for the platform call has to end first.
    fn set(&mut self, id: u64, awaiting: Awaiting) {
        if let Some(pending) = self.rows.get_mut(&id) {
            pending.awaiting = awaiting;
        }
    }

    /// Moves one logical token out of the grant that was holding it and onto this table's own, for the
    /// session.
    ///
    /// Called by the owner of that grant - for a DNS-over-TCP query the flow's own connection, since the
    /// token is per transport - once the platform has taken a question this process can no longer watch.
    /// `false` is a token that could not be moved, which is counted rather than believed: it would be
    /// capacity this session goes on thinking it has, and its caller is expected to keep the token where it
    /// is rather than release it.
    ///
    /// Cannot be refused for capacity: the move is onto a ledger row that already exists, which is the whole
    /// reason the tokens live here rather than in grants of their own.
    pub(crate) fn quarantine(&mut self, admission: &mut Admission, from: &Lease) -> bool {
        if !Quarantine::holds_a_token(admission, from) {
            // Nothing at risk on this grant, so there is nothing to move and nothing to count. Reached when
            // a closing transport has already handed the token to the question that is settling now.
            return true;
        }
        // The one state this cannot reach on its own: the ledger having lost one of the two rows the move
        // needs. Consumed once, and deliberately *after* the token check above, so what a test composes is a
        // real token on a real debt with a move that cannot be represented - not a fabricated refusal.
        #[cfg(test)]
        if std::mem::take(&mut self.refuse_next_quarantine) {
            self.unquarantined += 1;
            return false;
        }
        if self
            .quarantined
            .take(admission, from, &self.tables)
            .is_err()
        {
            self.unquarantined += 1;
            return false;
        }
        true
    }

    /// Takes over a closed transport's grant whose token could not reach the question still outstanding.
    ///
    /// The one place the two ways a token goes at risk meet: a submission the platform accepted and could not
    /// be watched, and a close that could not hand its token over. Both end with the token on this table's own
    /// grant, and both leave the rest of what the closed transport owned released as usual. If even that move
    /// cannot be represented the closed grant is kept whole, which shows up as an outstanding lease in the
    /// session's exit report rather than as capacity handed back for a slot Android may still hold.
    pub(crate) fn strand(&mut self, admission: &mut Admission, stranded: dns_debt::Stranded) {
        if self.quarantine(admission, stranded.lease()) {
            stranded.released(admission);
        } else {
            stranded.kept();
        }
    }

    /// The debt a closing transport must hand its token to, if its question is still outstanding.
    ///
    /// Answered rather than acted on, because the move itself belongs to [dns_debt::close] - one function
    /// that knows the connection's grant, the debt's grant and the rule connecting them, rather than two
    /// halves that can disagree about which of them released the token.
    pub(crate) fn debt(&self, key: u64) -> Option<&QueryDebt> {
        self.rows.get(&key).map(|pending| &pending.debt)
    }

    /// The next transaction to have reached its terminal.
    ///
    /// Selected on by the ingress task, so it waits forever while nothing is outstanding rather than
    /// answering at once - and cancellation-safe, which is what lets it sit in a `select!` beside the flow
    /// table. A poll either takes exactly one ready row out and yields it in the same poll, or it changes
    /// nothing at all: there is no state in which a row has been removed and its result has not been handed
    /// to the caller, so abandoning this future loses neither.
    pub(crate) async fn finished(&mut self) -> Settlement {
        poll_fn(|cx| self.poll_finished(cx)).await
    }

    fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Settlement> {
        // Every row is polled, so every one of them registers this task's waker; the first that is ready is
        // the one taken. A row passed over is a row another was ready before, and the ready one leaves the
        // table - so nothing here can be starved by a peer that is always ready first.
        let mut ready = None;
        for (key, pending) in self.rows.iter_mut() {
            if let Poll::Ready(completed) = pending.awaiting.poll(cx) {
                let unobservable = completed.unobservable();
                // Classified here, where the query it was for is still owned, so what leaves this table is
                // either something to put on the stream or something that ends it - never a platform outcome
                // the transport would have to guess about. Owned from here: this is the buffer the daemon
                // carries through the settle, the park and the handoff.
                let result = resolved(completed.answer(), &pending.message).map(Owned::new);
                ready = Some((*key, result, unobservable));
                break;
            }
        }
        let Some((key, result, unobservable)) = ready else {
            return Poll::Pending;
        };
        let Some(pending) = self.rows.remove(&key) else {
            // Unreachable: the key came from this very scan.
            return Poll::Pending;
        };
        Poll::Ready(Settlement {
            key,
            pending,
            result,
            unobservable,
        })
    }

    /// Settles one finished transaction, and hands back what the *delivery* after it still owns.
    ///
    /// The descriptor record and any logical token end here, because the platform's transaction is over. The
    /// answer does not: the transport has yet to receive it, classify it, frame it and hand each chunk to the
    /// client's stack, and every one of those buffers exists after this returns.
    ///
    /// Unless the transaction was one this process stopped being able to watch, in which case the token is not
    /// over at all. It sits in exactly one of two places by then - on the debt, if a closing transport already
    /// handed it there, or still on a live transport's own connection - so this moves whichever one it holds
    /// and tells its caller, through [Delivered::unobservable], to deal with the other.
    ///
    /// `None` is that move failing, and it is the one outcome with nothing to deliver. A token on the debt
    /// means the transport that asked has already closed and handed it here, so there is nobody an answer
    /// could go to; and settling would release the very grant the token is sitting on, handing back capacity
    /// for a resolver slot the platform still holds. So the physical owners die here in the order every other
    /// terminal uses and the grant is kept - see [QueryDebt::kept] - which shows up as an outstanding lease in
    /// the exit report and as `unquarantined` in this table's own. Deliberately not a delivery built from a
    /// second grant: that would be a ledger row this table has no reason to own, for an answer nobody is
    /// waiting for.
    pub(crate) fn settle(
        &mut self,
        settlement: Settlement,
        admission: &mut Admission,
    ) -> Option<Delivered> {
        let Settlement {
            key,
            pending,
            result,
            unobservable,
        } = settlement;
        let Pending {
            debt,
            message,
            stamp,
            flow,
            network,
            awaiting,
        } = pending;
        // The platform transaction is over by construction - it produced this result - so what is left of it
        // is dropped here rather than carried into a delivery that has nothing to do with it.
        drop(awaiting);
        if unobservable {
            self.unobservable += 1;
            // Before the settle below, which is what releases a token a closed transport had handed here.
            if !self.quarantine(admission, debt.lease()) {
                // Said before it is destroyed, because nothing downstream can say it: the transport that put
                // this token here has closed, and returning `None` means no delivery reaches an engine and no
                // terminal reaches a transport. This owner is the last one that sees the failure at all.
                if let Err(failure) = &result {
                    crate::resolver::report_unobservable(key, failure);
                }
                // The answer, then the query - the resolver's own half went above - and only then is the
                // grant kept rather than released. Every byte this transaction held is gone; what stays
                // charged is one row carrying a token that must never be reused.
                drop(result);
                drop(message);
                debt.kept();
                return None;
            }
        }
        Some(Delivered::new(
            dns_debt::Settled::delivering(
                dns_debt::settle(admission, debt, DELIVERY_BYTES),
                Resolved::new(result, Some(message)),
            ),
            stamp,
            flow,
            network,
            unobservable,
        ))
    }

    /// The session is over: every row goes, in physical order.
    ///
    /// Not a cancellation that reclaims capacity - the process is about to exit. Dropping a row returns this
    /// process's descriptor, which is as far as a process can get: the platform's slot is released when its
    /// own work finishes, and nothing here can observe or wait for that.
    pub(crate) fn shutdown(&mut self, admission: &mut Admission) {
        for (_, pending) in self.rows.drain() {
            pending.drain(admission);
        }
    }

    /// Nothing about a duplicate settlement appears here, and that is deliberate: [Transactions::finished]
    /// removes the row it yields, and a [Settlement] is a value that carries it - so a second settlement for
    /// the same transaction is unrepresentable rather than counted.
    pub(crate) fn describe(&self) -> String {
        format!(
            "{} outstanding transactions, skipped {}, unobservable {}, quarantined {}, \
             unquarantined {}",
            self.rows.len(),
            self.skipped,
            self.unobservable,
            self.quarantined.len(),
            self.unquarantined
        )
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;

    use tokio::sync::mpsc;
    use vpnhotspotd::shared::admission::Totals;
    use vpnhotspotd::shared::dns_wire::MAX_MESSAGE;

    use super::super::{Answered, Control, Serving};
    use super::*;
    use crate::resolver::Outcome;

    fn admission() -> Admission {
        Admission::new(Totals {
            admission_id: 1,
            record_total: 64,
            dns_record_floor: 8,
            byte_total: 8 << 20,
            reserved_byte_floor: 1 << 20,
            fragment_cap: 1 << 20,
            dns_token_cap: 4,
            byte_only_owners: 4,
        })
        .expect("the fixture totals hold their own accounting")
    }

    /// One ordinary query for `example.com`, which is what a client's stream carries - and what a SERVFAIL
    /// has to echo to be an answer at all.
    fn query(id: u16) -> Vec<u8> {
        let mut query = vec![
            0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
            b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
        ];
        query[..2].copy_from_slice(&id.to_be_bytes());
        query
    }

    fn flow(worker: u64) -> Event {
        Event {
            handle: smoltcp::iface::SocketHandle::default(),
            worker,
        }
    }

    /// One flow's owner-side control pair, built the way [crate::flow_setup::prepare] builds it - at the
    /// production types and the production depth - with the transport's ends handed back so a test can read
    /// what the owner really sent.
    fn serving() -> (Serving, mpsc::Receiver<Control>, mpsc::Sender<Owned>) {
        let (answers, control) = mpsc::channel(1);
        let (filled, accepted) = mpsc::channel(1);
        (Serving::new(answers, accepted), control, filled)
    }

    /// Everything an ingress owner does between a framed length and a published query, through the real
    /// table: admit the announced length, fill exactly that buffer, and publish it under the config sampled
    /// at that moment.
    fn published(
        queries: &mut Transactions,
        admission: &mut Admission,
        network: Network,
        stamp: Stamp,
        worker: u64,
        message: &[u8],
    ) -> Submitted {
        let (reserved, mut query) = queries
            .reserve(message.len(), admission)
            .expect("the fixture has room for one exchange");
        assert_eq!(
            query.capacity(),
            message.len(),
            "the buffer is exactly the announced length"
        );
        assert_eq!(query.extend_within_capacity(message), message.len());
        queries.submit(network, stamp, flow(worker), reserved, query, admission)
    }

    fn outstanding(submitted: Submitted) -> u64 {
        match submitted {
            Submitted::Outstanding(key) => key,
            Submitted::Unobservable { .. } => panic!("the platform took it and could be watched"),
            Submitted::Refused(_, _) => panic!("the table had room"),
        }
    }

    /// How many tasks this runtime is keeping alive, which is what a transaction used to add one of.
    fn alive() -> usize {
        tokio::runtime::Handle::current()
            .metrics()
            .num_alive_tasks()
    }

    /// Two outstanding transactions cost this runtime no task at all, and the table's charge is the one it was
    /// given before either collection existed.
    ///
    /// Both halves are worth pinning, for different reasons. A transaction used to be a spawned task - a
    /// boxed future, a `JoinSet` row and a cancellation token - and those cells are **count-bounded rather
    /// than byte-charged** under the approved policy, so having none of them per query is a simplification
    /// rather than bytes recovered: fewer moving parts, and a query lifetime that does not depend on a
    /// transport's. What *is* an accounting property is the second half: the charge is taken before the table
    /// exists and covers its whole logical bound, so admitting rows inside that bound adds nothing to it.
    #[tokio::test]
    async fn outstanding_transactions_cost_no_task_and_no_growth() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        assert_eq!(
            admission.bytes_charged() - baseline,
            Transactions::footprint(admission.dns_token_cap() as usize).expect("bounded"),
            "the charge is the footprint, taken before either collection existed"
        );
        let empty = admission.bytes_charged();
        let tasks = alive();
        let (answers, asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);

        let first = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &query(0x1111),
        ));
        let second = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            2,
            &query(0x2222),
        ));
        assert_ne!(first, second, "each transaction is named once");
        assert_eq!(queries.len(), 2);
        assert_eq!(
            alive(),
            tasks,
            "two outstanding transactions and not one task between them"
        );
        assert_eq!(admission.records_charged(), 2, "one descriptor each");
        assert_eq!(
            admission.dns_tokens_charged(),
            0,
            "and no token: those belong to the transports, which this table has none of"
        );

        // Both are really outstanding at the platform, which is what makes the counts above about live work.
        assert_eq!(asked.len(), 2);
        queries.shutdown(&mut admission);
        drop(asked);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(crate::owned::peak().0.buffers, 0);
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A settled transaction gives its slot back and the next question takes it.
    ///
    /// The ordinary shape: a question settles, its row goes, another question arrives. The token cap is the
    /// only admission condition, so the slot a settlement frees is one the next question gets - the map's own
    /// backing is opaque count-bounded overhead and is not consulted.
    #[tokio::test]
    async fn a_settled_transaction_frees_its_slot_for_the_next_question() {
        crate::owned::reset();
        let mut admission = admission();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let prepared = queries.prepared();
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);

        // The whole token cap.
        let mut worker = 0u64;
        let mut live = Vec::new();
        while queries.len() < prepared {
            worker += 1;
            outstanding(published(
                &mut queries,
                &mut admission,
                7,
                Stamp::default(),
                worker,
                &query(0x1111),
            ));
            live.push(asked.recv().await.expect("the platform was asked"));
        }
        assert!(prepared > 0, "the fixture admits something");
        assert_eq!(queries.len(), prepared);

        // Settling one frees its slot, and the next question takes it - twice over, because a stream does
        // this for as long as it lasts.
        for round in 0..2 * prepared {
            let oldest = live.remove(0);
            oldest
                .answer
                .send(b"answer".to_vec())
                .expect("its row waits");
            let settlement = queries.finished().await;
            queries
                .settle(settlement, &mut admission)
                .expect("an observable transaction settles")
                .discard(&mut admission);
            assert_eq!(
                queries.len(),
                prepared - 1,
                "round {round}: the slot came back"
            );
            let skipped = queries.skipped;
            let next = query(0x2222);
            worker += 1;
            let (reserved, mut buffer) = queries
                .reserve(next.len(), &mut admission)
                .unwrap_or_else(|| panic!("round {round}: a settled slot is available again"));
            assert_eq!(buffer.extend_within_capacity(&next), next.len());
            outstanding(queries.submit(
                7,
                Stamp::default(),
                flow(worker),
                reserved,
                buffer,
                &mut admission,
            ));
            live.push(asked.recv().await.expect("the platform was asked"));
            assert_eq!(
                queries.len(),
                prepared,
                "round {round}: and the next question took it"
            );
            assert_eq!(
                queries.skipped, skipped,
                "round {round}: nothing was skipped"
            );
        }

        queries.shutdown(&mut admission);
        drop(asked);
        queries.release(&mut admission);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The second question answered is the first one settled, and it settles as itself.
    ///
    /// Out-of-order completion is the ordinary case - the platform answers whichever name resolves first -
    /// and what it has to produce is the *asking* transport's answer, not the oldest one's.
    #[tokio::test]
    async fn a_later_question_answered_first_settles_as_itself() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let empty = admission.bytes_charged();
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);

        let first = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &query(0x1111),
        ));
        let second = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            2,
            &query(0x2222),
        ));
        let one = asked.recv().await.expect("the first asked");
        let two = asked.recv().await.expect("the second asked");
        assert_eq!(&one.message[..2], &0x1111u16.to_be_bytes());
        assert_eq!(&two.message[..2], &0x2222u16.to_be_bytes());

        // Only the second is answered, so it is the only one that can settle.
        two.answer.send(b"two".to_vec()).expect("its row waits");
        let settlement = queries.finished().await;
        assert_eq!(settlement.key(), second, "the one that finished");
        let delivered = queries
            .settle(settlement, &mut admission)
            .expect("an observable transaction settles");
        assert_eq!(delivered.flow().worker, 2, "on the transport that asked it");
        assert!(delivered.has_answer());
        assert_eq!(queries.len(), 1, "and the first is still outstanding");

        let (mut serving, mut control, _filled) = serving();
        assert!(!delivered
            .answering()
            .hand_over(&mut admission, &mut serving));
        let Some(Control::Answered(Answered::Delivered { delivery, result })) =
            control.recv().await
        else {
            panic!("the transport is handed the answer it can acknowledge")
        };
        assert_eq!(&result[..], b"two");
        assert_eq!(serving.parked(), Some(delivery));
        drop(result);
        assert_eq!(
            serving.acknowledge(&mut admission, delivery),
            dns_debt::Acked::Released
        );

        // The first is still exactly where it was, and settles as itself afterwards.
        one.answer.send(b"one".to_vec()).expect("its row waits");
        let settlement = queries.finished().await;
        assert_eq!(settlement.key(), first);
        let delivered = queries
            .settle(settlement, &mut admission)
            .expect("an observable transaction settles");
        assert_eq!(delivered.flow().worker, 1);
        delivered.discard(&mut admission);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(crate::owned::peak().0.buffers, 0);
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// Abandoning a pending scan loses neither row nor result: each transaction is still received exactly
    /// once afterwards.
    ///
    /// This is the property that lets the scan sit in the ingress task's `select!` beside the flow table. A
    /// poll that had removed a row before deciding what to do with it - or that had taken a value out of a
    /// channel and dropped it with the future - would lose exactly one answer per config change, which is a
    /// client left waiting and a grant nothing releases.
    #[tokio::test]
    async fn a_cancelled_scan_loses_no_transaction() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);
        let first = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &query(0x1111),
        ));
        let second = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            2,
            &query(0x2222),
        ));

        // Polled once with nothing ready, and then abandoned - which is exactly what a `select!` arm losing
        // its race does.
        {
            let mut scanning = pin!(queries.finished());
            let pending = std::future::poll_fn(|cx| Poll::Ready(scanning.as_mut().poll(cx))).await;
            assert!(pending.is_pending(), "nothing has answered yet");
        }
        assert_eq!(queries.len(), 2, "and the abandoned scan took no row");

        let one = asked.recv().await.expect("the first asked");
        let two = asked.recv().await.expect("the second asked");
        one.answer.send(b"one".to_vec()).expect("its row waits");
        two.answer.send(b"two".to_vec()).expect("its row waits");

        // Each, exactly once, in whichever order the scan reaches them.
        let mut settled = Vec::new();
        for _ in 0..2 {
            let settlement = queries.finished().await;
            settled.push(settlement.key());
            queries
                .settle(settlement, &mut admission)
                .expect("an observable transaction settles")
                .discard(&mut admission);
        }
        settled.sort_unstable();
        let mut expected = [first, second];
        expected.sort_unstable();
        assert_eq!(settled, expected, "every transaction, once");
        assert_eq!(queries.len(), 0);
        assert_eq!(crate::owned::peak().0.buffers, 0);
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A session that ends over an unanswered question drains it whole: every buffer dies, the answering
    /// side sees a closed transaction, and the aggregate comes back to exactly what the table alone owed.
    #[tokio::test]
    async fn shutdown_drains_an_unfinished_transaction_whole() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let empty = admission.bytes_charged();
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);
        outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &query(0x1234),
        ));
        let held = asked.recv().await.expect("the transaction asked");
        assert_eq!(crate::owned::peak().0.buffers, 1, "the query it retained");

        queries.shutdown(&mut admission);
        assert_eq!(queries.len(), 0);
        assert_eq!(
            crate::owned::peak().0.buffers,
            0,
            "the query died with the row, before its grant was given back"
        );
        assert_eq!(
            admission.bytes_charged(),
            empty,
            "back to exactly what the table itself owes"
        );
        assert_eq!(admission.records_charged(), 0);
        // The resolver side is closed by the drained row, which is what a dropped transaction does.
        assert!(
            held.answer.is_closed(),
            "the row that was awaiting this answer is gone"
        );
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A query is charged at exactly the length its client announced, before one byte of it is stored - and a
    /// length nothing can be granted for allocates nothing at all and never reaches the platform.
    ///
    /// The failure this closes is the ordinary one: read the message, then ask whether it was allowed. A
    /// client announcing 65535 gets that allocation either way, and the aggregate finds out about it
    /// afterwards. What makes the order checkable rather than merely stated is the refusal - a buffer that
    /// exists after a denial is one nothing admitted.
    #[tokio::test]
    async fn a_query_is_charged_at_its_announced_length_before_it_is_stored() {
        crate::owned::reset();
        let mut admission = admission();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);
        let empty = admission.bytes_charged();

        let announced = query(7).len();
        let (reserved, query) = queries
            .reserve(announced, &mut admission)
            .expect("a short query fits");
        assert_eq!(query.capacity(), announced, "exactly what was announced");
        assert_eq!(
            admission.bytes_charged(),
            empty + exchange_bytes(announced).expect("bounded"),
            "the announced length and the answer allowance, charged before the buffer existed"
        );
        assert!(
            admission.bytes_charged() < empty + exchange_bytes(MAX_MESSAGE).expect("bounded"),
            "and nothing like the largest message a client could have announced"
        );
        assert_eq!(
            admission.records_charged(),
            1,
            "the transaction's descriptor"
        );
        assert_eq!(
            crate::owned::peak().0.buffers,
            1,
            "one buffer, and it is the admitted one"
        );
        assert!(asked.try_recv().is_err(), "the platform was not asked");

        // The transport went away before it filled anything: the reservation ends where it is.
        drop(query);
        reserved.end(&mut admission);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(crate::owned::peak().0.buffers, 0);

        // Now a length nothing can be granted for. Everything but a sliver of the aggregate is held by
        // something else, so neither tier fits.
        let held = admission
            .reserve(Request::bytes(
                admission.byte_total() - admission.bytes_charged() - 1_000,
                Class::Reserved,
            ))
            .expect("the rest of the aggregate");
        let charged = admission.bytes_charged();
        assert!(
            queries.reserve(MAX_MESSAGE, &mut admission).is_none(),
            "the largest message a prefix can describe does not fit"
        );
        assert_eq!(
            admission.bytes_charged(),
            charged,
            "a refused query charges nothing"
        );
        assert_eq!(
            crate::owned::peak().0.buffers,
            0,
            "and allocates nothing: not the message, not a fraction of it"
        );
        assert!(asked.try_recv().is_err(), "and asks the platform nothing");
        assert_eq!(admission.records_charged(), 0);

        // Room again, and the very next question is admitted: a refusal is one query's outcome, not the
        // stream's.
        admission.release(held);
        let (reserved, query) = queries
            .reserve(announced, &mut admission)
            .expect("the next question is admitted as usual");
        drop(query);
        reserved.end(&mut admission);
        assert_eq!(admission.bytes_charged(), empty);
        queries.release(&mut admission);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A query the descriptor floor has no room for is still answered: the second tier costs the answer and
    /// no descriptor at all, and what the client gets is that query's own SERVFAIL.
    ///
    /// This is the difference between a refusal and silence. Both leave the stream open; only one of them
    /// tells the client to stop waiting, and the client cannot know which happened unless something arrives.
    #[tokio::test]
    async fn a_query_with_no_descriptor_left_is_answered_here() {
        crate::owned::reset();
        let mut admission = admission();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);
        let empty = admission.bytes_charged();

        // Every descriptor this session may hold is held by something else, and bytes are not the problem.
        let records = admission
            .reserve(Request::records(admission.record_total(), Class::Reserved))
            .expect("every record");
        let message = query(0x2b2b);
        let (reserved, mut query) = queries
            .reserve(message.len(), &mut admission)
            .expect("the second tier admits it");
        assert!(
            !reserved.submittable(),
            "with no descriptor there is no transaction to open"
        );
        assert_eq!(
            admission.bytes_charged(),
            empty + answered_here_bytes(message.len()).expect("bounded"),
            "the query and the answer built from it, and no descriptor"
        );
        assert_eq!(query.extend_within_capacity(&message), message.len());

        // Answered here, under the grant that already covered it.
        let (mut serving, mut control, _filled) = serving();
        let answering = super::super::answered_here(reserved, query, &mut serving, &mut admission)
            .expect("a well-formed query is answerable");
        assert_eq!(
            crate::owned::peak().1.buffers,
            2,
            "the query and its answer, never a third"
        );
        assert_eq!(
            crate::owned::peak().0.buffers,
            1,
            "and the query died as soon as its answer was built from it"
        );
        assert!(
            !answering.hand_over(&mut admission, &mut serving),
            "nothing was parked here before"
        );
        let delivery = serving
            .parked()
            .expect("parked before the answer was handed over");
        let Some(Control::Answered(Answered::Delivered {
            delivery: named,
            result,
        })) = control.recv().await
        else {
            panic!("the transport is handed the answer it can acknowledge")
        };
        assert_eq!(named, delivery, "and it names the delivery that is parked");
        assert_eq!(&result[..2], &message[..2], "that query's own identifier");
        assert_eq!(result[3] & 0x0f, 2, "and SERVFAIL");
        assert_eq!(
            admission.bytes_charged(),
            empty + super::super::delivery_bytes(result.capacity()),
            "reconciled to exactly what physically survives"
        );
        assert!(asked.try_recv().is_err(), "the platform was never asked");
        assert_eq!(admission.records_charged(), admission.record_total());

        drop(result);
        assert_eq!(
            serving.acknowledge(&mut admission, delivery),
            dns_debt::Acked::Released
        );
        assert_eq!(admission.bytes_charged(), empty);
        admission.release(records);
        queries.release(&mut admission);
        assert_eq!(crate::owned::peak().0.buffers, 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A submission the platform never received is one query's own expected failure: it settles into that
    /// query's SERVFAIL and refunds whole, and nothing of Android's is held.
    #[tokio::test]
    async fn a_submission_that_never_reached_the_platform_settles_as_a_servfail() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let empty = admission.bytes_charged();
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let injected = queries.answered_by(answers);
        injected.force(Outcome::NeverReached);

        let message = query(0x3131);
        outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &message,
        ));
        assert!(
            asked.try_recv().is_err(),
            "a submission the platform refused handed nothing over"
        );
        assert_eq!(queries.quarantined(), 0, "and holds nothing of Android's");

        let settlement = queries.finished().await;
        let delivered = queries
            .settle(settlement, &mut admission)
            .expect("an observable transaction settles");
        assert!(
            delivered.has_answer(),
            "the client is told, not left waiting"
        );
        let (mut serving, mut control, _filled) = serving();
        assert!(!delivered
            .answering()
            .hand_over(&mut admission, &mut serving));
        let Some(Control::Answered(Answered::Delivered { delivery, result })) =
            control.recv().await
        else {
            panic!("a refused submission still answers its query")
        };
        assert_eq!(&result[..2], &message[..2], "that query's own identifier");
        assert_eq!(result[3] & 0x0f, 2, "and SERVFAIL");
        drop(result);
        assert_eq!(
            serving.acknowledge(&mut admission, delivery),
            dns_debt::Acked::Released
        );
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(crate::owned::peak().0.buffers, 0);
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The table refuses a row rather than growing to hold one, and the reservation comes back whole for the
    /// local-answer path with the platform never asked.
    ///
    /// Unreachable in the daemon - the room check at reservation is what makes it so - and forced here
    /// because a refusal that stranded a reservation would be capacity nothing ever gives back, and because
    /// "infallible after the grant" has to be a claim about a path that exists rather than one that does not.
    #[tokio::test]
    async fn a_refused_row_hands_its_reservation_back_whole() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let empty = admission.bytes_charged();
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);

        let message = query(0x4141);
        queries.refuse_next_insert();
        let refused = published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &message,
        );
        let Submitted::Refused(reserved, query) = refused else {
            panic!("the insertion was armed to refuse")
        };
        assert_eq!(queries.len(), 0, "no row was taken");
        assert!(
            asked.try_recv().is_err(),
            "and the platform was never asked"
        );
        assert_eq!(&query[..], &message[..], "the exact query came back");

        // ...and it is answered here, under the grant that already covered it.
        let (mut serving, mut control, _filled) = serving();
        let answering = super::super::answered_here(reserved, query, &mut serving, &mut admission)
            .expect("a well-formed query is answerable");
        assert!(!answering.hand_over(&mut admission, &mut serving));
        let Some(Control::Answered(Answered::Delivered { delivery, result })) =
            control.recv().await
        else {
            panic!("a refused row still answers its query")
        };
        assert_eq!(&result[..2], &message[..2], "that query's own identifier");
        assert_eq!(result[3] & 0x0f, 2, "and SERVFAIL");
        drop(result);
        assert_eq!(
            serving.acknowledge(&mut admission, delivery),
            dns_debt::Acked::Released
        );
        assert_eq!(admission.bytes_charged(), empty, "refunded exactly once");
        assert_eq!(admission.records_charged(), 0);

        // The very next question is published as usual: a refusal is one query's outcome, not the table's.
        let key = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &message,
        ));
        let held = asked.recv().await.expect("the next question was asked");
        held.answer.send(b"real".to_vec()).expect("its row waits");
        let settlement = queries.finished().await;
        assert_eq!(settlement.key(), key);
        queries
            .settle(settlement, &mut admission)
            .expect("an observable transaction settles")
            .discard(&mut admission);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(crate::owned::peak().0.buffers, 0);
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A settled answer with nothing to deliver parks no delivery at all.
    ///
    /// The bug this closes is invisible in a balance and permanent in a session: `hand_over` used to park
    /// *every* settled result and only then discover it was a failure, so the transport was refused while a
    /// delivery sat on its flow with no identity anyone would ever name. That grant could only end when the
    /// whole flow closed - and for a DNS-over-TCP transport that is a connection a client may keep open for
    /// as long as it likes.
    #[tokio::test]
    async fn a_terminal_failure_parks_nothing_and_refunds_at_once() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let empty = admission.bytes_charged();
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let injected = queries.answered_by(answers);
        // Android took the question and this process cannot watch it: the local wrapper failing is the one
        // outcome that is neither an answer nor something a client can drive.
        injected.force(Outcome::Unobservable);

        let message = query(0x5151);
        let (reserved, mut buffer) = queries
            .reserve(message.len(), &mut admission)
            .expect("room for one exchange");
        assert_eq!(buffer.extend_within_capacity(&message), message.len());
        let charged = admission.bytes_charged();
        assert!(
            charged > empty,
            "the exchange really owed something before it was published"
        );
        let submitted = queries.submit(
            7,
            Stamp::default(),
            flow(1),
            reserved,
            buffer,
            &mut admission,
        );
        let Submitted::Unobservable { failure, .. } = submitted else {
            panic!("the local wrapper was armed to fail after the platform took it")
        };
        assert!(
            failure.reportable().is_some(),
            "this daemon's own step failing is a structured report, not a client-driven outcome"
        );
        assert!(
            asked.recv().await.is_some(),
            "the platform really did receive the question"
        );
        assert_eq!(queries.len(), 0, "and no row survives it");
        assert_eq!(
            admission.bytes_charged(),
            empty,
            "every byte of that exchange came back, once"
        );
        assert_eq!(admission.records_charged(), 0, "and its descriptor with it");
        assert_eq!(
            crate::owned::peak().0.buffers,
            0,
            "the query died before its grant was given back"
        );
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A closed transport's token that reached its question and then cannot reach this table is never handed
    /// back, even though every buffer that question owned dies on time.
    ///
    /// The composition is the point, and every half of it but one is real. A DNS-over-TCP transport closes
    /// while its question is outstanding, so [dns_debt::close] hands its logical token to that question's
    /// debt - the token now sits on a grant whose only terminal is a settlement. The platform then keeps a
    /// question this process has stopped being able to watch, so that settlement has to move the token onto
    /// this table's session-lived grant. When *that* move cannot be represented there is no honest local
    /// recovery: settling normally splits the delivery off the debt and releases the rest, which hands back a
    /// token for a resolver slot Android is still holding - and a token that looked free for an instant is a
    /// second query admitted against a limiter with no room for it.
    ///
    /// So the answer is fail-closed in exactly one direction. The physical owners die here on the ordinary
    /// path - the resolver's half, the answer, then the query - and the grant carrying the token stays
    /// charged for the rest of the session, which is a charge larger than what exists rather than smaller.
    /// Nothing is delivered, and nothing needs to be: a token on the debt means the transport that asked
    /// closed to put it there, so there is nobody an answer could reach.
    #[tokio::test]
    async fn an_unrepresentable_unobservable_token_is_kept_rather_than_refunded() {
        // Taken before anything that can report, and therefore released after all of it: locals drop in
        // reverse declaration order, so a guard taken further down would be gone while the owners created
        // above it were still dropping - and a `Workers` owner reports a task that did not complete as it
        // goes. That report would then land in whichever conversation had installed itself next.
        let _reporting = crate::report::exclusive().await;
        // Nothing downstream of this settlement exists to describe the failure - it returns `None`, so no
        // delivery reaches an engine and no refusal reaches a transport - which is why this owner reports it
        // and why the count is asserted here rather than left to a terminal that never runs.
        // Not `published`: that is this module's own helper for publishing a query, and shadowing it here
        // would make the exchange below unbuildable.
        let (control, mut nonfatals) = tokio::sync::mpsc::unbounded_channel();
        let reporter = crate::report::init_owned(control.clone(), |_, _| Vec::new())
            .expect("no other conversation owns reporting");
        let collector = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(message) = nonfatals.recv().await {
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

        crate::owned::reset();
        let mut admission = admission();
        let leases = admission.outstanding_leases();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        let injected = queries.answered_by(answers);
        // Watched and then lost, rather than never watched: the platform has the question and this process
        // can no longer see it end, which is what puts a token permanently at risk.
        injected.force(Outcome::LostWhileWatching);

        let key = outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &query(0x6161),
        ));
        assert!(
            asked.recv().await.is_some(),
            "the platform really was asked"
        );

        // The transport closes while its question is outstanding, through the production close, so what the
        // settlement below finds is a real transfer's result rather than a hand-placed token.
        let mut connection = dns_debt::open(&mut admission, 4_096, true).expect("a connection");
        connection.asking(Some(key));
        assert!(
            dns_debt::close(&mut admission, connection, queries.debt(key)).is_ok(),
            "the token reached the question instead of stranding on the connection"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "one token, and it is on the debt now"
        );
        let charged = admission.bytes_charged();
        let records = admission.records_charged();

        // And the move from that debt onto this table cannot be represented.
        queries.refuse_next_quarantine();
        let settlement = queries.finished().await;
        assert!(
            queries.settle(settlement, &mut admission).is_none(),
            "there is nothing to deliver and no transport left to hand it to"
        );

        assert_eq!(queries.len(), 0, "the row is gone");
        assert_eq!(queries.unobservable, 1);
        assert_eq!(
            queries.unquarantined, 1,
            "the move that could not be represented is counted where it happened"
        );
        assert_eq!(
            queries.quarantined.len(),
            0,
            "and nothing was quarantined, because nothing could be"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "the token was never returned for a slot the platform still holds"
        );
        assert_eq!(
            crate::owned::peak().0.buffers,
            0,
            "while the query buffer really did die"
        );
        assert_eq!(
            admission.bytes_charged(),
            charged,
            "the grant that carries the token is kept whole rather than released"
        );
        assert_eq!(
            admission.records_charged(),
            records,
            "including its descriptor record, which is the conservative direction"
        );

        // Even the table that could not take the token goes before the token does.
        queries.release(&mut admission);
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "one grant outlives the table, which is what the exit report shows"
        );
        assert_eq!(
            admission.outstanding_leases(),
            leases + 1,
            "exactly one, and nothing else leaked beside it"
        );
        // No ledger contradiction was fabricated to get here: the refusal stands in for the state, and the
        // ledger itself was never asked to do anything it could not.
        assert_eq!(admission.invariant_violations(), 0);

        reporter.finish().await.expect("the flush completes");
        drop(control);
        let reports = collector.await.expect("the collector joined");
        let [reported] = &reports[..] else {
            panic!("one report for one lost slot, not {}", reports.len())
        };
        assert!(
            reported.message.contains("can no longer observe"),
            "and it says what was lost rather than only counting it: {reported:?}"
        );
    }

    /// A settled result that can never be delivered parks no delivery at all, and its grant ends at once.
    ///
    /// The bug this closes is invisible in a balance and permanent in a session: `hand_over` used to park
    /// *every* settled result and only then discover it was a failure, so the transport was refused while a
    /// delivery sat on its flow carrying an identity nobody would ever name. That grant could only end when
    /// the whole flow closed - and for a DNS-over-TCP transport that is a connection a client may keep open
    /// for as long as it likes, which is a client-driven leak.
    ///
    /// Reached the way it really is: a query too malformed for even a SERVFAIL, whose expected platform
    /// failure therefore has nothing to become. Everything else a client can drive is already an answer by
    /// the time it gets here.
    #[tokio::test]
    async fn a_result_that_cannot_be_delivered_parks_nothing() {
        crate::owned::reset();
        let mut admission = admission();
        let baseline = admission.bytes_charged();
        let mut queries = Transactions::new(&mut admission).expect("a table");
        let empty = admission.bytes_charged();
        let (answers, mut asked) = tokio::sync::mpsc::unbounded_channel();
        queries.answered_by(answers);

        // No question section, so there is nothing for a SERVFAIL to echo.
        let malformed = b"a question".to_vec();
        outstanding(published(
            &mut queries,
            &mut admission,
            7,
            Stamp::default(),
            1,
            &malformed,
        ));
        // Dropped, which is what a refusal, a timeout or a full per-UID limiter arrives as.
        drop(asked.recv().await.expect("the transaction asked"));

        let settlement = queries.finished().await;
        let delivered = queries
            .settle(settlement, &mut admission)
            .expect("an observable transaction settles");
        assert!(
            delivered.has_answer(),
            "there is a settled result; it just cannot be delivered"
        );
        let charged = admission.bytes_charged();
        assert!(charged > empty, "and its delivery grant is still held");

        let (mut serving, mut control, _filled) = serving();
        assert!(!delivered
            .answering()
            .hand_over(&mut admission, &mut serving));
        assert_eq!(
            serving.parked(),
            None,
            "nothing was parked for a value no acknowledgment could ever name"
        );
        assert_eq!(
            admission.bytes_charged(),
            empty,
            "and the grant ended here rather than waiting for the flow to close"
        );
        let Some(Control::Answered(Answered::Refused(_))) = control.recv().await else {
            panic!("the transport is told its stream is over")
        };
        assert_eq!(crate::owned::peak().0.buffers, 0);
        queries.release(&mut admission);
        assert_eq!(admission.bytes_charged(), baseline);
        assert_eq!(admission.invariant_violations(), 0);
    }
}
