//! Owner-polled table of irrevocably submitted resolver transactions.
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

enum Awaiting {
    Unsubmitted,
    // Dropping Resolving closes our descriptor but does not cancel Android's resolver work.
    Resolver(Resolving),
    Failed(Option<Failure>),
}

fn unreached() -> Failure {
    Failure::platform(io::Error::from(io::ErrorKind::NotConnected))
}

impl Awaiting {
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Vec<u8>, Failure>> {
        match self {
            Self::Resolver(resolving) => resolving.poll_result(cx),
            Self::Failed(failure) => Poll::Ready(Err(failure.take().unwrap_or_else(unreached))),
            // Never leave an accounting row pending forever if submit's invariant is broken.
            Self::Unsubmitted => Poll::Ready(Err(unreached())),
        }
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
    awaiting: Awaiting,
}

impl Pending {
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

pub(crate) struct Settlement {
    key: u64,
    pending: Pending,
    result: Result<Owned, Failure>,
}

impl Settlement {
    pub(crate) fn key(&self) -> u64 {
        self.key
    }

    pub(crate) fn flow(&self) -> Event {
        self.pending.flow
    }
}

pub(crate) enum Submitted {
    Outstanding(u64),
    Refused(Reserved, Owned),
}

pub(crate) struct Reserved {
    id: u64,
    debt: QueryDebt,
    submittable: bool,
}

impl Reserved {
    pub(crate) fn submittable(&self) -> bool {
        self.submittable
    }

    pub(crate) fn settle(self, admission: &mut Admission, delivery_bytes: u64) -> Delivery {
        dns_debt::settle(admission, self.debt, delivery_bytes)
    }

    pub(crate) fn end(self, admission: &mut Admission) {
        dns_debt::abandon(admission, self.debt);
    }
}

pub(crate) struct Transactions {
    rows: HashMap<u64, Pending>,
    // HashMap may round up; only `prepared` rows were charged.
    prepared: usize,
    next: u64,
    tables: Lease,
    skipped: u64,
}

impl Transactions {
    pub(crate) fn footprint(tokens: usize) -> Option<u64> {
        logical_footprint::<(u64, Pending)>(tokens)?.checked_add(std::mem::size_of::<Self>() as u64)
    }

    pub(crate) fn new(admission: &mut Admission) -> Result<Self, Denied> {
        let prepared = admission.dns_token_cap() as usize;
        // Reserve before allocating either collection.
        let bytes = Self::footprint(prepared).ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Request::bytes(bytes, Class::Reserved))?;
        Ok(Self {
            rows: HashMap::with_capacity(prepared),
            prepared,
            next: 0,
            tables,
            skipped: 0,
        })
    }

    pub(crate) fn release(self, admission: &mut Admission) {
        drop(self.rows);
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
        let submitted =
            exchange_bytes(length).and_then(|bytes| dns_debt::submit(admission, id, bytes).ok());
        let (debt, submittable) = match submitted {
            Some(debt) => (debt, true),
            None => {
                let held = answered_here_bytes(length)
                    .and_then(|bytes| dns_debt::hold(admission, id, bytes).ok());
                match held {
                    Some(debt) => (debt, false),
                    None => {
                        self.skipped += 1;
                        return None;
                    }
                }
            }
        };
        self.next = next;
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
        // Commit before the synchronous resolver call; no config update can interleave here.
        let submission = match self.rows.get(&id) {
            Some(pending) => crate::shizuku::resolver::submit(network, &pending.message),
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
            Ok(expected) => {
                self.set(id, Awaiting::Failed(Some(expected)));
                Ok(Submitted::Outstanding(id))
            }
            Err(ending) => {
                if let Some(pending) = self.rows.remove(&id) {
                    pending.drain(admission);
                }
                Err(ending)
            }
        }
    }

    fn set(&mut self, id: u64, awaiting: Awaiting) {
        if let Some(pending) = self.rows.get_mut(&id) {
            pending.awaiting = awaiting;
        }
    }

    pub(crate) fn debt(&self, key: u64) -> Option<&QueryDebt> {
        self.rows.get(&key).map(|pending| &pending.debt)
    }

    pub(crate) fn poll_finished(&mut self, cx: &mut Context<'_>) -> Poll<Settlement> {
        // Poll every row so each resolver registers this task's waker.
        let mut ready = None;
        for (key, pending) in self.rows.iter_mut() {
            if let Poll::Ready(completed) = pending.awaiting.poll(cx) {
                let result = resolved(completed, &pending.message).map(Owned::new);
                ready = Some((*key, result));
                break;
            }
        }
        let Some((key, result)) = ready else {
            return Poll::Pending;
        };
        let Some(pending) = self.rows.remove(&key) else {
            return Poll::Pending;
        };
        Poll::Ready(Settlement {
            key,
            pending,
            result,
        })
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
            awaiting,
            ..
        } = pending;
        // Close our resolver descriptor before releasing the row's admission debt.
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

    pub(crate) fn shutdown(&mut self, admission: &mut Admission) {
        for (_, pending) in self.rows.drain() {
            pending.drain(admission);
        }
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} outstanding transactions, skipped {}",
            self.rows.len(),
            self.skipped
        )
    }
}
