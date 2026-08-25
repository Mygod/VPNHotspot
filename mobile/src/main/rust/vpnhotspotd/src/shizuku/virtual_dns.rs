//! The virtual-DNS handoff: an exact configured endpoint on port 53 that the daemon answers rather than
//! relays.
//!
//! Nothing here owns a selected-network socket, and that is the whole reason this is not part of the UDP
//! relay. The query goes to the platform resolver, which keeps private DNS, caching, and per-network
//! resolver configuration the daemon could not reimplement; the answer comes back on a descriptor. So a
//! handover has no socket of this module's to sweep, and a query submitted just after one still has a live
//! transport for its reply.
//!
//! Two consequences follow, and both are deliberate:
//!
//! - **An in-flight query is never cancelled to free capacity.** Cancelling recovers this process's
//!   descriptor and not the resolver's work, and it destroys the completion signal that made the charge
//!   exact. The descriptor is held, the answer awaited, and the slot refunded when the task that held it has
//!   actually completed - never on a deadline, and never because a config changed. The one thing that does
//!   cancel a query is the session itself ending, where there is no capacity left to free and the process is
//!   recovering what it can before it exits.
//! - **An answer is discarded by both axes, not by neither.** The generation because the answer may have
//!   been resolved on a network that is no longer selected, and the epoch because the client address it
//!   would go to may no longer mean the same device. Discarding still refunds.
//! - **The two discards are not equally silent.** A swept generation leaves the client's transport intact and
//!   the client still waiting, so it is owed the one terminal packet a sweep writes - a SERVFAIL, which fits
//!   capacity this module already owns. A retired epoch is owed nothing, because the address that answer
//!   would go to may now be a different device.
//! - **A query the platform took and this process cannot watch keeps its logical token.** `android_res_nsend`
//!   is irreversible: once it has answered with a descriptor, one of this UID's resolver slots is taken
//!   whatever happens in this process. If the two local steps after it fail, there is nothing left to observe
//!   that slot's end with - so the token is moved into a session-owned quarantine rather than refunded, and
//!   the query's own record and bytes go back as usual. See [vpnhotspotd::shared::dns_debt::Quarantine].

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use crate::shizuku::workers::{Ended, Terminal, Workers};
use tokio::sync::mpsc;
use vpnhotspotd::shared::dns_wire::servfail_response;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::udp_wire::Relayed;

use crate::report;
use crate::shizuku::budget::MAX_DATAGRAM;
use crate::shizuku::output::Output;
use crate::shizuku::owned::Owned;
use crate::shizuku::resolver;
use crate::shizuku::tun_writer::Stamp;
use vpnhotspotd::shared::admission::{Admission, Class, Denied, Lease, Request as Grant};
use vpnhotspotd::shared::dns_debt::Quarantine;
use vpnhotspotd::shared::reply_bound::channel_footprint;

/// What one query copy may cost. A DNS message over UDP cannot exceed one datagram, so this is its ceiling
/// rather than an estimate of its usual size.
const QUERY_BYTES: u64 = MAX_DATAGRAM as u64;

/// What one answer may cost, on the same reasoning. Charged before the platform is asked, because a result
/// that arrived and could not be admitted would be an allocation nothing accounted for.
const ANSWER_BYTES: u64 = MAX_DATAGRAM as u64;

/// Newly originated TUN-side packets use an immutable local origin value rather than preserving anything
/// received: a virtual-DNS answer is not relayed traffic, it is this daemon's own packet, and 64 is the
/// conventional origin for one.
const LOCAL_ORIGIN_HOP_LIMIT: u8 = 64;

/// One finished resolver transaction, carrying everything needed to answer without a second lookup.
pub(crate) struct Answer {
    /// Which transaction this belongs to, so the ingress owner can park it on that record rather than acting
    /// on it the moment it arrives.
    transaction: u64,
    /// The retirement the query was submitted under. Both axes are read separately here, because they differ
    /// in what a mismatch costs: a swept generation still owes the client a SERVFAIL, a retired epoch owes it
    /// nothing.
    stamp: Stamp,
    /// The exact virtual endpoint the client addressed, which the answer is sourced from so that it looks
    /// like a reply from the resolver the client thinks it is talking to.
    endpoint: SocketAddr,
    client: SocketAddr,
    /// Retained because a failure is answered with SERVFAIL built from the question, which is the only way
    /// a client learns to stop waiting. Owned from the copy that was made for it, so what covers it is
    /// visible for every step of its life rather than from wherever a later owner picks it up.
    query: Owned,
    /// Classified, because what the app is told depends on which side failed - see
    /// [vpnhotspotd::shared::failure].
    result: Result<Owned, Failure>,
    /// Whether this process lost the ability to watch a question the platform had already accepted, so the
    /// logical token this query holds may never be reused. Read at the terminal, where the grant holding that
    /// token is still in hand.
    unobservable: bool,
    submitted: Instant,
}

#[derive(Default)]
struct Counters {
    answered: u64,
    servfail: u64,
    denied: u64,
    no_upstream: u64,
    discarded: u64,
    unanswerable: u64,
    /// A terminal for a transaction this handoff no longer holds, which cannot happen while ids are unique
    /// and is counted rather than assumed away.
    unsettled: u64,
    /// Submissions the platform accepted and this process cannot observe. Each is one logical token that is
    /// gone for the rest of the session.
    unobservable: u64,
    /// A token that could not be moved into the quarantine, which would be capacity this session goes on
    /// believing it has. Counted rather than assumed away.
    unquarantined: u64,
    /// Round-trip of the platform resolver itself, which is what makes the concurrency ceiling checkable
    /// rather than merely asserted.
    slowest: Duration,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "answered {} servfail {} denied {} no-upstream {} discarded {} unanswerable {} \
             unsettled {} unobservable {} unquarantined {} slowest {:?}",
            self.answered,
            self.servfail,
            self.denied,
            self.no_upstream,
            self.discarded,
            self.unanswerable,
            self.unsettled,
            self.unobservable,
            self.unquarantined,
            self.slowest
        )
    }
}

pub(crate) struct Handoff {
    stamp: Stamp,
    /// Only the network. There is no interface check here because there is no socket to check one on: the
    /// resolver owns the transport, so the reply cannot be delivered to a reused local identity.
    upstream: Option<Network>,
    answers: mpsc::Sender<Answer>,
    /// Held here rather than by the ingress loop, so that settling a terminal can drain whatever this
    /// transaction already sent before deciding it sent nothing - which is what makes the two arrival orders
    /// one order.
    arrivals: mpsc::Receiver<Answer>,
    /// One per submitted query, holding the transaction's descriptor. Owned rather than detached because the
    /// query slot is the platform limiter's, and only this task finishing says the transaction is over: the
    /// answer arriving says the *answer* is over.
    queries: Workers<u64, Debt>,
    /// The transaction table's own capacity, the answer channel's slots and the quarantine's, charged once
    /// for the session.
    tables: Lease,
    /// Logical tokens the platform took and this process can no longer watch. Bounded by the same cap and
    /// preallocated to it, because only a query that already held a token can put one here.
    quarantined: Quarantine,
    counters: Counters,
}

/// The last owner of one answer on its way to a client: the grant that covers it, and the buffers it covers.
///
/// The lease is *inside*, and [Delivering::sent] is the only way out - which consumes both buffers before it
/// yields anything. That makes the terminal order unspellable the wrong way round rather than merely
/// unwritten: an owner cannot release a grant while the response it pays for is still alive, because it
/// cannot reach the grant without destroying that response first.
///
/// A balance cannot tell the two orders apart - the lease is released exactly once either way and the buffers
/// are dropped exactly once either way - so the type is what carries the property.
struct Delivering {
    held: Lease,
    response: Owned,
    /// Absent when this answer is a SERVFAIL, which was built from the query and outlived it.
    query: Option<Owned>,
}

/// One lease whose buffers are provably gone.
struct Releasable {
    lease: Lease,
}

impl Delivering {
    /// Writes the answer, destroys both buffers, and only then yields the lease.
    ///
    /// The write is a closure rather than an argument because the owner's output borrows differ from this
    /// one's, and because what matters is that it happens *inside*: the answer is written, dropped, and the
    /// grant surfaces afterwards.
    fn sent(self, write: impl FnOnce(&Owned)) -> Releasable {
        let Self {
            held,
            response,
            query,
        } = self;
        // [Output] has taken its own copy by the time this returns, so these are the last owners of these
        // bytes.
        write(&response);
        drop(response);
        drop(query);
        Releasable { lease: held }
    }
}

/// What one submitted query owes: a DNS-class descriptor record, one logical resolver token, and the bytes of
/// the query it copied plus the answer the platform will hand back.
///
/// Reserved before the task is spawned and released only when that task has been joined - not when the answer
/// arrived. The answer arriving says the *answer* is over; the task completing says the descriptor is back.
struct Debt {
    lease: Lease,
    /// The completion, parked here until this transaction's worker has actually been joined.
    ///
    /// Parked rather than delivered on arrival, and that is the whole of the fix. The answer arriving says
    /// the *answer* is ready; only the task completing says the descriptor is back and the accounting may
    /// move. Delivering on arrival meant the result's bytes were owned by whoever was building a packet from
    /// them while the grant that covered them could be released by a terminal in the same turn of the loop -
    /// two orders of the same two events, one of which released capacity for memory that still existed.
    /// Attached to the retained record, there is only one order: joined, then owned, then delivered.
    answer: Option<Answer>,
}

impl Handoff {
    pub(crate) fn new(admission: &mut Admission) -> Result<Self, Denied> {
        let prepared = admission.dns_token_cap() as usize;
        // One slot per logical token, which is exactly how many answers can be in flight: a query that has no
        // token was never submitted, so nothing beyond the cap can ever reach this channel. At least one,
        // because a zero-capacity channel is not constructible - and the charge below is taken at this same
        // depth, so the depth that makes it true is the depth that exists.
        let depth = prepared.max(1);
        // The quarantine adds nothing to this: it allocates nothing at all, holding its tokens on the very
        // grant this figure covers - see [Quarantine].
        let bytes = Workers::<u64, Debt>::footprint(prepared)
            // The channel for what a channel is, not for the messages in it: its shared state, its value
            // blocks and their headers, and every payload its slots may carry. `depth * size_of(message)`
            // understated it, which is the fail-open direction the aggregate exists to prevent - and it read
            // as though the answers were the only thing being allocated.
            // One sender per query task, because this owner clones it into each - so the grow-race term is
            // one per slot rather than one.
            .and_then(|table| table.checked_add(channel_footprint::<Answer>(depth, depth)?))
            .ok_or(Denied::Arithmetic)?;
        // Reserved-class: the accounting for name resolution is not optional work the relay may degrade.
        let tables = admission.reserve(Grant::bytes(bytes, Class::Reserved))?;
        let (answers, arrivals) = mpsc::channel(depth);
        // The charge above and the channel here have to be the same depth or the bound says nothing. Asserted
        // rather than trusted, on the production path, because the two are written a few lines apart.
        debug_assert_eq!(
            answers.max_capacity(),
            depth,
            "the answer queue is charged at the depth it is built at"
        );
        Ok(Self {
            stamp: Stamp::default(),
            upstream: None,
            answers,
            arrivals,
            queries: Workers::with_capacity("shizuku.virtual_dns", prepared),
            tables,
            quarantined: Quarantine::default(),
            counters: Counters::default(),
        })
    }

    /// Releases the table's own capacity, after every transaction has been settled - and with it every logical
    /// token this session had to quarantine, because those were moved onto this very grant.
    ///
    /// The tokens are the one thing released *because the process is ending* rather than because the work they
    /// stood for finished: Android's slot is its own, and nothing in this process can observe or wait for it.
    pub(crate) fn release(self, admission: &mut Admission) {
        drop(self.queries);
        drop(self.answers);
        drop(self.arrivals);
        admission.release(self.tables);
    }

    /// Adopts a config. Nothing is retired and nothing is drained: this module holds no TUN-visible state
    /// and no selected-network socket, so there is nothing an epoch or a generation invalidates that is not
    /// already handled by discarding the answer when it arrives.
    pub(crate) fn apply(&mut self, stamp: Stamp, upstream: Option<Network>) {
        self.stamp = stamp;
        self.upstream = upstream;
    }

    /// Submits one client query. Denial is answered rather than dropped whenever answering fits capacity
    /// already owned - a SERVFAIL needs a buffer and no descriptor, so it always does.
    pub(crate) fn submit(
        &mut self,
        datagram: Relayed<'_>,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        let Some(network) = self.upstream else {
            self.counters.no_upstream += 1;
            return self.refuse(datagram, output);
        };
        if !self.queries.has_room() {
            self.counters.denied += 1;
            return self.refuse(datagram, output);
        }
        // One logical token, one DNS-class descriptor record, and the bytes this transaction will own: the
        // copy of the query it takes below, and the answer the platform hands back. Both are charged before
        // the copy is made, because a query copied and then refused is an allocation nothing admitted.
        //
        // Reserved-class throughout. This is the floor inside the aggregate that only name resolution may
        // enter, which is what keeps a flood of forged sources from crowding DNS out.
        let Ok(lease) = admission.reserve(Grant {
            records: 1,
            record_class: Class::Reserved,
            bytes: QUERY_BYTES + ANSWER_BYTES,
            byte_class: Class::Reserved,
            dns_tokens: 1,
            ..Grant::default()
        }) else {
            self.counters.denied += 1;
            return self.refuse(datagram, output);
        };
        let answers = self.answers.clone();
        let stamp = self.stamp;
        let endpoint = datagram.destination;
        let client = datagram.source;
        // Before the task and before the query is copied: an identity that cannot be issued is a
        // transaction that must not start.
        let Ok(identity) = self.queries.identity() else {
            admission.release(lease);
            self.counters.denied += 1;
            return self.refuse(datagram, output);
        };
        let cancel = identity.cancel.clone();
        let transaction = identity.id;
        let submitted = Instant::now();
        // Copied only now, which is what the preflight above is for: an identity that cannot be issued is a
        // transaction that must not start, and copying before that point would allocate a query for one that
        // never does. The bytes are already covered - the grant above reserved them - so this is the copy the
        // reservation was taken for rather than one beside it.
        let query = Owned::new(datagram.payload.to_vec());
        // The submission travels *into* the task rather than being captured by it, which is what lets the
        // record be committed first. See below: nothing external happens until this table has irrevocably
        // taken the transaction, so a refused admission is a refusal the platform never heard about.
        //
        // The whole [resolver::Submission] travels, not a `Result` distilled from it. Collapsing the three
        // outcomes into one `Err` here is exactly how a submission the platform *refused* came out the other
        // side looking like one it had accepted and this process could not watch - and so had its logical
        // token quarantined when nothing of Android's was ever held. The distinction is an ownership, so it
        // stays typed until the owner that acts on it has read it.
        type Handed = (resolver::Submission, Owned);
        // The one oneshot this owner has, and it is the named submission handoff. One per query record, taken
        // after the grant and the identity above, and gone with the record; its shared cell is count-bounded
        // rather than byte-charged, like the task and the token beside it.
        let (handoff, accepted) = tokio::sync::oneshot::channel::<Handed>();
        let admitted = self.queries.admit(
            identity.id,
            &identity,
            Debt {
                lease,
                answer: None,
            },
            async move {
                // Closed without a value means admission unwound before it submitted anything, so there is no
                // transaction and nothing to settle.
                let Ok((submission, query)) = accepted.await else {
                    return Ended::Expected;
                };
                // Raced only against the session ending. A config change does not cancel this - see the module
                // note - so the transaction runs to completion whenever there is still a session to answer into,
                // and the descriptor it holds is released here rather than by a deadline. What is awaited is
                // only the *result*; the submission is already behind us. A submission that produced no
                // transaction has its answer already and awaits nothing.
                //
                // Each outcome keeps its own ownership across this boundary. A query the platform never
                // received is an ordinary expected failure whose token goes back with the rest of its grant;
                // one it received and this process cannot watch keeps that token, whether the watching was
                // lost here or was never possible in the first place.
                let completed = match submission {
                    resolver::Submission::Accepted(resolving) => tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ended::Expected,
                        completed = resolving.read() => completed,
                    },
                    resolver::Submission::NeverReached(failure) => {
                        resolver::Completed::Answered(Err(failure))
                    }
                    resolver::Submission::Unobservable(failure) => {
                        resolver::Completed::Unobservable(failure)
                    }
                };
                // Whether this process lost the ability to watch a question the platform had accepted, which
                // its terminal reads to decide whether the logical token may ever be reused. Carried beside
                // the answer rather than folded into it, because a failure says which step went wrong while
                // this says who still owns a resolver slot.
                let unobservable = completed.unobservable();
                // Owned from here: this is the buffer the daemon carries through the terminal, the settle and
                // the packetization, and it is counted for every step of that rather than from wherever a
                // later owner happens to pick it up.
                let result = completed.answer().map(Owned::new);
                let answer = Answer {
                    transaction,
                    stamp,
                    endpoint,
                    client,
                    query,
                    result,
                    unobservable,
                    submitted,
                };
                // The receiver is dropped only when the session is over, at which point there is no client left
                // to answer and no accounting left to settle.
                let handed = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Ended::Expected,
                    sent = answers.send(answer) => sent,
                };
                if handed.is_err() {
                    report::stdout!(
                        "virtual dns answer for {client} arrived after the session ended"
                    );
                }
                Ended::Expected
            },
        );
        if let Err((Debt { lease, .. }, _)) = admitted {
            // The record could not be committed, so the platform is never asked: `handoff` is dropped here
            // and the task - if one exists at all - sees a closed channel. Everything charged goes back, and
            // the client is refused rather than left waiting on a question nobody took.
            drop(handoff);
            // The copy this registration made goes before the grant that covered it, like every other buffer
            // on this path: a release while it is still alive is capacity given back for memory this process
            // is holding.
            drop(query);
            admission.release(lease);
            self.counters.denied += 1;
            return self.refuse(datagram, output);
        }
        // Committed. Only now does anything leave this process, and it leaves synchronously on the owner -
        // no await and no config application can interleave between the commit above and the call below, so
        // a query cannot first reach the resolver on a `Network` a successor config has already replaced.
        let submission = resolver::submit(network, &query);
        // Not reported here, and that is deliberate: an accepted question can stop being observable at this
        // call or much later while it is being watched, and only one boundary sees both - the terminal, where
        // the token is moved and the outcome is settled. Reporting the immediate case here as well made the
        // same failure two reports from two sites. See [Handoff::settle].
        // The task is already admitted and awaiting exactly this; a closed receiver would mean it had ended,
        // which its own terminal settles.
        let _ = handoff.send((submission, query));
    }

    /// Moves one query's logical token out of its own grant and onto this handoff's own, for the session.
    ///
    /// For a UDP query the token is on that query's own grant, so this is the grant really holding it - unlike
    /// DNS-over-TCP, where it belongs to the transport and two owners can be the one holding it. There is
    /// exactly one terminal per query and the grant is released there, so this is the single place the move
    /// can happen and it needs no guard against happening twice.
    ///
    /// `false` is the move not happening, which its caller must not release the source grant after: that grant
    /// is then the only thing still accounting for a resolver slot Android is holding, and releasing it would
    /// hand the slot back. The same fail-closed answer DNS-over-TCP gives through
    /// [vpnhotspotd::shared::dns_debt::Stranded], reached here without a second type because this owner
    /// already has the grant in hand.
    fn strand(&mut self, lease: &Lease, admission: &mut Admission) -> bool {
        // Destructured so the quarantine and the grant it moves onto can be borrowed at once: they are
        // disjoint fields, which a `&mut self` helper would hide.
        let Self {
            quarantined,
            counters,
            tables,
            ..
        } = self;
        match quarantined.take(admission, lease, tables) {
            Ok(()) => {
                counters.unobservable += 1;
                true
            }
            Err(_) => {
                counters.unquarantined += 1;
                false
            }
        }
    }

    /// Refunds one query slot, once the task that held its descriptor has actually completed. Separate from
    /// answer delivery on purpose: the answer arriving says the answer is ready, and only the task finishing
    /// says the transaction - and the descriptor it held - is over.
    pub(crate) fn settle(
        &mut self,
        terminal: Terminal<u64>,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        let Terminal { key, id, ended } = terminal;
        if let Ended::Failed { context, error } = ended {
            report::io_with_details(context, error, [("query", key)]);
        }
        // Whatever this transaction already sent is parked before its record is taken, so the two orders the
        // scheduler may present - answer first, or terminal first - become one. A terminal settled without
        // this would retire a record whose answer was still in the channel, and the answer would then find
        // nothing to belong to.
        self.park_arrivals();
        let Some(Debt { lease, answer }) = self.queries.retire(&key, id) else {
            self.counters.unsettled += 1;
            return;
        };
        // The task is joined, so this process's descriptor for it is closed. Whether the *platform's* slot is
        // over is a different question and was answered above: an observable transaction ends its slot here,
        // and an unobservable one does not - which is why its token was moved into the quarantine rather than
        // released with the rest of this grant.
        // What it does still own is the query it copied and the result it received, and the whole of the
        // work below happens under the grant that already covers both of them *plus* a whole answer
        // allowance. Nothing is split off before the decision, because a split sized from the buffers that
        // happen to be live now undercharges every outcome that goes on to build a new one - and a split
        // taken before an obsolete buffer is dropped is capacity refunded for memory this process is holding.
        let Some(answer) = answer else {
            // The transaction was cancelled before it produced anything, so there is nothing to deliver and
            // nothing that outlives this grant.
            //
            // A logical token this query may have been holding at risk goes back here rather than into the
            // quarantine, and that is not a hole: the only thing that cancels one of these is the session
            // ending, and the aggregate itself is dropped immediately afterwards - so there is no window in
            // which a refunded token could admit a second query.
            admission.release(lease);
            return;
        };
        let Answer {
            stamp,
            endpoint,
            client,
            query,
            result,
            unobservable,
            ..
        } = answer;
        // The one report for this outcome, before every early return below: whether the token could be
        // quarantined and whether there is still a client to answer are separate questions, and neither
        // changes the fact that the platform is holding a slot this process cannot watch.
        // The one place a UDP query's loss is reported, because there is exactly one terminal per query and
        // it always runs - see [crate::shizuku::resolver::report_unobservable] for why the two protocols choose their
        // reporting owner differently.
        if unobservable {
            if let Err(failure) = &result {
                resolver::report_unobservable(key, failure);
            }
        }
        // Before every release path below, because each of them gives this grant - and the logical token on
        // it - back. The platform is holding a resolver slot whose end this process stopped being able to
        // watch, so that token is moved out first and only the rest is refunded. A query the platform never
        // received reaches none of this: nothing of Android's is held for it, so its token goes back with
        // everything else.
        if unobservable && !self.strand(&lease, admission) {
            // The move did not happen, so this grant is the only thing still accounting for a slot Android is
            // holding and it may not be released. Kept whole - which surfaces as an outstanding lease in the
            // session's exit report rather than as capacity handed back - and both buffers go, because
            // nothing is going to deliver them. Reaching here means the aggregate has contradicted itself
            // about a grant this owner is holding, so there is no answer worth building on top of it.
            drop(result);
            drop(query);
            return;
        }
        // Epoch first: an epoch change may have put a different device behind the tuple this would be
        // addressed to, so there is nobody an answer *or* a failure about it can honestly be sent to. Both
        // buffers die here, under the grant that covered them, and nothing is split for a delivery that will
        // never happen.
        if stamp.epoch != self.stamp.epoch {
            self.counters.discarded += 1;
            drop(result);
            drop(query);
            admission.release(lease);
            return;
        }
        // Whether the platform's own answer is still what goes out. A generation change leaves the client's
        // transport untouched, so it is owed the courtesy of being told to stop rather than left to time out.
        let answered = match result {
            Ok(response) if stamp.generation == self.stamp.generation => {
                self.counters.answered += 1;
                Some(response)
            }
            // Swept: resolved on a network that is no longer selected, so the answer itself is discarded and
            // this query's own SERVFAIL is built below instead. Dropped here and explicitly, *before* the
            // replacement exists: the two never have to be alive at once, and keeping the obsolete one across
            // the build is a peak nothing chose to pay for.
            Ok(stale) => {
                self.counters.discarded += 1;
                drop(stale);
                None
            }
            // The resolver refusing - including the transient -EBUSY of its own per-UID limiter - is a
            // SERVFAIL to the client rather than silence, so it stops waiting and retries. Nothing is
            // reported for it: a client chooses how many queries it sends, so a report per refusal would be
            // a flood it drives.
            Err(Failure::Expected(_)) => None,
            // The daemon's own wrapper around the transaction failing is not the client's doing, so this one
            // is reported - and the client still gets its SERVFAIL. Unless it is the outcome already reported
            // above, which is the same failure and would be the same report twice.
            Err(Failure::Local { context, error }) => {
                if !unobservable {
                    report::io_with_details(
                        context,
                        error,
                        [
                            ("client", client.to_string()),
                            ("endpoint", endpoint.to_string()),
                        ],
                    );
                }
                None
            }
        };
        // Built while the grant still covers the query *and* a whole answer allowance, which is what makes
        // this allocation one that was charged for before it existed. The query then dies as soon as it has
        // been built from, so what the split below retains is only what is physically on its way out.
        let (query, response) = match answered {
            Some(response) => (Some(query), Some(response)),
            None => {
                let servfail = self.servfail(&query);
                drop(query);
                (None, servfail)
            }
        };
        let Some(response) = response else {
            // A query too malformed for a SERVFAIL, or an epoch-silent discard's sibling: nothing physically
            // survives this call, so the whole grant ends here rather than being split for a delivery.
            drop(query);
            admission.release(lease);
            return;
        };
        // Reconciled downward to what is really on its way to the client: the reservation was a conservative
        // maximum, and each buffer is adopted at the capacity it was counted at rather than charged again.
        let live = (query.as_ref().map_or(0, Owned::capacity) + response.capacity()) as u64;
        // Whichever grant now covers exactly what is alive. A split that finds no ledger row leaves the bytes
        // on the original, which is then released after the delivery rather than before it - correct, just
        // less precise.
        let held = match admission.split(
            &lease,
            Grant::bytes(live.min(QUERY_BYTES + ANSWER_BYTES), Class::Reserved),
        ) {
            Ok(delivery) => {
                admission.release(lease);
                delivery
            }
            Err(_) => lease,
        };
        // The lease goes *inside* the buffers it covers, and the only way back out of [Delivering] is to
        // destroy them. That is what makes the terminal order structural here rather than a comment: this
        // owner cannot reach the grant without first having written the answer and dropped both buffers.
        let stamp = self.stamp;
        let releasable = Delivering {
            held,
            response,
            query,
        }
        .sent(|response| {
            output.datagram(stamp, endpoint, client, LOCAL_ORIGIN_HOP_LIMIT, response);
        });
        admission.release(releasable.lease);
    }

    /// Moves every answer waiting in the channel onto the record it belongs to.
    ///
    /// Idempotent and cheap: there is at most one answer per transaction, and a transaction whose record is
    /// already gone has nothing to attach one to.
    fn park_arrivals(&mut self) {
        while let Ok(answer) = self.arrivals.try_recv() {
            self.park(answer);
        }
    }

    /// Parks one answer on the record it belongs to.
    fn park(&mut self, answer: Answer) {
        self.counters.slowest = self.counters.slowest.max(answer.submitted.elapsed());
        match self.queries.get_mut(&answer.transaction) {
            Some(held) => held.record.answer = Some(answer),
            // Its record is gone, which means its terminal was settled first and this answer arrived after -
            // only reachable if a worker outlived its own terminal, which the join fence forbids.
            None => self.counters.discarded += 1,
        }
    }

    /// The next transaction to have finished, parking any answers that arrive while waiting.
    ///
    /// One arm rather than two, because an answer is not an event the ingress owner acts on: it is state that
    /// belongs to a record, and the only event is the worker completing. Selected on by the ingress task, so
    /// it waits forever while nothing is in flight rather than answering at once. Cancel-safe, because both
    /// halves are.
    pub(crate) async fn settled(&mut self) -> Terminal<u64> {
        enum Woke {
            Terminal(Terminal<u64>),
            Arrived(Option<Answer>),
        }
        loop {
            let woke = {
                let queries = &mut self.queries;
                let arrivals = &mut self.arrivals;
                tokio::select! {
                    biased;
                    terminal = queries.finished() => Woke::Terminal(terminal),
                    answer = arrivals.recv() => Woke::Arrived(answer),
                }
            };
            match woke {
                Woke::Terminal(terminal) => return terminal,
                Woke::Arrived(Some(answer)) => self.park(answer),
                // impossible while this handoff holds a sender
                Woke::Arrived(None) => std::future::pending().await,
            }
        }
    }

    /// Cancels every outstanding transaction and joins its task, so no query outlives the session that made
    /// it and every descriptor is back before the process exits. The answers themselves are abandoned: there
    /// is no client left to send one to.
    ///
    /// This recovers what the process owns and nothing more. The resolver's own work behind `dnsproxyd`
    /// belongs to Android once submitted, so neither this nor process death ends it, and nothing here claims
    /// otherwise.
    pub(crate) async fn shutdown(&mut self, output: &mut Output, admission: &mut Admission) {
        self.queries.cancel_all();
        while self.queries.working() {
            let terminal = self.queries.finished().await;
            self.settle(terminal, output, admission);
        }
    }

    /// This query's own SERVFAIL, or nothing when none can be built from it.
    ///
    /// A query malformed enough that no SERVFAIL can be built from it gets no answer, because there is no
    /// question to echo back and a header-only response would be a different message.
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

    /// Answers one query this handoff never submitted, straight from the datagram it arrived in.
    ///
    /// Nothing is split or reserved for this one: the answer is written and dropped inside this call, so
    /// unlike a delivery whose client may take as long as it likes to read it, it has no life to charge for.
    fn refuse(&mut self, datagram: Relayed<'_>, output: &mut Output) {
        let Some(response) = self.servfail(datagram.payload) else {
            return;
        };
        output.datagram(
            self.stamp,
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
