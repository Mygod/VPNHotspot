//! The resolver transactions an engine's DNS-over-TCP flows have outstanding, and what each of them owes.
//!
//! Apart from the engine's flow table because a retirement joins that one and may not touch this one, and
//! keyed by its own identity rather than by the flow's handle, which the client-side stack reuses once the
//! flow is gone. That independence is the invariant: a transaction outlives the transport that asked for it,
//! survives a config handover, and ends when this daemon's own side of it is over - the platform answered, or
//! the wrapper watching for that answer failed - never when a client's connection goes away. Android's
//! resolver work is not what ends a row and is not waited for: it finishes when it finishes, which nothing
//! here observes.
//!
//! # A lifetime, not a task
//!
//! This is a fixed-capacity table the ingress owner polls. [Transactions::poll_finished] scans the prepared
//! rows, takes exactly one that has reached a terminal this process can see - an answer, what the platform
//! answered instead, or this daemon's own wrapper around the descriptor failing - and removes it. Nothing is
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
use vpnhotspotd::shared::dns_debt::{self, Delivery, QueryDebt};
use vpnhotspotd::shared::dns_wire::resolved;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::model::Network;

use crate::shizuku::owned::Owned;
use crate::shizuku::resolver::Resolving;
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
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Vec<u8>, Failure>> {
        match self {
            Self::Resolver(resolving) => resolving.poll_result(cx),
            Self::Failed(failure) => Poll::Ready(Err(failure.take().unwrap_or_else(unreached))),
            // Unreachable: `submit` replaces it before it returns. Answered rather than left pending, because
            // a row nothing can ever settle is a grant nothing gives back.
            Self::Unsubmitted => Poll::Ready(Err(unreached())),
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
    /// Ends one row whole, for the two reasons a row ends without an answer: the session is over, or this
    /// daemon's own wrapper around its transaction failed and the owner is about to end on that.
    ///
    /// The platform transaction's descriptor, whatever the resolver left behind and the query are all dropped
    /// first, and only then is the grant that accounted for them released.
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
}

impl Settlement {
    /// Which transaction this is, for an owner that keyed something else by it.
    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    /// The exact transport that asked, both halves, for an owner that has to reach it whatever this
    /// settlement turns out to be. Read before the settle rather than out of the delivery it produces,
    /// because a settlement that ends the session produces none.
    pub(crate) fn flow(&self) -> Event {
        self.pending.flow
    }
}

/// What publishing one accepted query came to.
pub(crate) enum Submitted {
    /// The row is in the table and the platform has the question - or has refused it, which is that query's
    /// own outcome and settles into its SERVFAIL at the next poll.
    Outstanding(u64),
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

    /// Reconciles this reservation down to the bytes still allocated after answering the query here, and
    /// names the delivery by the same identity a submitted query's would have used.
    pub(crate) fn settle(self, admission: &mut Admission, delivery_bytes: u64) -> Delivery {
        dns_debt::settle(admission, self.debt, delivery_bytes)
    }

    /// Nobody will submit or answer this: the transport is gone, or nothing could be built for it.
    ///
    /// Whatever buffer it covered must already be dropped - a release while those bytes are alive is capacity
    /// refunded for memory this process is still holding. No descriptor was opened and the platform was never
    /// asked, so there is nothing else to settle.
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
    /// The table's own retained capacity, charged once for the session.
    tables: Lease,
    /// Framed queries nothing could be granted for, which the transport skips.
    skipped: u64,
}

impl Transactions {
    /// What a table prepared for `tokens` transactions owns, whatever is in it.
    ///
    /// Charged once by its owner and kept charged until the table is dropped, because it is a charge on the
    /// prepared bound rather than on the rows currently in it. Checked throughout: a figure that would wrap is
    /// a capacity that cannot be accounted for and therefore must not be prepared.
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
            skipped: 0,
        })
    }

    /// Releases the table's own capacity, after every row is settled. One release, so there is no second path
    /// to get it wrong on.
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
        // transaction frees its row for the next question; the map's own backing is opaque count-bounded
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
    ///
    /// `Err` is this daemon's own wrapper around the descriptor Android returned having failed. That is not
    /// one query's outcome - an owner that could not wrap this descriptor cannot wrap the next either - so
    /// the row is drained here and the failure ends the ingress task rather than this one transport.
    pub(crate) fn submit(
        &mut self,
        network: Network,
        stamp: Stamp,
        flow: Event,
        reserved: Reserved,
        query: Owned,
        admission: &mut Admission,
    ) -> io::Result<Submitted> {
        // Re-checked here because the reservation above and this insertion are separate owner steps and the
        // table may have taken a row in between.
        if self.rows.len() >= self.prepared {
            return Ok(Submitted::Refused(reserved, query));
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
            None => Err(unreached()),
        };
        let failure = match submission {
            Ok(resolving) => {
                self.set(id, Awaiting::Resolver(resolving));
                return Ok(Submitted::Outstanding(id));
            }
            Err(failure) => failure,
        };
        match failure.ending([("transaction", id)]) {
            // Nothing of the platform's is held for this process to wrap, so this is one query's own expected
            // failure: the row settles at the next poll and the client gets its SERVFAIL on a stream that
            // carries on.
            Ok(expected) => {
                self.set(id, Awaiting::Failed(Some(expected)));
                Ok(Submitted::Outstanding(id))
            }
            // This daemon's own wrapper around the descriptor Android returned. Everything this row held
            // goes back: the descriptor was cancelled and closed by the dropped submission, and the query,
            // the answer allowance and the DNS-class descriptor record are this process's. The transport's
            // own logical token is untouched here because a query never held one - and nothing is recorded
            // on the flow for this question, so that transport's close releases the token with its own grant
            // rather than trying to hand it to a row that is gone. Nothing is reported here either: this
            // failure travels, and the session that ends on it delivers the report it already carries.
            Err(ending) => {
                if let Some(pending) = self.rows.remove(&id) {
                    pending.drain(admission);
                }
                Err(ending)
            }
        }
    }

    /// Stores what a freshly inserted row is waiting on. Separate only because the borrow that read the
    /// query for the platform call has to end first.
    fn set(&mut self, id: u64, awaiting: Awaiting) {
        if let Some(pending) = self.rows.get_mut(&id) {
            pending.awaiting = awaiting;
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
                // Classified here, where the query it was for is still owned, so what leaves this table is
                // either something to put on the stream or something that ends it - never a platform outcome
                // the transport would have to guess about. Owned from here: this is the buffer the daemon
                // carries through the settle, the park and the handoff.
                let result = resolved(completed, &pending.message).map(Owned::new);
                ready = Some((*key, result));
                break;
            }
        }
        let Some((key, result)) = ready else {
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
        })
    }

    /// Settles one finished transaction, and hands back what the *delivery* after it still owns.
    ///
    /// The descriptor record and any logical token a closing transport handed here end with it, because this
    /// process's side of the transaction is over: the descriptor is closed and nothing else will be read from
    /// it. That is not a claim about Android, whose own resolver work for the same query ends when it ends -
    /// the accounting released here is this daemon's own. The answer does not end with it either: the
    /// transport has yet to receive it, classify it, frame it and write the framing into its flow's bridge,
    /// and every one of those buffers exists after this returns.
    ///
    /// `Err` is this daemon's own wrapper around the transaction having failed while it was being watched.
    /// There is nothing to deliver then and nothing to tell a transport: the owner that could not watch this
    /// transaction cannot watch the next one, so it ends. Everything this row held goes back in the order
    /// every other terminal here uses - the resolver's own half, then the query, and only then the grant,
    /// including a token a closed transport had handed here - and the failure travels out carrying the one
    /// report it will ever produce.
    pub(crate) fn settle(
        &mut self,
        settlement: Settlement,
        admission: &mut Admission,
    ) -> io::Result<Delivered> {
        let Settlement {
            key,
            pending,
            result,
        } = settlement;
        let Pending {
            debt,
            message,
            stamp,
            network,
            awaiting,
            ..
        } = pending;
        // This row's own side of the transaction is over by construction - it produced this result, whether
        // that was an answer or this daemon's wrapper around the descriptor failing - so what is left of it
        // is dropped here rather than carried into a delivery that has nothing to do with it. Dropping it
        // closes this process's descriptor and nothing more: Android's resolver work for the same query is
        // not ended by it and is not waited for.
        drop(awaiting);
        let result = match result {
            Ok(answer) => Ok(answer),
            Err(failure) => match failure.ending([("transaction", key)]) {
                Ok(expected) => Err(expected),
                Err(ending) => {
                    drop(message);
                    dns_debt::abandon(admission, debt);
                    return Err(ending);
                }
            },
        };
        Ok(Delivered::new(
            dns_debt::Settled::delivering(
                dns_debt::settle(admission, debt, DELIVERY_BYTES),
                Resolved::new(result, Some(message)),
            ),
            stamp,
            network,
        ))
    }

    /// The session is over: every row goes, each dropping what it holds before its grant is released.
    ///
    /// Not a cancellation that reclaims capacity - the process is about to exit. Dropping a row returns this
    /// process's descriptor, which is as far as a process can get: Android's own operation ends when its
    /// resolver work returns, and nothing here can observe or wait for that.
    pub(crate) fn shutdown(&mut self, admission: &mut Admission) {
        for (_, pending) in self.rows.drain() {
            pending.drain(admission);
        }
    }

    /// Nothing about a duplicate settlement appears here, and that is deliberate:
    /// [Transactions::poll_finished] removes the row it yields, and a [Settlement] is a value that carries
    /// it - so a second settlement for the same transaction is unrepresentable rather than counted.
    pub(crate) fn describe(&self) -> String {
        format!(
            "{} outstanding transactions, skipped {}",
            self.rows.len(),
            self.skipped
        )
    }
}
