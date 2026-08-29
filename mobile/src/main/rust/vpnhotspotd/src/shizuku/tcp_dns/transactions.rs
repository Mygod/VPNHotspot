//! Readiness-driven owner of submitted resolver transactions.
//!
//! Each query has one row and one table-polled future, independent of its flow. No task is spawned or
//! detached: this owner polls and drops each future directly, so shutdown needs neither abort nor join;
//! Android's work remains unjoinable. Missing rows or waits and uncovered delivery grants are reported as
//! table invariants.
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};

use vpnhotspotd::shared::admission::{logical_footprint, Admission, Denied, Lease};
use vpnhotspotd::shared::dns_debt::{self, QueryDebt};
use vpnhotspotd::shared::dns_wire::resolved;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::protocol::IoErrorReportExt;

use crate::report;
use crate::shizuku::owned::Owned;
use crate::shizuku::resolver::Resolving;
use crate::shizuku::tcp_flow::Event;
use crate::shizuku::tun_writer::Stamp;

use super::{exchange_bytes, Delivered, Resolved, DELIVERY_BYTES};

/// Where a resolver transaction's own failure is reported from when it finished with no owner left to end.
const INCOMPLETE: &str = "shizuku.tcp_dns.transaction";

/// Where an accounting invariant met while splitting a settled query's delivery is reported from.
const SPLIT: &str = "shizuku.tcp_dns.delivery_split";

/// Report context for a broken completion/row bijection.
const MISMATCH: &str = "shizuku.tcp_dns.transaction_mismatch";

/// Conservative upper bound for `FuturesUnordered`'s opaque per-future node overhead: about ten words,
/// doubled to avoid undercharging. Recheck when `futures-util` changes.
const COMPLETION_NODE_BYTES: u64 = (std::mem::size_of::<usize>() as u64) * 20;

/// Conservative fixed `FuturesUnordered` overhead: its queue plus one full-size stub node.
const COMPLETION_STATE_BYTES: u64 = COMPLETION_NODE_BYTES
    + std::mem::size_of::<Completion>() as u64
    + (std::mem::size_of::<usize>() as u64) * 10;

fn unreached() -> Failure {
    Failure::platform(io::Error::from(io::ErrorKind::NotConnected))
}

/// What one submitted query is waiting on.
enum Awaiting {
    /// The descriptor `android_res_nsend` returned, waiting only to be read.
    Resolver(Resolving),
    /// Synchronous platform refusal routed through normal settlement.
    Refused(Option<Failure>),
}

/// One table-owned query future, charged per row by [Transactions::footprint].
struct Completion {
    id: u64,
    awaiting: Awaiting,
}

impl Future for Completion {
    type Output = (u64, Result<Vec<u8>, Failure>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let completed = match &mut this.awaiting {
            Awaiting::Resolver(resolving) => match resolving.poll_result(cx) {
                Poll::Ready(completed) => completed,
                Poll::Pending => return Poll::Pending,
            },
            // A yielded completion is removed, so a second poll is an invariant failure.
            Awaiting::Refused(failure) => Err(failure.take().unwrap_or_else(unreached)),
        };
        Poll::Ready((this.id, completed))
    }
}

struct Pending {
    debt: QueryDebt,
    // Retained because settlement may need the original question to build SERVFAIL.
    message: Owned,
    stamp: Stamp,
    // Includes both flow halves; a reused handle alone cannot identify the requester.
    flow: Event,
    network: Network,
}

impl Pending {
    fn drain(self, admission: &mut Admission) {
        let Self { debt, message, .. } = self;
        drop(message);
        dns_debt::abandon(admission, debt);
    }
}

/// Reports a refused delivery split whose original grant still covers the buffers.
pub(super) fn report_split(transaction: u64, denied: Option<Denied>) {
    let Some(denied) = denied else {
        return;
    };
    report::message_with_details(
        SPLIT,
        "a settled DNS-over-TCP query kept the whole grant its delivery split was refused from",
        "InvalidData",
        [
            ("transaction", transaction.to_string()),
            ("denied", format!("{denied:?}")),
        ],
    );
}

/// Builds the terminal error for a missing transaction row or wait.
fn mismatched(transaction: u64, missing: &str) -> io::Error {
    io::Error::other(format!(
        "resolver transaction {transaction} has no {missing}, which this table's own bookkeeping should \
         make impossible"
    ))
    .with_report_context(MISMATCH)
}

/// Builds the terminal error for a delivery grant that covers none of its buffers.
pub(super) fn uncovered(transaction: u64, denied: Denied) -> io::Error {
    io::Error::other(format!(
        "a settled DNS-over-TCP query has no grant known to cover its buffers: transaction \
         {transaction}, {denied:?}"
    ))
    .with_report_context(SPLIT)
}

pub(crate) struct Settlement {
    key: u64,
    pending: Pending,
    result: Result<Owned, Failure>,
}

impl Settlement {
    pub(crate) fn flow(&self) -> Event {
        self.pending.flow
    }
}

pub(crate) enum Submitted {
    /// Table-owned; platform outcomes, including a later limiter refusal, arrive through its descriptor.
    Outstanding,
    Refused(Reserved, Owned),
}

pub(crate) struct Reserved {
    id: u64,
    debt: QueryDebt,
}

impl Reserved {
    /// Transaction identity retained for settlement reports.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Settles this reservation; the caller owns cleanup for [dns_debt::Split::Uncovered].
    pub(crate) fn settle(self, admission: &mut Admission, delivery_bytes: u64) -> dns_debt::Split {
        dns_debt::settle(admission, self.debt, delivery_bytes)
    }

    pub(crate) fn end(self, admission: &mut Admission) {
        dns_debt::abandon(admission, self.debt);
    }
}

pub(crate) struct Transactions {
    rows: HashMap<u64, Pending>,
    /// One future per row. `None` after shutdown takes the collection; taking avoids
    /// `FuturesUnordered::clear`'s replacement allocation and permits direct iteration.
    completions: Option<FuturesUnordered<Completion>>,
    // HashMap may round up; only `prepared` rows were charged.
    prepared: usize,
    next: u64,
    tables: Lease,
    skipped: u64,
}

impl Transactions {
    /// Charges rows, futures, readiness nodes and collection fixed state.
    pub(crate) fn footprint(queries: usize) -> Option<u64> {
        let per_completion =
            (std::mem::size_of::<Completion>() as u64).checked_add(COMPLETION_NODE_BYTES)?;
        logical_footprint::<(u64, Pending)>(queries)?
            .checked_add(u64::try_from(queries).ok()?.checked_mul(per_completion)?)?
            .checked_add(COMPLETION_STATE_BYTES)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// Waits still held; zero after shutdown takes the collection.
    fn waiting(&self) -> usize {
        self.completions.as_ref().map_or(0, |waits| waits.len())
    }

    /// Active invariant: one wait per row, bounded by prepared capacity.
    fn bounded(&self) -> bool {
        self.waiting() == self.rows.len() && self.rows.len() <= self.prepared
    }

    /// Outstanding queries, used to verify flow retirement leaves them untouched.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn new(admission: &mut Admission) -> Result<Self, Denied> {
        // Size and preallocate for the maximum number of queries admission can charge.
        let prepared = dns_debt::rows(admission);
        // Reserve before allocating either collection.
        let bytes = Self::footprint(prepared).ok_or(Denied::Arithmetic)?;
        let tables = dns_debt::tables(admission, bytes)?;
        Ok(Self {
            rows: HashMap::with_capacity(prepared),
            completions: Some(FuturesUnordered::new()),
            prepared,
            next: 0,
            tables,
            skipped: 0,
        })
    }

    /// Releases an empty or already-shutdown table.
    pub(crate) fn release(self, admission: &mut Admission) {
        debug_assert!(
            self.waiting() == 0 && self.rows.is_empty(),
            "every wait and every row is ended before this table's lease goes back"
        );
        drop(self.rows);
        drop(self.completions);
        admission.release(self.tables);
    }
    pub(crate) fn reserve(
        &mut self,
        length: usize,
        admission: &mut Admission,
    ) -> Option<(Reserved, Owned)> {
        if self.rows.len() >= self.prepared {
            self.skipped += 1;
            return None;
        }
        // One checked identity names both the transaction and its eventual delivery.
        let Some(next) = self.next.checked_add(1) else {
            self.skipped += 1;
            return None;
        };
        let id = self.next;
        // Reserve one descriptor and the query plus worst-case delivery bytes before allocating the query.
        let Some(debt) =
            exchange_bytes(length).and_then(|bytes| dns_debt::submit(admission, id, bytes).ok())
        else {
            self.skipped += 1;
            return None;
        };
        self.next = next;
        let query = Owned::with_capacity(length);
        Some((Reserved { id, debt }, query))
    }

    pub(crate) fn submit(
        &mut self,
        network: Network,
        stamp: Stamp,
        flow: Event,
        reserved: Reserved,
        query: Owned,
        admission: &mut Admission,
    ) -> io::Result<Submitted> {
        // Reservation and insertion are separate owner turns.
        if self.rows.len() >= self.prepared {
            return Ok(Submitted::Refused(reserved, query));
        }
        // Shutdown has taken the completion collection, so no platform work can start.
        let Some(completions) = self.completions.as_mut() else {
            return Ok(Submitted::Refused(reserved, query));
        };
        let Reserved { id, debt, .. } = reserved;
        // Install any returned descriptor in its future before returning to the owner loop.
        let awaiting = match crate::shizuku::resolver::submit(network, &query) {
            Ok(resolving) => Awaiting::Resolver(resolving),
            Err(failure) => match failure.ending([("transaction", id)]) {
                Ok(expected) => Awaiting::Refused(Some(expected)),
                Err(ending) => {
                    drop(query);
                    dns_debt::abandon(admission, debt);
                    return Err(ending);
                }
            },
        };
        completions.push(Completion { id, awaiting });
        self.rows.insert(
            id,
            Pending {
                debt,
                message: query,
                stamp,
                flow,
                network,
            },
        );
        debug_assert!(
            self.bounded(),
            "one submitted query owns one wait and one row"
        );
        Ok(Submitted::Outstanding)
    }

    /// Polls the next ready transaction without scanning rows. A completion without its row is terminal; a
    /// local completion failure remains primary and the mismatch is reported separately.
    pub(crate) fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Settlement>> {
        let Some(completions) = self.completions.as_mut() else {
            // Shutdown has taken every wait.
            return Poll::Pending;
        };
        let (key, completed) = match completions.poll_next_unpin(cx) {
            Poll::Ready(Some(finished)) => finished,
            // Nothing can complete until this owner submits another query.
            Poll::Ready(None) | Poll::Pending => return Poll::Pending,
        };
        let Some(pending) = self.rows.remove(&key) else {
            // Preserve a local failure even though the missing row has no client to answer.
            let carried = match completed {
                Err(failure) => failure.ending([("transaction", key)]).err(),
                Ok(_) => None,
            };
            let Some(ending) = carried else {
                return Poll::Ready(Err(mismatched(key, "row")));
            };
            // Keep the local failure primary and report the independent mismatch beside it.
            report::io(MISMATCH, mismatched(key, "row"));
            return Poll::Ready(Err(ending));
        };
        debug_assert!(
            self.bounded(),
            "a settled query leaves neither a wait nor a row"
        );
        let result = resolved(completed, &pending.message).map(Owned::new);
        Poll::Ready(Ok(Settlement {
            key,
            pending,
            result,
        }))
    }

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
            ..
        } = pending;
        // Removing the completion closes its descriptor before debt is released.
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
        let delivery = match dns_debt::settle(admission, debt, DELIVERY_BYTES) {
            dns_debt::Split::Covered(delivery, denied) => {
                report_split(key, denied);
                delivery
            }
            dns_debt::Split::Uncovered(debt, denied) => {
                // Drop response and query before abandoning their debt.
                drop(result);
                drop(message);
                dns_debt::abandon(admission, debt);
                return Err(uncovered(key, denied));
            }
        };
        Ok(Delivered::new(
            dns_debt::Settled::delivering(delivery, Resolved::new(result, Some(message))),
            stamp,
            network,
        ))
    }

    /// Ends every transaction without waiting, returning the first observable local failure.
    ///
    /// Takes and polls each stored future once so one aggregate `Pending` cannot hide another ready future,
    /// while avoiding `FuturesUnordered::clear`'s replacement allocation. Descriptors and buffers are dropped
    /// before their debts; missing rows or waits are reported.
    pub(crate) fn shutdown(&mut self, admission: &mut Admission) -> io::Result<()> {
        let mut ended = Ok(());
        for completion in self.completions.take().into_iter().flatten() {
            let id = completion.id;
            // Consume one direct poll; dropping the future closes its descriptor before debt release.
            let completed = completion.now_or_never();
            let row = self.rows.remove(&id);
            if row.is_none() {
                ended = report::keep_first(MISMATCH, ended, Err(mismatched(id, "row")));
            }
            // Consume any answer before releasing its grant.
            let carried = match completed {
                Some((_, Err(failure))) => failure.ending([("transaction", id)]).err(),
                Some((_, Ok(_))) | None => None,
            };
            if let Some(ending) = carried {
                ended = report::keep_first(INCOMPLETE, ended, Err(ending));
            }
            if let Some(pending) = row {
                pending.drain(admission);
            }
        }
        // Remaining rows have no waits; report before draining them.
        for (id, pending) in self.rows.drain() {
            ended = report::keep_first(MISMATCH, ended, Err(mismatched(id, "wait")));
            pending.drain(admission);
        }
        ended
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} outstanding transactions ({} still waiting), skipped {}",
            self.rows.len(),
            self.waiting(),
            self.skipped
        )
    }
}
