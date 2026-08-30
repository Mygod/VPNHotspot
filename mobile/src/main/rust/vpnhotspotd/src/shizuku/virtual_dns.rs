//! UDP virtual DNS using Android's resolver selection for the app UID.
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use vpnhotspotd::shared::dns_wire::servfail_response;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::udp_wire::Relayed;
use vpnhotspotd::shared::workers::{Ended, Terminal, Workers};

use crate::report;
use crate::shizuku::budget::MAX_DATAGRAM;
use crate::shizuku::output::Output;
use crate::shizuku::owned::Owned;
use crate::shizuku::resolver;
use vpnhotspotd::shared::admission::{Admission, Class, Denied, Lease, Request as Grant};
use vpnhotspotd::shared::dns_debt;
use vpnhotspotd::shared::reply_bound::channel_footprint;

const QUERY_BYTES: u64 = MAX_DATAGRAM as u64;

const ANSWER_BYTES: u64 = MAX_DATAGRAM as u64;

// Keep the row-bound divisor no larger than this exchange's charge.
const _: () = assert!(QUERY_BYTES + ANSWER_BYTES >= dns_debt::MINIMUM_SUBMITTED_BYTES);

const UNROUTED: &str = "shizuku.virtual_dns.unrouted";

const LOCAL_ORIGIN_HOP_LIMIT: u8 = 64;

pub(crate) struct Answer {
    transaction: u64,
    endpoint: SocketAddr,
    client: SocketAddr,
    query: Owned,
    result: Result<Owned, Failure>,
    submitted: Instant,
}

enum Arrival {
    Answer(Answer),
    Ending(io::Error),
}

pub(crate) enum Settled {
    Terminal(Terminal<u64>),
    Ending(io::Error),
}

#[derive(Default)]
struct Counters {
    answered: u64,
    servfail: u64,
    denied: u64,
    discarded: u64,
    unanswerable: u64,
    unsettled: u64,
    slowest: Duration,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "answered {} servfail {} denied {} discarded {} unanswerable {} \
             unsettled {} slowest {:?}",
            self.answered,
            self.servfail,
            self.denied,
            self.discarded,
            self.unanswerable,
            self.unsettled,
            self.slowest
        )
    }
}

pub(crate) struct Handoff {
    answers: mpsc::Sender<Arrival>,
    arrivals: mpsc::Receiver<Arrival>,
    queries: Workers<u64, Debt>,
    tables: Lease,
    counters: Counters,
}

struct Debt {
    lease: Lease,
    answer: Option<Answer>,
}

impl Handoff {
    pub(crate) fn new(admission: &mut Admission) -> Result<Self, Denied> {
        // Size both to the maximum admission can charge, giving every admitted worker a completion slot.
        let prepared = dns_debt::rows(admission);
        let depth = prepared.max(1);
        let bytes = Workers::<u64, Debt>::footprint(prepared)
            .and_then(|table| table.checked_add(channel_footprint::<Arrival>(depth, depth)?))
            .ok_or(Denied::Arithmetic)?;
        let tables = dns_debt::tables(admission, bytes)?;
        let (answers, arrivals) = mpsc::channel(depth);
        debug_assert_eq!(
            answers.max_capacity(),
            depth,
            "the answer queue is charged at the depth it is built at"
        );
        Ok(Self {
            answers,
            arrivals,
            queries: Workers::with_capacity("shizuku.virtual_dns", prepared),
            tables,
            counters: Counters::default(),
        })
    }

    pub(crate) fn release(self, admission: &mut Admission) {
        drop(self.queries);
        drop(self.answers);
        drop(self.arrivals);
        admission.release(self.tables);
    }

    pub(crate) fn submit(
        &mut self,
        datagram: Relayed<'_>,
        output: &mut Output,
        admission: &mut Admission,
    ) -> io::Result<()> {
        if !self.queries.has_room() {
            self.counters.denied += 1;
            self.refuse(datagram, output);
            return Ok(());
        }
        let Ok(lease) = admission.reserve(Grant {
            records: 1,
            record_class: Class::Reserved,
            bytes: QUERY_BYTES + ANSWER_BYTES,
            byte_class: Class::Reserved,
            ..Grant::default()
        }) else {
            self.counters.denied += 1;
            self.refuse(datagram, output);
            return Ok(());
        };
        let answers = self.answers.clone();
        let endpoint = datagram.destination;
        let client = datagram.source;
        let Ok(identity) = self.queries.identity() else {
            admission.release(lease);
            self.counters.denied += 1;
            self.refuse(datagram, output);
            return Ok(());
        };
        let cancel = identity.cancel.clone();
        let transaction = identity.id;
        let submitted = Instant::now();
        let query = Owned::new(datagram.payload.to_vec());
        type Handed = (Result<resolver::Resolving, Failure>, Owned);
        let (handoff, accepted) = tokio::sync::oneshot::channel::<Handed>();
        let admitted = self.queries.admit(
            identity.id,
            &identity,
            Debt {
                lease,
                answer: None,
            },
            async move {
                let Ok((submission, query)) = accepted.await else {
                    return Ended::Expected;
                };
                let result = match submission {
                    Ok(resolving) => tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ended::Expected,
                        completed = resolving.read() => completed,
                    },
                    Err(failure) => Err(failure),
                };
                let outcome = match result.map(Owned::new) {
                    Ok(response) => Ok(Ok(response)),
                    Err(failure) => failure
                        .ending([("client", client), ("endpoint", endpoint)])
                        .map(Err),
                };
                let arrival = match outcome {
                    Ok(result) => Arrival::Answer(Answer {
                        transaction,
                        endpoint,
                        client,
                        query,
                        result,
                        submitted,
                    }),
                    Err(ending) => {
                        drop(query);
                        Arrival::Ending(ending)
                    }
                };
                let undelivered = match answers.try_send(arrival) {
                    Ok(()) => return Ended::Expected,
                    Err(mpsc::error::TrySendError::Closed(arrival)) => arrival,
                    Err(mpsc::error::TrySendError::Full(arrival)) => arrival,
                };
                match undelivered {
                    Arrival::Answer(_) => report::stdout!(
                        "virtual dns answer for {client} arrived after the session ended"
                    ),
                    Arrival::Ending(ending) => report::io_with_details(
                        UNROUTED,
                        ending,
                        [("client", client), ("endpoint", endpoint)],
                    ),
                }
                Ended::Expected
            },
        );
        if let Err((Debt { lease, .. }, _)) = admitted {
            drop(handoff);
            drop(query);
            admission.release(lease);
            self.counters.denied += 1;
            self.refuse(datagram, output);
            return Ok(());
        }
        // Install the debt owner before synchronous submission so no returned descriptor is orphaned.
        let submission = match resolver::submit(&query) {
            Err(failure) => match failure.ending([("client", client), ("endpoint", endpoint)]) {
                Err(ending) => {
                    drop(handoff);
                    drop(query);
                    return Err(ending);
                }
                Ok(failure) => Err(failure),
            },
            accepted => accepted,
        };
        let _ = handoff.send((submission, query));
        Ok(())
    }

    pub(crate) fn settle(
        &mut self,
        terminal: Terminal<u64>,
        output: &mut Output,
        admission: &mut Admission,
    ) -> io::Result<()> {
        let Terminal { key, id, ended } = terminal;
        if let Ended::Failed { context, error } = ended {
            report::io_with_details(context, error, [("query", key)]);
        }
        // Reconcile a queued answer before consuming the matching task terminal; either may arrive first.
        let mut drained = Ok(());
        while let Ok(arrival) = self.arrivals.try_recv() {
            if let Some(ending) = self.park(arrival) {
                drained = report::keep_first(UNROUTED, drained, Err(ending));
            }
        }
        let Some(Debt { lease, answer }) = self.queries.retire(&key, id) else {
            self.counters.unsettled += 1;
            return drained;
        };
        let Some(answer) = answer else {
            admission.release(lease);
            return drained;
        };
        let Answer {
            endpoint,
            client,
            query,
            result,
            ..
        } = answer;
        let answered = match result {
            Ok(response) => {
                self.counters.answered += 1;
                Some(response)
            }
            Err(_) => None,
        };
        let (query, response) = match answered {
            Some(response) => (Some(query), Some(response)),
            None => {
                let servfail = self.servfail(&query);
                drop(query);
                (None, servfail)
            }
        };
        let Some(response) = response else {
            drop(query);
            admission.release(lease);
            return drained;
        };
        let live = (query.as_ref().map_or(0, Owned::capacity) + response.capacity()) as u64;
        let held = match admission.split(
            &lease,
            Grant::bytes(live.min(QUERY_BYTES + ANSWER_BYTES), Class::Reserved),
        ) {
            Ok(delivery) => {
                admission.release(lease);
                delivery
            }
            // The original lease still covers both buffers; report the invariant failure and retain it.
            Err(denied) if denied.leaves_source_intact() => {
                report::message_with_details(
                    UNROUTED,
                    "a settled virtual DNS answer kept the whole grant its delivery split was refused from",
                    "InvalidData",
                    [("query", key.to_string()), ("denied", format!("{denied:?}"))],
                );
                lease
            }
            // Nothing covers these buffers: drop them before releasing the lease and discard only this
            // datagram.
            Err(denied) => {
                report::message_with_details(
                    UNROUTED,
                    "a settled virtual DNS answer has no grant known to cover its buffers",
                    "InvalidData",
                    [
                        ("query", key.to_string()),
                        ("denied", format!("{denied:?}")),
                    ],
                );
                self.counters.discarded += 1;
                drop(response);
                drop(query);
                admission.release(lease);
                return drained;
            }
        };
        output.datagram(endpoint, client, LOCAL_ORIGIN_HOP_LIMIT, &response);
        // Drop every buffer covered by the delivery before returning its lease.
        drop(response);
        drop(query);
        admission.release(held);
        drained
    }

    fn park(&mut self, arrival: Arrival) -> Option<io::Error> {
        let answer = match arrival {
            Arrival::Answer(answer) => answer,
            Arrival::Ending(ending) => return Some(ending),
        };
        self.counters.slowest = self.counters.slowest.max(answer.submitted.elapsed());
        match self.queries.get_mut(&answer.transaction) {
            Some(held) => held.record.answer = Some(answer),
            None => self.counters.discarded += 1,
        }
        None
    }

    pub(crate) async fn settled(&mut self) -> Settled {
        enum Woke {
            Terminal(Terminal<u64>),
            Arrived(Option<Arrival>),
        }
        loop {
            let woke = {
                let queries = &mut self.queries;
                let arrivals = &mut self.arrivals;
                tokio::select! {
                    biased;
                    terminal = queries.finished() => Woke::Terminal(terminal),
                    arrival = arrivals.recv() => Woke::Arrived(arrival),
                }
            };
            match woke {
                Woke::Terminal(terminal) => return Settled::Terminal(terminal),
                Woke::Arrived(Some(arrival)) => {
                    if let Some(ending) = self.park(arrival) {
                        return Settled::Ending(ending);
                    }
                }
                Woke::Arrived(None) => std::future::pending().await,
            }
        }
    }

    pub(crate) async fn shutdown(
        &mut self,
        output: &mut Output,
        admission: &mut Admission,
    ) -> io::Result<()> {
        self.queries.cancel_all();
        let mut ended = Ok(());
        while self.queries.working() {
            let terminal = self.queries.finished().await;
            ended = report::keep_first(UNROUTED, ended, self.settle(terminal, output, admission));
        }
        ended
    }

    fn servfail(&mut self, query: &[u8]) -> Option<Owned> {
        match servfail_response(query) {
            Some(response) => {
                self.counters.servfail += 1;
                Some(Owned::new(response))
            }
            None => {
                self.counters.unanswerable += 1;
                None
            }
        }
    }

    fn refuse(&mut self, datagram: Relayed<'_>, output: &mut Output) {
        let Some(response) = self.servfail(datagram.payload) else {
            return;
        };
        output.datagram(
            datagram.destination,
            datagram.source,
            LOCAL_ORIGIN_HOP_LIMIT,
            &response,
        );
    }

    pub(crate) fn describe(&self) -> String {
        self.counters.describe()
    }
}
