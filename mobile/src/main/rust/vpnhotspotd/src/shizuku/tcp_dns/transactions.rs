//! Readiness-driven owner of submitted resolver transactions.
//!
//! Each query has one row and one table-polled future, independent of its flow. No task is spawned or
//! detached: this owner polls and drops each future directly, so shutdown needs neither abort nor join;
//! Android's work remains unjoinable. Missing rows or waits are reported as table invariants.
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::stream::FuturesUnordered;
use futures_util::{FutureExt, StreamExt};

use vpnhotspotd::shared::dns_debt::{self, QueryDebt};
use vpnhotspotd::shared::dns_wire::resolved;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::protocol::IoErrorReportExt;

use crate::report;
use crate::shizuku::owned::Owned;
use crate::shizuku::resolver::Resolving;
use crate::shizuku::tcp_flow::Event;

use super::{Delivered, Resolved};

/// Where a resolver transaction's own failure is reported from when it finished with no owner left to end.
const INCOMPLETE: &str = "shizuku.tcp_dns.transaction";

/// Report context for a broken completion/row bijection.
const MISMATCH: &str = "shizuku.tcp_dns.transaction_mismatch";

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

/// One table-owned query future.
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
    // Includes both flow halves; a reused handle alone cannot identify the requester.
    flow: Event,
}

/// Builds the terminal error for a missing transaction row or wait.
fn mismatched(transaction: u64, missing: &str) -> io::Error {
    io::Error::other(format!(
        "resolver transaction {transaction} has no {missing}, which this table's own bookkeeping should \
         make impossible"
    ))
    .with_report_context(MISMATCH)
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
    pub(crate) fn settle(self) -> dns_debt::Delivery {
        dns_debt::settle(self.debt)
    }
}

pub(crate) struct Transactions {
    rows: HashMap<u64, Pending>,
    /// One future per row. `None` after shutdown takes the collection; taking avoids
    /// `FuturesUnordered::clear`'s replacement allocation and permits direct iteration.
    completions: Option<FuturesUnordered<Completion>>,
    next: u64,
    skipped: u64,
}

impl Transactions {
    /// Waits still held; zero after shutdown takes the collection.
    fn waiting(&self) -> usize {
        self.completions.as_ref().map_or(0, |waits| waits.len())
    }

    /// Active invariant: one wait per row.
    fn consistent(&self) -> bool {
        self.waiting() == self.rows.len()
    }

    /// Outstanding queries, used to verify flow closure leaves them untouched.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn new() -> Self {
        Self {
            rows: HashMap::new(),
            completions: Some(FuturesUnordered::new()),
            next: 0,
            skipped: 0,
        }
    }

    pub(crate) fn release(self) {
        debug_assert!(
            self.waiting() == 0 && self.rows.is_empty(),
            "every wait and every row is ended before this table is released"
        );
        drop(self.rows);
        drop(self.completions);
    }
    pub(crate) fn reserve(&mut self, length: usize) -> Option<(Reserved, Owned)> {
        // One checked identity names both the transaction and its eventual delivery.
        let Some(next) = self.next.checked_add(1) else {
            self.skipped += 1;
            return None;
        };
        let id = self.next;
        let debt = dns_debt::submit(id);
        self.next = next;
        let query = Owned::with_capacity(length);
        Some((Reserved { id, debt }, query))
    }

    pub(crate) fn submit(
        &mut self,
        flow: Event,
        reserved: Reserved,
        query: Owned,
    ) -> io::Result<Submitted> {
        // Reservation and insertion are separate owner turns.
        // Shutdown has taken the completion collection, so no platform work can start.
        let Some(completions) = self.completions.as_mut() else {
            return Ok(Submitted::Refused(reserved, query));
        };
        let Reserved { id, debt, .. } = reserved;
        // Install any returned descriptor in its future before returning to the owner loop.
        let awaiting = match crate::shizuku::resolver::submit(&query) {
            Ok(resolving) => Awaiting::Resolver(resolving),
            Err(failure) => match failure.ending([("transaction", id)]) {
                Ok(expected) => Awaiting::Refused(Some(expected)),
                Err(ending) => {
                    drop(query);
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
                flow,
            },
        );
        debug_assert!(
            self.consistent(),
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
            self.consistent(),
            "a settled query leaves neither a wait nor a row"
        );
        let result = resolved(completed, &pending.message).map(Owned::new);
        Poll::Ready(Ok(Settlement {
            key,
            pending,
            result,
        }))
    }

    pub(crate) fn settle(&mut self, settlement: Settlement) -> io::Result<Delivered> {
        let Settlement {
            key,
            pending,
            result,
        } = settlement;
        let Pending { debt, message, .. } = pending;
        let result = match result {
            Ok(answer) => Ok(answer),
            Err(failure) => match failure.ending([("transaction", key)]) {
                Ok(expected) => Err(expected),
                Err(ending) => {
                    drop(message);
                    return Err(ending);
                }
            },
        };
        let delivery = dns_debt::settle(debt);
        Ok(Delivered::new(dns_debt::Settled::delivering(
            delivery,
            Resolved::new(result, Some(message)),
        )))
    }

    /// Ends every transaction without waiting, returning the first observable local failure.
    ///
    /// Takes and polls each stored future once so one aggregate `Pending` cannot hide another ready future,
    /// while avoiding `FuturesUnordered::clear`'s replacement allocation. Missing rows or waits are reported.
    pub(crate) fn shutdown(&mut self) -> io::Result<()> {
        let mut ended = Ok(());
        for completion in self.completions.take().into_iter().flatten() {
            let id = completion.id;
            // Consume one direct poll before dropping the resolver future.
            let completed = completion.now_or_never();
            let row = self.rows.remove(&id);
            if row.is_none() {
                ended = report::keep_first(MISMATCH, ended, Err(mismatched(id, "row")));
            }
            let carried = match completed {
                Some((_, Err(failure))) => failure.ending([("transaction", id)]).err(),
                Some((_, Ok(_))) | None => None,
            };
            if let Some(ending) = carried {
                ended = report::keep_first(INCOMPLETE, ended, Err(ending));
            }
            drop(row);
        }
        // Remaining rows have no waits; report before draining them.
        for (id, pending) in self.rows.drain() {
            ended = report::keep_first(MISMATCH, ended, Err(mismatched(id, "wait")));
            drop(pending);
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
