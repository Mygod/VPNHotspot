//! The virtual-DNS handoff: an exact configured endpoint on port 53 that the daemon answers rather than
//! relays.
//!
//! Nothing here owns a selected-network socket, and that is the whole reason this is not part of the UDP
//! relay. The query goes to the platform resolver, which keeps private DNS, caching, and per-network
//! resolver configuration the daemon could not reimplement; the answer comes back on a descriptor. So a
//! handover has no socket of this module's to sweep, and a query submitted just after one still has a live
//! transport for its reply.
//!
//! Three consequences follow, and all of them are deliberate:
//!
//! - **An in-flight query is never cancelled to free capacity.** Cancelling recovers this process's
//!   descriptor and not the resolver's work, and it destroys the completion signal that made the charge
//!   exact. The descriptor is held, the answer awaited, and the slot refunded when the task that held it has
//!   actually completed - never on a deadline, and never because a config changed. The one thing that does
//!   cancel a query is the session itself ending, where there is no capacity left to free and the process is
//!   recovering what it can before it exits.
//! - **An answer resolved on a network that is no longer selected is discarded.** Discarding still refunds,
//!   and it is not silent: the client's transport is intact and the client is still waiting, so it is owed
//!   the one terminal packet a sweep writes - a SERVFAIL, which fits capacity this module already owns.
//! - **This daemon's own wrapper around a transaction failing ends the session.** Everything the platform
//!   answers is one query's own outcome and becomes that query's SERVFAIL. Making the descriptor nonblocking,
//!   registering it, and the readiness registration it is then watched with are not the platform's and not
//!   any one query's: an owner whose wrapper failed cannot wrap the next query either, so the failure leaves
//!   this module rather than being answered here and ends the ingress task - see
//!   [vpnhotspotd::shared::failure::Failure::ending] and [crate::shizuku::app_session], which delivers the
//!   one report it carries.
//!
//!   Exactly one of these travels that way, because one result carries one error. Every further one observed
//!   independently, and any observed with no owner left to hand it to, is routed once as a nonfatal here
//!   instead - never dropped, and never both reported and returned. See [UNROUTED].

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use vpnhotspotd::shared::dns_wire::servfail_response;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::udp_wire::Relayed;
use vpnhotspotd::shared::workers::{Ended, Terminal, Workers};

use crate::report;
use crate::shizuku::budget::MAX_DATAGRAM;
use crate::shizuku::output::Output;
use crate::shizuku::owned::Owned;
use crate::shizuku::resolver;
use crate::shizuku::tun_writer::Stamp;
use vpnhotspotd::shared::admission::{Admission, Class, Denied, Lease, Request as Grant};
use vpnhotspotd::shared::reply_bound::channel_footprint;

/// What one query copy may cost. A DNS message over UDP cannot exceed one datagram, so this is its ceiling
/// rather than an estimate of its usual size.
const QUERY_BYTES: u64 = MAX_DATAGRAM as u64;

/// What one answer may cost, on the same reasoning. Charged before the platform is asked, because a result
/// that arrived and could not be admitted would be an allocation nothing accounted for.
const ANSWER_BYTES: u64 = MAX_DATAGRAM as u64;

/// Names a wrapper failure with no result left to travel out on - one observed beside another already on its
/// way, or one whose owner is already gone - for the nonfatal it becomes instead.
///
/// Only ever applied to an error carrying no report of its own, which nothing on this path produces:
/// [vpnhotspotd::shared::failure::Failure::ending] attaches one where the step failed, and both
/// [report::io_with_details] and [report::keep_first] hand an attached report back unchanged.
const UNROUTED: &str = "shizuku.virtual_dns.unrouted";

/// Newly originated TUN-side packets use an immutable local origin value rather than preserving anything
/// received: a virtual-DNS answer is not relayed traffic, it is this daemon's own packet, and 64 is the
/// conventional origin for one.
const LOCAL_ORIGIN_HOP_LIMIT: u8 = 64;

/// One finished resolver transaction, carrying everything needed to answer without a second lookup.
pub(crate) struct Answer {
    /// Which transaction this belongs to, so the ingress owner can park it on that record rather than acting
    /// on it the moment it arrives.
    transaction: u64,
    /// The retirement the query was submitted under, so an answer that came back over a selection this
    /// session has since left can be told apart from one it can still send. The mismatch is not silence: the
    /// client is still waiting, and it is owed a SERVFAIL.
    stamp: Stamp,
    /// The exact virtual endpoint the client addressed, which the answer is sourced from so that it looks
    /// like a reply from the resolver the client thinks it is talking to.
    endpoint: SocketAddr,
    client: SocketAddr,
    /// Retained because a failure is answered with SERVFAIL built from the question, which is the only way
    /// a client learns to stop waiting. Owned from the copy that was made for it, so what covers it is
    /// visible for every step of its life rather than from wherever a later owner picks it up.
    query: Owned,
    /// Only ever what the platform answered, or what it answered instead. The other half of
    /// [vpnhotspotd::shared::failure::Failure] - this daemon's own wrapper failing - never reaches a record:
    /// the task that observed it sends [Arrival::Ending] rather than an answer, because it is not this
    /// query's outcome and there is nothing to build from it.
    result: Result<Owned, Failure>,
    submitted: Instant,
}

/// What one query task hands its owner when its transaction is over.
///
/// Two, because only one of them is about the query. An answer belongs to the record that owes it and is
/// acted on at that record's own terminal; an ending belongs to the *owner*, and waiting for the join behind
/// it would leave the ingress loop dispatching into a resolver wrapper this daemon already knows is broken.
/// So it travels on this same channel and is returned to the owner the moment it is taken off it, rather than
/// parked; consuming the message is what makes it observed exactly once.
///
/// What that buys is a bound on what happens *after* the observation, not before it. The ingress owner's
/// select is biased, but biased ordering only ranks arms within one poll: a task that sends between two polls
/// can lose to a TUN arm that was already ready, so a datagram may still be dispatched while an ending is in
/// flight. What cannot happen is a DNS submission committed *after* the owner has taken one, because every
/// path that observes one leaves the loop with it - see [crate::shizuku::tun_reader].
enum Arrival {
    /// The transaction's own outcome, to be parked on the record that owes it.
    Answer(Answer),
    /// This daemon's own wrapper around that transaction failed, already carrying the one report it will
    /// produce. The record that sent it stays exactly as it was: its grant still covers what that
    /// transaction owned, and the worker's own terminal releases it when [Handoff::shutdown] joins it.
    Ending(io::Error),
}

/// What the ingress owner woke for.
pub(crate) enum Settled {
    /// One transaction's worker has been joined, so its descriptor is back and its record may be settled.
    Terminal(Terminal<u64>),
    /// An answer arrived carrying this daemon's own wrapper failure. Returned rather than parked, so the
    /// owner ends on it in the same turn it was observed - see [Arrival::Ending].
    Ending(io::Error),
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
    /// Round-trip of the platform resolver itself, which is what makes the concurrency ceiling checkable
    /// rather than merely asserted.
    slowest: Duration,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "answered {} servfail {} denied {} no-upstream {} discarded {} unanswerable {} \
             unsettled {} slowest {:?}",
            self.answered,
            self.servfail,
            self.denied,
            self.no_upstream,
            self.discarded,
            self.unanswerable,
            self.unsettled,
            self.slowest
        )
    }
}

pub(crate) struct Handoff {
    stamp: Stamp,
    /// Only the network. There is no interface check here because there is no socket to check one on: the
    /// resolver owns the transport, so the reply cannot be delivered to a reused local identity.
    upstream: Option<Network>,
    answers: mpsc::Sender<Arrival>,
    /// Held here rather than by the ingress loop, so that settling a terminal can drain whatever this
    /// transaction already sent before deciding it sent nothing - which is what makes the two arrival orders
    /// one order.
    arrivals: mpsc::Receiver<Arrival>,
    /// One per submitted query, holding the transaction's descriptor. Owned rather than detached because only
    /// this task finishing says the descriptor is back and this daemon's own logical token may be reused: the
    /// answer arriving says the *answer* is over. That token is this session's own accounting, sized under
    /// Android's per-UID limit rather than standing for a slot in it - Android's operation ends when its
    /// resolver work returns, which nothing here observes.
    queries: Workers<u64, Debt>,
    /// The transaction table's own capacity and the answer channel's slots, charged once for the session.
    tables: Lease,
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
    /// move. Absent for a transaction whose task sent an [Arrival::Ending] instead, which is what keeps its
    /// grant charged until that task is joined. Delivering on arrival meant the result's bytes were owned by whoever was building a packet from
    /// them while the grant that covered them could be released by a terminal in the same turn of the loop -
    /// two orders of the same two events, one of which released capacity for memory that still existed.
    /// Attached to the retained record, there is only one order: joined, then owned, then delivered.
    answer: Option<Answer>,
}

impl Handoff {
    pub(crate) fn new(admission: &mut Admission) -> Result<Self, Denied> {
        let prepared = admission.dns_token_cap() as usize;
        // One slot per logical token, which is exactly how many arrivals can be in flight: a query that has
        // no token was never submitted, so nothing beyond the cap can ever reach this channel. At least one,
        // because a zero-capacity channel is not constructible - and the charge below is taken at this same
        // depth, so the depth that makes it true is the depth that exists.
        //
        // This is also what lets a query task hand its outcome over without awaiting - see the send in
        // [Handoff::submit]'s worker. The table below is prepared for the same number, a record's slot is
        // freed only after [Handoff::settle] has drained this channel, and each query sends at most one
        // arrival, so a sender can never find every slot taken.
        let depth = prepared.max(1);
        let bytes = Workers::<u64, Debt>::footprint(prepared)
            // The channel for what a channel is, not for the messages in it: its shared state, its value
            // blocks and their headers, and every payload its slots may carry. `depth * size_of(message)`
            // understated it, which is the fail-open direction the aggregate exists to prevent - and it read
            // as though the answers were the only thing being allocated.
            // One sender per query task, because this owner clones it into each - so the grow-race term is
            // one per slot rather than one.
            .and_then(|table| table.checked_add(channel_footprint::<Arrival>(depth, depth)?))
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
            counters: Counters::default(),
        })
    }

    /// Releases the table's own capacity, after every transaction has been settled.
    pub(crate) fn release(self, admission: &mut Admission) {
        drop(self.queries);
        drop(self.answers);
        drop(self.arrivals);
        admission.release(self.tables);
    }

    /// Adopts a config. Nothing is retired and nothing is drained: this module holds no selected-network
    /// socket, so there is nothing a generation invalidates that is not already handled by discarding the
    /// answer when it arrives.
    pub(crate) fn apply(&mut self, stamp: Stamp, upstream: Option<Network>) {
        self.stamp = stamp;
        self.upstream = upstream;
    }

    /// Submits one client query. Denial is answered rather than dropped whenever answering fits capacity
    /// already owned - a SERVFAIL needs a buffer and no descriptor, so it always does.
    ///
    /// `Err` is the one outcome that is not this query's: this daemon's own wrapper around the descriptor
    /// Android returned failed, so the ingress owner that called this ends rather than dispatching another
    /// packet into it. Everything the platform itself answers is `Ok` and reaches the client as SERVFAIL.
    pub(crate) fn submit(
        &mut self,
        datagram: Relayed<'_>,
        output: &mut Output,
        admission: &mut Admission,
    ) -> io::Result<()> {
        let Some(network) = self.upstream else {
            self.counters.no_upstream += 1;
            self.refuse(datagram, output);
            return Ok(());
        };
        if !self.queries.has_room() {
            self.counters.denied += 1;
            self.refuse(datagram, output);
            return Ok(());
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
            self.refuse(datagram, output);
            return Ok(());
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
            self.refuse(datagram, output);
            return Ok(());
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
        // Only what there is to wait on travels, because only the platform's own refusal is still this
        // query's business by then: this daemon's own wrapper failing never enters the task at all, and is
        // returned to the ingress owner below instead.
        type Handed = (Result<resolver::Resolving, Failure>, Owned);
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
                // A submission the platform refused has its answer already and awaits nothing; one it took is
                // awaited to its terminal, and the readiness registration failing there is this daemon's own
                // rather than the platform's - which its terminal reads off the failure's own classification.
                let result = match submission {
                    Ok(resolving) => tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ended::Expected,
                        completed = resolving.read() => completed,
                    },
                    Err(failure) => Err(failure),
                };
                // Split by whose failure it was, here, in the task that observed it: the report then names
                // the step, this client and this endpoint rather than whichever owner picked it up, and what
                // travels is already either this query's business or the owner's - never both. Owned from
                // here too, so the buffer the daemon carries through the terminal, the settle and the
                // packetization is counted for every step rather than from wherever a later owner finds it.
                let outcome = match result.map(Owned::new) {
                    Ok(response) => Ok(Ok(response)),
                    Err(failure) => failure
                        .ending([("client", client), ("endpoint", endpoint)])
                        .map(Err),
                };
                let arrival = match outcome {
                    Ok(result) => Arrival::Answer(Answer {
                        transaction,
                        stamp,
                        endpoint,
                        client,
                        query,
                        result,
                        submitted,
                    }),
                    // Nothing will be built from the query and nothing is delivered for this transaction, so
                    // the copy goes now. What it was charged against stays on this task's record until the
                    // owner joins this task, which is the only thing that says the descriptor is back.
                    Err(ending) => {
                        drop(query);
                        Arrival::Ending(ending)
                    }
                };
                // Handed over without awaiting and without a race. This channel is one slot per logical
                // resolver token, only a query holding one of those tokens ever reaches here, and each such
                // query sends exactly one arrival - so a slot is always free and this cannot block. That is
                // the whole reason it need not be awaited, and awaiting it under a cancellation race is what
                // this replaces: an outcome that already exists would be thrown away on the cancel, and for
                // an ending that is the one report it will ever produce. Cancellation still ends a query that
                // has produced nothing, at the read above, so shutdown still joins promptly.
                let undelivered = match answers.try_send(arrival) {
                    Ok(()) => return Ended::Expected,
                    // The receiver went with the session, so there is no owner left to hand anything to.
                    Err(mpsc::error::TrySendError::Closed(arrival)) => arrival,
                    // Unreachable while the depth is the token cap. Answered rather than asserted, because a
                    // panic here would take the process with it, and answered the same way: whatever it is,
                    // this task is the last owner that can say anything about it.
                    Err(mpsc::error::TrySendError::Full(arrival)) => arrival,
                };
                match undelivered {
                    // Nobody is waiting for it any more, which is not a failure of anything.
                    Arrival::Answer(_) => report::stdout!(
                        "virtual dns answer for {client} arrived after the session ended"
                    ),
                    // The last owner that can say it. No result of this task's reaches the ingress owner, so
                    // the ending has nowhere to travel and becomes its one report here instead of being
                    // silently downgraded to an ordinary cancellation.
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
            self.refuse(datagram, output);
            return Ok(());
        }
        // Committed. Only now does anything leave this process, and it leaves synchronously on the owner -
        // no await and no config application can interleave between the commit above and the call below, so
        // a query cannot first reach the resolver on a `Network` a successor config has already replaced.
        let submission = match resolver::submit(network, &query) {
            // This daemon's own wrapper around the descriptor Android returned. The descriptor is already
            // cancelled and closed by the dropped submission; what must not happen next is another query, so
            // the failure goes back to the ingress owner and ends it *here* rather than travelling into a
            // task and arriving one dispatch later. Nothing is reported here: this failure travels, and the
            // session that ends on it delivers the report it already carries.
            //
            // Dropping the handoff sender is what unwinds the record - the task sees a closed channel, ends
            // expected, and its own terminal releases everything this query owed. The copy goes first, like
            // every other buffer here, so nothing is refunded while it is still alive.
            Err(failure) => match failure.ending([("client", client), ("endpoint", endpoint)]) {
                Err(ending) => {
                    drop(handoff);
                    drop(query);
                    return Err(ending);
                }
                // The platform refused it, which is this query's own outcome: the task turns it into that
                // query's SERVFAIL exactly as it does an answer.
                Ok(failure) => Err(failure),
            },
            accepted => accepted,
        };
        // The task is already admitted and awaiting exactly this; a closed receiver would mean it had ended,
        // which its own terminal settles.
        let _ = handoff.send((submission, query));
        Ok(())
    }

    /// Refunds one query slot, once the task that held its descriptor has actually completed. Separate from
    /// answer delivery on purpose: the answer arriving says the answer is ready, and only the task finishing
    /// says the transaction - and the descriptor it held - is over.
    ///
    /// `Err` is this daemon's own wrapper around a transaction having failed while it was being watched. It
    /// is not this query's outcome and is not answered as one: it ends the ingress owner, exactly as the same
    /// failure at submission does. It need not be *this* transaction's - a terminal ready at the same moment
    /// as such an answer takes this turn, so the drain below is the other place one is observed - and the
    /// settlement in hand still runs to completion either way, because the record it names owes its grant
    /// whichever failure ends the session.
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
        // Whatever this transaction already sent is parked before its record is taken, so the two orders the
        // scheduler may present - answer first, or terminal first - become one. A terminal settled without
        // this would retire a record whose answer was still in the channel, and the answer would then find
        // nothing to belong to. Carried to whichever exit below is taken rather than returned here: every one
        // of them still has accounting to finish first.
        let drained = self.park_arrivals();
        let Some(Debt { lease, answer }) = self.queries.retire(&key, id) else {
            self.counters.unsettled += 1;
            return drained;
        };
        // The task is joined, so this process's descriptor for it is closed, and the logical token it stood
        // for goes back with the rest of this grant. Whether Android's own operation has finished is a
        // different question and not this process's: `android_res_cancel` closed a descriptor, and the
        // platform's limiter releases its slot when its own work returns.
        // What it does still own is the query it copied and the result it received, and the whole of the
        // work below happens under the grant that already covers both of them *plus* a whole answer
        // allowance. Nothing is split off before the decision, because a split sized from the buffers that
        // happen to be live now undercharges every outcome that goes on to build a new one - and a split
        // taken before an obsolete buffer is dropped is capacity refunded for memory this process is holding.
        let Some(answer) = answer else {
            // The transaction was cancelled before it produced anything - or its task sent an
            // [Arrival::Ending] instead, which is what keeps this grant charged until that task is joined.
            // Either way there is nothing to deliver and nothing that outlives this grant.
            admission.release(lease);
            return drained;
        };
        let Answer {
            stamp,
            endpoint,
            client,
            query,
            result,
            ..
        } = answer;
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
            // a flood it drives. It is the only failure that can be here at all - see [Answer::result].
            Err(_) => None,
        };
        // Built while the grant still covers the query *and* a whole answer allowance, which is what makes
        // this allocation one that was charged for before it existed. The query then dies as soon as it has
        // been built from, so what the split below retains is only the bytes still on their way out.
        let (query, response) = match answered {
            Some(response) => (Some(query), Some(response)),
            None => {
                let servfail = self.servfail(&query);
                drop(query);
                (None, servfail)
            }
        };
        let Some(response) = response else {
            // A query too malformed for a SERVFAIL to be formed from: nothing is left allocated after this
            // call, so the whole grant ends here rather than being split for a delivery.
            drop(query);
            admission.release(lease);
            return drained;
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
        drained
    }

    /// Moves everything waiting in the channel onto the record it belongs to.
    ///
    /// Idempotent and cheap: there is at most one arrival per transaction, and a transaction whose record is
    /// already gone has nothing to attach one to.
    ///
    /// `Err` is the first ending among them. A second is routed to a nonfatal rather than lost, because only
    /// one failure can travel out on this result and the alternative to saying so is a silent discard - see
    /// [report::keep_first].
    fn park_arrivals(&mut self) -> io::Result<()> {
        let mut drained = Ok(());
        while let Ok(arrival) = self.arrivals.try_recv() {
            if let Some(ending) = self.park(arrival) {
                drained = report::keep_first(UNROUTED, drained, Err(ending));
            }
        }
        drained
    }

    /// Parks one arrival on the record it belongs to, or hands back the ending it carries instead.
    ///
    /// `Some` is [Arrival::Ending]: nothing is parked and nothing is delivered, the message carrying it is
    /// consumed here - which is what makes it observed exactly once - and the record it came from is left as
    /// it was, so its grant is still charged until [Handoff::shutdown] joins the worker that sent it.
    fn park(&mut self, arrival: Arrival) -> Option<io::Error> {
        let answer = match arrival {
            Arrival::Answer(answer) => answer,
            Arrival::Ending(ending) => return Some(ending),
        };
        self.counters.slowest = self.counters.slowest.max(answer.submitted.elapsed());
        match self.queries.get_mut(&answer.transaction) {
            Some(held) => held.record.answer = Some(answer),
            // Its record is gone, which means its terminal was settled first and this answer arrived after -
            // reachable only if an answer were sent after the task that sent it had been reported and its row
            // taken back, which the join fence rules out: [Workers::finished] reports a task only once tokio
            // has dropped its future, and the send happens before that task returns.
            None => self.counters.discarded += 1,
        }
        None
    }

    /// The next thing the ingress owner has to act on: a transaction that finished, or a wrapper failure that
    /// ends this owner.
    ///
    /// An *answer* is still not an event: it is state that belongs to a record, and the event is the worker
    /// completing. An ending is the opposite - it belongs to no record, and the join behind it is turns of
    /// the ingress loop spent dispatching into a resolver wrapper this daemon already knows is broken - so it
    /// is returned the moment it is taken off the channel rather than waited for. Selected on by the ingress
    /// task, so it waits forever while nothing is in flight rather than answering at once. Cancel-safe,
    /// because both halves are and because nothing is taken off the channel without being either parked or
    /// returned.
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
    ///
    /// `Err` is a wrapper failure whose answer was still in the channel when the session ended: it has no
    /// owner left to end, so it is handed to the caller for the one place that still describes a failure -
    /// the ingress task's own result. The first is kept and every one after it becomes a nonfatal here, so a
    /// drain that turns up several loses none of them and repeats none of them; see [report::keep_first].
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
