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
//! This is a fixed-capacity table the ingress owner polls. [Transactions::finished] scans the prepared
//! rows, takes exactly one whose platform transaction has reached its terminal, and removes it. Nothing is
//! spawned, nothing is cancelled to reclaim capacity, and dropping a row is what returns this process's
//! descriptor. A retirement does not touch these rows, and a row settles into whatever its own stamp says it
//! is.
//!
//! # The commit order
//!
//! A row is inserted *irrevocably* and then the platform is called, synchronously, on the owner's own task.
//! There is no await, no allocation and no refusal between the two, so there is no window in which
//! `android_res_nsend` has a question this table is not accounting for. Everything that can refuse - room,
//! the identity, both grants, the buffer - happens at [Transactions::reserve], before a byte of the client's
//! message is stored.

use std::collections::HashMap;
use std::io;
use std::task::{Context, Poll};

use vpnhotspotd::shared::admission::{logical_footprint, Admission, Class, Denied, Lease, Request};
use vpnhotspotd::shared::dns_debt::{self, Delivery, Quarantine, QueryDebt};
use vpnhotspotd::shared::dns_wire::resolved;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::model::Network;

use crate::shizuku::owned::Owned;
use crate::shizuku::resolver::{Completed, Resolving, Submission};
use crate::shizuku::tcp_flow::Event;
use crate::shizuku::tun_writer::Stamp;

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
    /// platform returns, and the framed copy on its way into the flow's bridge.
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
/// A value rather than a borrow, so the owner can hold one across a config application - the race the
/// retained stamp exists for - and settle it afterwards in a deterministic order.
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
        })
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
        // Re-checked here because the reservation above and this insertion are separate owner steps and the
        // table may have taken a row in between.
        if self.rows.len() >= self.prepared {
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
            Some(pending) => crate::shizuku::resolver::submit(network, &pending.message),
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
        // that asked, and a transport ending on a local failure is reported once by [crate::shizuku::tcp] where every
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
    /// Polled rather than awaited, because the ingress task registers this beside the flow table and the
    /// rooms in one turn and cannot hold a future for each - see [crate::shizuku::tcp::Engine::attention]. It
    /// waits forever while nothing is outstanding rather than answering at once, and is cancellation-safe: a
    /// poll either takes exactly one ready row out and yields it in the same poll, or it changes nothing at
    /// all: there is no state in which a row has been removed and its result has not been handed to the
    /// caller, so abandoning this loses neither.
    pub(crate) fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Settlement> {
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
    /// answer does not: the transport has yet to receive it, classify it, frame it and write the framing into
    /// its flow's bridge, and every one of those buffers exists after this returns.
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
                    crate::shizuku::resolver::report_unobservable(key, failure);
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
            self.quarantined.count(),
            self.unquarantined
        )
    }
}
