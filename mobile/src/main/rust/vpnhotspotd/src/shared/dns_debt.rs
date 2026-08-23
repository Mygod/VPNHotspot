//! Who owns what across a DNS-over-TCP connection and the queries it submits.
//!
//! Three transitions, and each of them was got wrong at least once, which is why they are here rather than
//! spread across the engine and the transaction table:
//!
//! - **opening** a connection owes its flow buffers and exactly one logical resolver token. Not an exchange's
//!   worth of bytes: an idle connection has no query, no answer and nothing to frame, and charging for them
//!   anyway is a debt for work that has not happened, taken from the floor that exists so real resolver work
//!   is never crowded out.
//! - **submitting** a query owes one DNS-class descriptor record and every byte that submission will own -
//!   and *no second token*, because the connection is already holding one. A token per query turns thirty-two
//!   token-holding connections into sixteen with a query each, which is an artifact of the accounting rather
//!   than a limit anyone chose. Owed when the client's length prefix has been framed and *before* the message
//!   is stored anywhere, which is why the figure is the announced length rather than the largest one there
//!   is: a query copied first and admitted afterwards is an allocation nothing agreed to.
//! - **closing** a connection releases the connection's own grant, and nothing else. If a question is still
//!   in flight the token moves to that question's debt in one operation rather than being released and
//!   re-reserved, because the platform's slot is still taken: a moment where the token looked free is a
//!   moment a second query could be admitted against a limiter with no room for it. The query's bytes are not
//!   touched at all - the resolver task still holds the query and will still return an answer.
//! - **answering here** owes the query and the answer built from it, and nothing else. There is no descriptor
//!   and no platform transaction, so no record and no token - but both buffers are real, with the same
//!   unbounded life as any other, because the client may take as long as it likes to read the answer. This is
//!   the second tier a framed query is offered when the first is denied, and the tier order is what makes a
//!   refusal an answer rather than silence: a query the descriptor floor has no room for can still be
//!   answered, and only one whose *bytes* do not fit is dropped. See [hold].
//!
//! Nothing here is async and nothing here spawns. The tasks, the sockets and the framing belong to the
//! engine; what belongs here is which grant owns which bytes at each of those moments, which is exactly the
//! part that can be checked.

use crate::shared::admission::{Admission, Class, Denied, Lease, Request};

/// The logical resolver tokens one owner has had to give up on, until its session ends.
///
/// A token reaches here when `android_res_nsend` succeeded and this process then lost the ability to observe
/// that transaction - the descriptor could not be made nonblocking or registered, the readiness registration
/// it was being watched with failed, or a closing transport could not hand it to the question still in
/// flight. Android is holding one of this UID's resolver slots in every one of those cases and nothing here
/// can observe its end. Refunding such a token would let a second query be admitted against a limiter that
/// has no room for it, and cancelling recovers nothing of Android's - so the honest thing is to stop counting
/// that slot as available for the rest of the session, and to say so.
///
/// # Why this holds no grant of its own
///
/// The token is *moved onto a grant its owner already holds* - the retained-table lease that owner keeps for
/// its whole session - rather than split into a lease per token. The ledger is derived as one row per
/// record-backed owner, plus the statically known byte-only owners, plus **one** spare for the single
/// owner-confined split or replacement in flight (see `Admission::ledger_slots`). A row per quarantined
/// token would therefore consume rows the derivation never budgeted, and the first quarantine that found
/// none would be refused - handing a token back while Android still holds its slot, which is the exact
/// fail-open this type exists to prevent. A move onto an existing row needs no slot at all, so it cannot be
/// refused for capacity; and the owner releasing that lease at the end of its session releases these with
/// it, exactly once, without a second release path to get wrong.
#[derive(Debug, Default)]
pub struct Quarantine {
    held: u32,
}

/// A logical token whose ownership could not be represented at all.
///
/// Not a capacity denial. [Quarantine::take] moves onto a row that already exists, so the only ways to get
/// here are a grant that was not holding the token it was asked for and a ledger that has lost one of the
/// two rows - both of which are the accounting contradicting itself rather than a session running out of
/// anything. There is no honest local recovery: the alternatives are handing the token back while Android
/// holds its slot, or keeping a grant nothing will ever release, so the caller says which of those it
/// prefers and reports it either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unrepresentable;

impl Quarantine {
    /// Moves one logical token out of the grant that was holding it and onto `onto`, for the session.
    ///
    /// A move rather than a release and a reserve: the platform's slot is taken either way, and a moment
    /// where the token looked free is a moment a second query could be admitted against a limiter with no
    /// room for it. `onto` must be the same session-lived grant every time, so that releasing it once
    /// releases every token this ever took.
    pub fn take(
        &mut self,
        admission: &mut Admission,
        from: &Lease,
        onto: &Lease,
    ) -> Result<(), Unrepresentable> {
        let moved = Request {
            dns_tokens: 1,
            ..Request::default()
        };
        if admission.transfer(from, onto, moved).is_err() {
            return Err(Unrepresentable);
        }
        // Saturating rather than checked: this is a count for a report, and a `u32` of them cannot be
        // reached by any session whose token cap is what bounds how many can ever be at risk.
        self.held = self.held.saturating_add(1);
        Ok(())
    }

    /// Whether `from` is really holding a token right now, asked of the ledger rather than assumed from how
    /// that grant was opened.
    ///
    /// What it is for: for DNS-over-TCP a token at risk sits either on the transport's own connection or on
    /// the debt a closing transport handed it to, and only one of those two owners can move it. Each asks
    /// this before it tries. The UDP handoff needs no such question - a query's token is always on that
    /// query's own grant, and its one terminal is the only place that moves it.
    pub fn holds_a_token(admission: &Admission, from: &Lease) -> bool {
        match admission.granted(from) {
            Some(granted) => granted.dns_tokens > 0,
            None => false,
        }
    }

    /// How many tokens this session has had to give up on.
    pub fn len(&self) -> u32 {
        self.held
    }

    pub fn is_empty(&self) -> bool {
        self.held == 0
    }
}

/// What a live connection owns: its flow buffers, its record, and one logical resolver token.
#[derive(Debug)]
pub struct Connection {
    lease: Lease,
    /// The query this connection currently has outstanding, if any. Named so that a close can hand that
    /// query the token rather than taking it away.
    outstanding: Option<u64>,
}

/// What one actually submitted query owns: a DNS-class descriptor record, and the query, answer and framing
/// bytes that submission will hold.
#[derive(Debug)]
pub struct QueryDebt {
    id: u64,
    lease: Lease,
}

impl Connection {
    /// The identity of the query this connection has outstanding, if any.
    pub fn outstanding(&self) -> Option<u64> {
        self.outstanding
    }

    /// Records that this connection has asked a question, or that its question is over.
    pub fn asking(&mut self, query: Option<u64>) {
        self.outstanding = query;
    }

    /// The grant, for an owner that needs to release it directly on a path this module does not cover.
    pub fn lease(&self) -> &Lease {
        &self.lease
    }
}

impl QueryDebt {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn lease(&self) -> &Lease {
        &self.lease
    }

    /// Nothing could account for a token a closing transport handed to this question, so nothing is given
    /// back at all: the grant stays charged until the session's aggregate is dropped.
    ///
    /// The mirror of [Stranded::kept], for the other half of the same problem. That one is a *connection*
    /// whose token could not reach its question; this one is the *question* whose token could not reach the
    /// table that outlives it - and [settle] must not be reached with such a debt, because settling releases
    /// the grant the token is sitting on and a token that looked free for an instant is a second query
    /// admitted against a limiter with no room for it.
    ///
    /// The empty body is the operation, for the reason spelled out on [Stranded::kept]: a [Lease] is a handle
    /// rather than a guard, and taking no `&mut Admission` is what stops a later edit reaching for a release
    /// from here without changing the shape of the call.
    pub fn kept(self) {}
}

/// Admits one DNS-over-TCP connection: its flow buffers, its record, and one logical token.
///
/// `resolver` is false for an ordinary TCP flow, which owes no token at all; the shape is shared so the two
/// cannot drift.
pub fn open(
    admission: &mut Admission,
    flow_bytes: u64,
    resolver: bool,
) -> Result<Connection, Denied> {
    let lease = admission.reserve(Request {
        records: 1,
        record_class: Class::General,
        bytes: flow_bytes,
        byte_class: Class::General,
        dns_tokens: u32::from(resolver),
        ..Request::default()
    })?;
    Ok(Connection {
        lease,
        outstanding: None,
    })
}

/// Admits one submitted query: a DNS-class descriptor record and the bytes that query will own.
///
/// No token. The connection is holding one already, and a second would halve how many connections the nested
/// cap allows.
pub fn submit(
    admission: &mut Admission,
    id: u64,
    exchange_bytes: u64,
) -> Result<QueryDebt, Denied> {
    let lease = admission.reserve(Request {
        records: 1,
        record_class: Class::Reserved,
        bytes: exchange_bytes,
        byte_class: Class::Reserved,
        ..Request::default()
    })?;
    Ok(QueryDebt { id, lease })
}

/// Admits one framed query this daemon will answer itself: its bytes, and the answer built from them.
///
/// No descriptor record and no token, because neither is taken: nothing leaves this process for a query the
/// platform is never asked about. What it still owns is real - the query has to be stored before a SERVFAIL
/// can echo its identifier and question, and the answer lives for as long as the client takes to read it -
/// so it is charged before either exists, exactly like a submitted one.
///
/// This is the second tier of [submit], and the tier order is what makes a refusal an answer rather than
/// silence: a query the descriptor floor has no room for can still be answered, and only a query whose *bytes*
/// do not fit is dropped.
pub fn hold(admission: &mut Admission, id: u64, bytes: u64) -> Result<QueryDebt, Denied> {
    let lease = admission.reserve(Request::bytes(bytes, Class::Reserved))?;
    Ok(QueryDebt { id, lease })
}

/// Ends one reservation whose query will never be submitted and will never be answered.
///
/// The transport was swept between the moment its query was admitted and the moment it handed one back, so
/// nothing survives this: no descriptor was opened, no platform slot was taken, and whatever buffer the
/// reservation covered is the caller's to drop *before* calling this - the grant is what accounts for it, and
/// giving it back while the bytes are alive is a refund for memory this process is still holding.
pub fn abandon(admission: &mut Admission, debt: QueryDebt) {
    admission.release(debt.lease);
}

/// A closed connection whose token did not reach the question that is still outstanding, with its grant
/// handed back to the caller rather than released.
///
/// The grant is *inside*, and that is the whole point: releasing it would return a logical token while the
/// platform may still be holding the slot that token stands for, and there is no way for [close] to know
/// where such a token belongs instead. Its caller does - it owns the session's [Quarantine] - so the grant
/// travels there, and the only two things that can be done with it are named below.
#[derive(Debug)]
pub struct Stranded {
    lease: Lease,
}

impl Stranded {
    /// The grant still holding the token, so its caller can move that token somewhere it will not be reused.
    pub fn lease(&self) -> &Lease {
        &self.lease
    }

    /// The token has been accounted for elsewhere, so what is left is ordinary bytes and a record the closed
    /// transport really is done with.
    pub fn released(self, admission: &mut Admission) {
        admission.release(self.lease);
    }

    /// Nothing could account for the token, so nothing is given back at all. The grant stays charged until
    /// this session's aggregate is dropped, which shows up as an outstanding lease in the exit report -
    /// conservative, visible, and bounded by the session, where the alternative is capacity handed back for a
    /// resolver slot Android is still holding.
    ///
    /// The empty body *is* the operation. A [Lease] is a handle rather than a guard - only
    /// [Admission::release] moves the ledger - so consuming one and letting it go is precisely "this row
    /// stays". What makes that safe to write as nothing at all is the signature: this takes no `&mut
    /// Admission`, so releasing the grant from here is not something a later edit can reach for without
    /// changing the shape of the call.
    pub fn kept(self) {}
}

/// Closes a connection, handing its token to the question still in flight if there is one.
///
/// The connection's own grant goes: its flow buffers and its record are what the closed transport owned. The
/// query's bytes are untouched, because the resolver still holds the query and will still return an answer -
/// releasing them here would be giving back memory that exists.
///
/// `debt` is the outstanding query's, and must be the one [Connection::outstanding] names.
///
/// # Every path that does not release the token
///
/// Three of them, and they used to be one. A connection that names an outstanding question and whose token
/// did not reach that question's debt may not have its grant released, because the token would go back into
/// circulation while the platform's slot for that question is still taken - and a token that looked free for
/// an instant is a second query admitted against a limiter with no room for it. So the transfer failing,
/// `debt` naming a *different* question, and `debt` being absent altogether are all the same answer:
/// [Stranded], with the grant handed back rather than released. They were previously the ordinary release
/// case, which was a fail-open on the one dimension this module exists to get right.
///
/// The exception is a connection that is not actually holding a token - asked of the ledger rather than
/// assumed - where there is nothing at risk and keeping its bytes and its record would be a leak for no
/// reason.
pub fn close(
    admission: &mut Admission,
    connection: Connection,
    debt: Option<&QueryDebt>,
) -> Result<(), Stranded> {
    let Connection { lease, outstanding } = connection;
    let Some(query) = outstanding else {
        // Closed idle, or its question settled before it did: the token goes back with the rest of the
        // grant, which is what releasing it does.
        admission.release(lease);
        return Ok(());
    };
    let moved = Request {
        dns_tokens: 1,
        ..Request::default()
    };
    let handed = match debt.filter(|debt| debt.id == query) {
        Some(debt) => admission.transfer(&lease, &debt.lease, moved).is_ok(),
        None => false,
    };
    if handed || !Quarantine::holds_a_token(admission, &lease) {
        admission.release(lease);
        return Ok(());
    }
    Err(Stranded { lease })
}

/// What is still owed after the resolver worker has been joined: the answer, the framed copy being built from
/// it, and the chunk on its way into the fair mailbox.
///
/// A separate owner because those buffers outlive the transaction that produced them. The worker's terminal
/// says the descriptor is closed - and, for every outcome this process could watch, that the platform's slot
/// is over with it. It says neither of the other two things: not that the answer has been delivered, and not
/// that an *unobservable* transaction's slot has ended, which is exactly the outcome whose token is
/// quarantined instead of released. Releasing the whole grant here gave back capacity for memory that was
/// about to be *created*:
/// the transport had yet to receive the answer, frame it, and hand each chunk over. Not `Clone`, like every
/// other grant here, so there is exactly one thing that can end it.
#[derive(Debug)]
pub struct Delivery {
    id: DeliveryId,
    lease: Lease,
}

/// Names one delivery exactly.
///
/// The submitted query's own identity, which its transaction table issues from a monotone counter and never
/// reuses, so an acknowledgment carrying one names a single delivery for the life of the process. That is
/// what a flow identity alone cannot do: a transport asks one question after another on the same flow, so an
/// acknowledgment naming only the flow matches whichever delivery happens to be parked when it arrives - and
/// a late one for a question already finished would release its *successor's* grant while the bytes that
/// grant covers are still being framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryId(u64);

impl DeliveryId {
    pub fn get(self) -> u64 {
        self.0
    }
}

impl Delivery {
    pub fn id(&self) -> DeliveryId {
        self.id
    }

    pub fn lease(&self) -> &Lease {
        &self.lease
    }
}

/// What an acknowledgment came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Acked {
    /// It named the delivery that is parked. The owner releases exactly this one.
    Released,
    /// It named a different delivery - a late acknowledgment for a question already finished, whose successor
    /// is now parked. A no-op: releasing here would give back a grant whose bytes are still being framed.
    Mismatched,
    /// Nothing is parked. A duplicate acknowledgment, or one for a delivery the flow's close already ended.
    Absent,
}

/// What one submitted query's owner holds while its resolver task runs.
///
/// The answer channel is here rather than with the transport, and that is the ordering the whole delivery
/// path depends on: the transport used to await the resolver directly, so it could frame an answer, hand
/// every piece over, take the last acknowledgment and report "delivered" before its own worker's terminal had
/// even been read - and parking happens at that terminal, so the report found nothing and the grant sat there
/// until the flow closed.
#[derive(Debug)]
pub struct Outstanding<R> {
    debt: QueryDebt,
    /// Where the resolver task leaves its finished answer. Read by the owner at the joined terminal, and by
    /// nothing before it.
    resolved: tokio::sync::oneshot::Receiver<R>,
}

impl<R> Outstanding<R> {
    pub fn new(debt: QueryDebt, resolved: tokio::sync::oneshot::Receiver<R>) -> Self {
        Self { debt, resolved }
    }

    pub fn debt(&self) -> &QueryDebt {
        &self.debt
    }

    /// The joined terminal: takes whatever the task left behind, ends the descriptor record and any logical
    /// token, and answers with what the delivery owner must now be given.
    ///
    /// The task's future is dropped by the time a terminal exists, so a value it sent is already here and one
    /// it never sent reads as closed. No waiting, and no window in which the transport could have seen it
    /// first.
    pub fn joined(mut self, admission: &mut Admission, delivery_bytes: u64) -> Settled<R> {
        let answer = self.resolved.try_recv().ok();
        Settled {
            delivery: settle(admission, self.debt, delivery_bytes),
            answer,
        }
    }
}

/// A settled query, waiting to be parked on the flow that will deliver it.
///
/// The answer is *in* here and there is no way to take it out except [Parked::park], which parks first. That
/// is the ordering made unspellable rather than merely unwritten.
#[derive(Debug)]
pub struct Settled<R> {
    delivery: Delivery,
    answer: Option<R>,
}

impl<R> Settled<R> {
    /// A delivery whose answer this daemon built itself, for a query the platform was never asked about.
    ///
    /// Nothing about the ordering changes: the answer is *inside* from the moment this exists and
    /// [Parked::park] is still the only way out of it, so a transport still cannot be handed an answer whose
    /// delivery is not somewhere an acknowledgment will find it. What differs from [Outstanding::joined] is
    /// only where the answer came from - a query refused before the platform was asked, answered under the
    /// grant that already covered it and reconciled down to what physically survives.
    ///
    /// `delivery` carries an id from the same counter a submitted query's does, and that is not a
    /// convenience: an acknowledgment names one delivery for the life of the process, so a refusal numbered
    /// out of a second id space could name one a real exchange is already using.
    pub fn delivering(delivery: Delivery, answer: R) -> Self {
        Self {
            delivery,
            answer: Some(answer),
        }
    }

    /// Whether there is an answer to deliver at all. `false` for a transaction that was cancelled.
    pub fn has_answer(&self) -> bool {
        self.answer.is_some()
    }

    /// A borrow of what is waiting, for an owner that has to say something about it before discarding it.
    ///
    /// Reading is not taking, so the park-first ordering is untouched: the answer still leaves only through
    /// [Settled::classify] and the park that follows it, and nothing can be delivered from here. What this
    /// exists for is the discard paths, where the last owner of a settlement has to describe a failure the
    /// transport it was built for will never see.
    pub fn answer(&self) -> Option<&R> {
        self.answer.as_ref()
    }

    /// Replaces the answer in place, without taking it out.
    ///
    /// The park-first property is what makes this a method rather than an accessor: the answer still leaves
    /// only through [Parked::park], so nothing downstream can be handed a buffer whose delivery has not been
    /// parked. What this exists for is the one case where the owner has to *substitute* an answer before it
    /// is delivered - a result resolved on a selection the session has stopped claiming, replaced by the
    /// query's own refusal - and doing that by extracting and re-wrapping would be exactly the ordering this
    /// type exists to forbid.
    ///
    /// `false` when there was nothing to replace, or when the replacement could not be built; the answer is
    /// gone in the second case, which leaves the caller to discard what is left.
    pub fn replace_answer(&mut self, replace: impl FnOnce(R) -> Option<R>) -> bool {
        let Some(answer) = self.answer.take() else {
            return false;
        };
        self.answer = replace(answer);
        self.answer.is_some()
    }

    /// Decides what this settled result really is, in place, and carries that decision in the type.
    ///
    /// The same rule as [Settled::replace_answer] and for the same reason: the value goes into a closure and
    /// whatever comes back goes straight back inside, so the only way anything leaves a settled delivery is
    /// still [Parked::park]. What this adds is that the decision can *change the type* - an owner that has
    /// worked out whether a result is deliverable at all may then hold something that cannot be anything
    /// else, rather than re-deciding after parking.
    ///
    /// That is what keeps an unacknowledgeable value from taking a delivery slot. A delivery is a lease its
    /// consumer gives back by naming it, so classifying afterwards meant parking a failure nobody would ever
    /// name - a grant that could only end when the whole flow closed. `None` parks nothing and releases the
    /// delivery at once, which is exactly what [Parked::park] already does for a transaction that produced
    /// nothing.
    pub fn classify<S>(self, classify: impl FnOnce(R) -> Option<S>) -> Settled<S> {
        let Self { delivery, answer } = self;
        Settled {
            delivery,
            answer: answer.and_then(classify),
        }
    }

    /// Nobody will consume this - the flow is gone, or there was never an answer. Drops the answer, then
    /// releases the delivery that covered it, exactly once.
    ///
    /// That order, and not the other one. The delivery grant *is* the accounting for this buffer, so
    /// releasing while the buffer is still alive is a refund for memory this process is still holding - for
    /// however long the rest of the function takes, which is exactly the shape of every early-release bug
    /// this path has had. Releasing `self.delivery` out of a partially moved `self` reads as one statement
    /// and is two, because what is left of `self` is dropped afterwards.
    pub fn discard(self, admission: &mut Admission) {
        let Self { delivery, answer } = self;
        // Whatever it is - a resolver answer, or the classified failure that replaced one - it goes here,
        // with whatever accounting it carries inside it.
        drop(answer);
        delivered(admission, delivery);
    }
}

/// What parking produced: the answer, now that it is safe to hand out, and whether it displaced another.
#[derive(Debug)]
pub struct Parking<R> {
    /// The answer and the exact delivery now parked for it, or `None` for a settled query that had none.
    ///
    /// Reachable only through here, which is what fixes the order: the answer cannot be handed to a
    /// transport before the delivery covering it is somewhere an acknowledgment will find it.
    pub answer: Option<(DeliveryId, R)>,
    /// A delivery was already parked here and has been released. A second answer for a question that was
    /// never asked; the caller counts it.
    pub replaced: bool,
}

/// One flow's delivery slot, and the three transitions its owner makes on it.
#[derive(Debug, Default)]
pub struct Parked {
    delivery: Option<Delivery>,
}

impl Parked {
    /// Whether anything is parked, and which delivery it is.
    pub fn id(&self) -> Option<DeliveryId> {
        self.delivery.as_ref().map(|delivery| delivery.id)
    }

    /// Parks a settled delivery on this flow and *then* hands its answer out.
    ///
    /// One call, so the order cannot be written the other way round. The transport learns the answer only
    /// after the delivery covering it is somewhere an acknowledgment can find it, which is what makes the
    /// acknowledgment that follows impossible to lose.
    ///
    /// A settled query with no answer parks nothing and releases at once: there is nothing to deliver and
    /// nobody will acknowledge it. A second delivery arriving while one is parked is a second answer for a
    /// question that was never asked - the old one is released and counted by the caller.
    pub fn park<R>(&mut self, admission: &mut Admission, settled: Settled<R>) -> Parking<R> {
        let Settled { delivery, answer } = settled;
        let Some(answer) = answer else {
            delivered(admission, delivery);
            return Parking {
                answer: None,
                replaced: false,
            };
        };
        let id = delivery.id;
        let mut replaced = false;
        if let Some(stale) = self.delivery.replace(delivery) {
            delivered(admission, stale);
            replaced = true;
        }
        Parking {
            answer: Some((id, answer)),
            replaced,
        }
    }

    /// Releases the parked delivery, and only if the acknowledgment names *it*.
    ///
    /// Both halves are checked before a release: the flow identity, which the caller validates and which says
    /// the acknowledgment came from a transport this owner still holds, and then this, which says it is about
    /// the answer that transport is still delivering. Either alone is insufficient - a flow outlives its
    /// answers, and an identity means nothing without the flow it belongs to.
    pub fn acknowledge(&mut self, admission: &mut Admission, acked: DeliveryId) -> Acked {
        match &self.delivery {
            Some(delivery) if delivery.id == acked => {
                // `take` rather than a borrow, so the release consumes the owner and a second acknowledgment
                // finds nothing.
                let delivery = self.delivery.take().expect("just matched");
                delivered(admission, delivery);
                Acked::Released
            }
            Some(_) => Acked::Mismatched,
            None => Acked::Absent,
        }
    }

    /// The flow is closing, so nothing will ever acknowledge what it was still delivering. Ends it here,
    /// exactly once.
    pub fn close(&mut self, admission: &mut Admission) {
        if let Some(delivery) = self.delivery.take() {
            delivered(admission, delivery);
        }
    }
}

/// Settles one query at its joined terminal, and hands what is left to the delivery that follows it.
///
/// Two things end here and one does not. The DNS-class descriptor record ends: the task is joined, so the
/// descriptor is closed. A logical token a closed transport handed over ends too - that is what "released
/// only at the real terminal" means, and it is why the transfer above is a move rather than a refund and a
/// reserve. What does *not* end is the answer: the transport has still to receive it, classify it, frame it
/// and hand each chunk to the client's stack, and every one of those exists after this returns.
///
/// So `delivery_bytes` is split out first and released only when that is finished. It is the conservative
/// peak of what remains - the result, the framed copy built beside it, and one chunk - reserved as part of
/// the original submission, so nothing here is a new charge and nothing is charged after an allocation.
pub fn settle(admission: &mut Admission, debt: QueryDebt, delivery_bytes: u64) -> Delivery {
    let QueryDebt { id, lease } = debt;
    // The query's own identity, which its table never reuses. Naming the delivery by it is what lets an
    // acknowledgment say *which* answer it is about rather than only which flow.
    let id = DeliveryId(id);
    match admission.split(&lease, Request::bytes(delivery_bytes, Class::Reserved)) {
        Ok(delivery) => {
            // The record, the token if one was moved here, and the query scratch the resolver has dropped.
            admission.release(lease);
            Delivery {
                id,
                lease: delivery,
            }
        }
        // No spare ledger row for the split. The whole grant becomes the delivery owner rather than being
        // released: the descriptor record stays charged a little past its close, which is the conservative
        // direction, and nothing is given back while the buffers it covers still exist.
        Err(_) => Delivery { id, lease },
    }
}

/// Ends one delivery, after every buffer it covered has actually been dropped or acknowledged.
///
/// Consuming the owner is what makes a double release unrepresentable, so a transport that finishes and a
/// close that finds the same delivery cannot both give it back.
pub fn delivered(admission: &mut Admission, delivery: Delivery) {
    admission.release(delivery.lease);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::admission::Totals;

    const FLOW_BYTES: u64 = 4_096;
    const QUERY_BYTES: u64 = 65_535;
    const DELIVERY_BYTES: u64 = 65_535 + (65_535 + 2) + 1_500;
    const EXCHANGE_BYTES: u64 = QUERY_BYTES + DELIVERY_BYTES;

    fn admission() -> Admission {
        Admission::new(Totals {
            admission_id: 1,
            record_total: 200,
            dns_record_floor: 64,
            byte_total: 8 << 20,
            reserved_byte_floor: 1 << 20,
            fragment_cap: 1 << 20,
            dns_token_cap: 32,
            byte_only_owners: 4,
        })
        .expect("the fixture totals hold their own accounting")
    }

    /// An idle connection owes its flow buffers and one token, and not one byte of an exchange it has not
    /// asked for.
    #[test]
    fn an_idle_connection_owes_no_exchange_bytes() {
        let mut admission = admission();
        let before = admission.bytes_charged();
        let connection = open(&mut admission, FLOW_BYTES, true).expect("granted");

        assert_eq!(admission.bytes_charged(), before + FLOW_BYTES);
        assert_eq!(admission.records_charged(), 1);
        assert_eq!(admission.dns_tokens_charged(), 1);
        assert_eq!(connection.outstanding(), None);
        // Explicitly: nothing like an exchange's worth is charged for a connection that has asked nothing.
        assert!(admission.bytes_charged() < before + EXCHANGE_BYTES);

        close(&mut admission, connection, None).expect("closed idle");
        assert_eq!(admission.bytes_charged(), before);
        assert_eq!(
            admission.dns_tokens_charged(),
            0,
            "an idle close returns it"
        );
        assert_eq!(admission.records_charged(), 0);
    }

    /// A submitted query owns the descriptor record *and* every byte of that exchange, and takes no second
    /// token.
    #[test]
    fn an_active_query_owns_the_descriptor_and_the_exchange_bytes() {
        let mut admission = admission();
        let before = admission.bytes_charged();
        let mut connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let debt = submit(&mut admission, 7, EXCHANGE_BYTES).expect("granted");
        connection.asking(Some(debt.id()));

        assert_eq!(
            admission.bytes_charged(),
            before + FLOW_BYTES + EXCHANGE_BYTES
        );
        assert_eq!(admission.records_charged(), 2, "the flow and the query");
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "the query took no second token"
        );

        close(&mut admission, connection, Some(&debt)).expect("the token moves");
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        assert_eq!(admission.bytes_charged(), before);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
    }

    /// A transport that closes over a question still in flight gives back its own buffers and leaves that
    /// question's bytes exactly where they are - because the resolver still holds them.
    #[test]
    fn closing_over_an_active_query_releases_the_flow_and_keeps_the_query() {
        let mut admission = admission();
        let before = admission.bytes_charged();
        let mut connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let debt = submit(&mut admission, 11, EXCHANGE_BYTES).expect("granted");
        connection.asking(Some(debt.id()));

        close(&mut admission, connection, Some(&debt)).expect("the token moves");
        // The flow's own buffers and record are gone...
        assert_eq!(
            admission.bytes_charged(),
            before + EXCHANGE_BYTES,
            "only the flow's bytes went"
        );
        assert_eq!(admission.records_charged(), 1, "the query's record stands");
        // ...and the token went *with the question*, not with the transport: the platform's slot is still
        // taken, so it may not look free for even an instant.
        assert_eq!(admission.dns_tokens_charged(), 1);
        assert_eq!(admission.granted(debt.lease()).expect("held").dns_tokens, 1);

        // It is released at the question's real terminal, and only then.
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        assert_eq!(admission.bytes_charged(), before);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// Thirty-two connections may each own one active query, and the cap still stops the thirty-third
    /// *connection* rather than the sixteenth query.
    #[test]
    fn every_token_holding_connection_may_own_one_query() {
        let mut admission = admission();
        let mut connections = Vec::new();
        for _ in 0..32 {
            connections.push(open(&mut admission, FLOW_BYTES, true).expect("granted"));
        }
        assert_eq!(admission.dns_tokens_charged(), 32);
        // The next connection is denied on the token, which is the limit that was chosen.
        assert_eq!(
            open(&mut admission, FLOW_BYTES, true).map(|_| ()),
            Err(Denied::DnsTokens)
        );

        let mut debts = Vec::new();
        for (index, connection) in connections.iter_mut().enumerate() {
            let debt = submit(&mut admission, index as u64, EXCHANGE_BYTES)
                .expect("a query needs no second token");
            connection.asking(Some(debt.id()));
            debts.push(debt);
        }
        assert_eq!(
            admission.dns_tokens_charged(),
            32,
            "thirty-two queries created no tokens at all"
        );
        assert_eq!(admission.records_charged(), 64);

        for (connection, debt) in connections.into_iter().zip(debts.iter()) {
            close(&mut admission, connection, Some(debt)).expect("the token moves");
        }
        assert_eq!(
            admission.dns_tokens_charged(),
            32,
            "still with the questions"
        );
        for debt in debts {
            let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
            delivered(&mut admission, delivery);
        }
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A close naming a question that is not this connection's does not move anything, and neither grant is
    /// disturbed.
    #[test]
    fn a_close_cannot_hand_its_token_to_someone_elses_question() {
        let mut admission = admission();
        let mut connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let mine = submit(&mut admission, 1, EXCHANGE_BYTES).expect("granted");
        let theirs = submit(&mut admission, 2, EXCHANGE_BYTES).expect("granted");
        connection.asking(Some(mine.id()));

        // Named the wrong debt: the token reaches neither question, so the grant comes back stranded rather
        // than being released. Releasing it would return a token to circulation while the platform's slot for
        // the question this connection *does* name is still taken - which is what the previous ordinary
        // release did, and what a nested cap cannot survive.
        let stranded =
            close(&mut admission, connection, Some(&theirs)).expect_err("nothing could be moved");
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "still charged, because nothing has accounted for it yet"
        );
        assert_eq!(
            admission.granted(theirs.lease()).expect("held").dns_tokens,
            0,
            "and it did not land on a question that never owned it"
        );
        assert_eq!(admission.granted(mine.lease()).expect("held").dns_tokens, 0);
        // And both questions still own exactly their own bytes.
        assert_eq!(
            admission.granted(mine.lease()).expect("held").bytes,
            EXCHANGE_BYTES
        );

        // Its caller is the only owner that knows where such a token belongs: onto a session-lived grant it
        // already holds. Only then is the rest of the closed transport's grant released.
        // Shaped like the retained-table grant the daemon's owners really move a token onto: real
        // reserved-class bytes, so the transfer touches only the token dimension.
        let session = admission
            .reserve(Request::bytes(1, Class::Reserved))
            .expect("a session-lived grant");
        let mut quarantine = Quarantine::default();
        quarantine
            .take(&mut admission, stranded.lease(), &session)
            .expect("a move onto a row that already exists");
        stranded.released(&mut admission);
        assert_eq!(quarantine.len(), 1);
        assert_eq!(
            admission.granted(&session).expect("held").dns_tokens,
            1,
            "the token is out of circulation until this grant is released"
        );

        let delivery = settle(&mut admission, mine, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        let delivery = settle(&mut admission, theirs, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        assert_eq!(
            admission.dns_tokens_charged(),
            1,
            "and only the quarantined one is left"
        );
        admission.release(session);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// Settling twice is unrepresentable - the debt is consumed - and a settle after a close changes nothing
    /// beyond what that one grant owed.
    #[test]
    fn a_settled_query_cannot_alter_capacity_twice() {
        let mut admission = admission();
        let before = (admission.records_charged(), admission.bytes_charged());
        let debt = submit(&mut admission, 3, EXCHANGE_BYTES).expect("granted");
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            before
        );
        assert_eq!(admission.invariant_violations(), 0);
        // A second settle would need a second debt, and there is no way to make one from the first: `settle`
        // consumes it. What a stale terminal can do instead is name a query the table no longer holds, which
        // never reaches here at all.
    }

    /// A buffer that reports which release it was dropped before or after, read from the aggregate's own
    /// release ledger rather than from the aggregate - which the release is holding exclusively at the time.
    struct Reports {
        at: std::rc::Rc<std::cell::Cell<Option<u64>>>,
    }

    impl Drop for Reports {
        fn drop(&mut self) {
            self.at
                .set(Some(crate::shared::admission::releases::so_far()));
        }
    }

    /// A discarded delivery gives its buffer back *before* the capacity that covered it.
    ///
    /// The order is the property, and it is invisible in a balance: the delivery is released exactly once
    /// either way and the buffer is dropped exactly once either way, so an owner that refunds first and then
    /// drops looks identical from outside. What tells them apart is asking the buffer, at its own drop, how
    /// many releases had happened by then - and neither that count nor the drop is written by the function
    /// under test. Refunding first is a refund for memory this process is still holding, for the whole rest
    /// of the function.
    ///
    /// The answer arrives the way the daemon's does: through the channel its resolver task sends on, taken by
    /// the owner at the joined terminal. Nothing is handed to [Settled] directly.
    #[test]
    fn a_discarded_delivery_drops_its_answer_before_it_refunds() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let dropped_at = std::rc::Rc::new(std::cell::Cell::new(None));

        let debt = submit(&mut admission, 11, EXCHANGE_BYTES).expect("granted");
        let (finished, resolved) = tokio::sync::oneshot::channel();
        finished
            .send(Reports {
                at: dropped_at.clone(),
            })
            .ok()
            .expect("the owner has not read it yet");
        let settled = Outstanding::new(debt, resolved).joined(&mut admission, DELIVERY_BYTES);
        assert!(settled.has_answer());
        assert_eq!(dropped_at.get(), None, "still held after the join");

        // Nobody will consume it - there is no flow left to deliver to. This is the last owner that can end
        // it.
        let before = crate::shared::admission::releases::so_far();
        settled.discard(&mut admission);
        assert_eq!(
            dropped_at.get(),
            Some(before),
            "the answer was dropped before the release, not after it"
        );
        assert_eq!(
            crate::shared::admission::releases::so_far(),
            before + 1,
            "and the delivery was released exactly once"
        );
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A conservative delivery reservation covers the largest answer there is, framed, with a chunk being
    /// handed over - and covers it from the submission rather than from an allocation.
    #[test]
    fn a_full_capacity_result_and_framing_stay_inside_the_reservation() {
        // The real peak the transport can reach after the join: the result the platform returned, the
        // length-prefixed copy built beside it, and one chunk on its way into the mailbox.
        const PREFIX: u64 = 2;
        const CHUNK: u64 = 1_500;
        let peak = 65_535 + (65_535 + PREFIX) + CHUNK;
        assert!(
            peak <= DELIVERY_BYTES,
            "a maximum answer, framed, with a chunk out: {peak} > {DELIVERY_BYTES}"
        );
        // And the submission covers that peak plus the query it was made from, so nothing on this path is
        // ever charged after it has been allocated.
        assert!(QUERY_BYTES + peak <= EXCHANGE_BYTES);

        let mut admission = admission();
        let debt = submit(&mut admission, 1, EXCHANGE_BYTES).expect("granted");
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        assert!(admission.granted(delivery.lease()).expect("held").bytes >= peak);
        delivered(&mut admission, delivery);
    }

    /// A transport that closes while its question is in flight keeps both: the token moves to that question,
    /// and the answer's bytes stay with the delivery that follows it.
    #[test]
    fn closing_over_an_active_query_keeps_the_token_and_the_delivery() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let mut connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let debt = submit(&mut admission, 9, EXCHANGE_BYTES).expect("granted");
        connection.asking(Some(debt.id()));

        close(&mut admission, connection, Some(&debt)).expect("the token moves");
        assert_eq!(admission.dns_tokens_charged(), 1, "with the question");
        assert_eq!(admission.bytes_charged(), empty + EXCHANGE_BYTES);

        // The join then ends the token with the descriptor - the platform's slot really is over - while the
        // answer's bytes carry on into the delivery.
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        assert_eq!(admission.dns_tokens_charged(), 0, "the slot is over");
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.bytes_charged(), empty + DELIVERY_BYTES);

        delivered(&mut admission, delivery);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A consumer that disappeared refunds exactly once, and a stale or repeated terminal cannot add to it.
    #[test]
    fn a_lost_consumer_refunds_once_and_a_stale_terminal_adds_nothing() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let debt = submit(&mut admission, 13, EXCHANGE_BYTES).expect("granted");
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);

        // The transport was swept before it could take the answer, so nobody will ever acknowledge it. The
        // owner that still holds the delivery gives it back - once.
        delivered(&mut admission, delivery);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.invariant_violations(), 0);

        // A second ending for the same query cannot be spelled: `settle` consumed the debt and `delivered`
        // consumed the delivery. What a stale terminal *can* do is name a transaction the table no longer
        // holds, which never reaches either of them - and if a lease from a replaced session did arrive, it
        // is counted rather than believed, and creates nothing.
        let mut other = Admission::new(Totals {
            admission_id: 99,
            ..Totals {
                admission_id: 1,
                record_total: 200,
                dns_record_floor: 64,
                byte_total: 8 << 20,
                reserved_byte_floor: 1 << 20,
                fragment_cap: 1 << 20,
                dns_token_cap: 32,
                byte_only_owners: 4,
            }
        })
        .expect("granted");
        let foreign = submit(&mut other, 1, EXCHANGE_BYTES).expect("granted");
        let foreign = settle(&mut other, foreign, DELIVERY_BYTES);
        let charged = (admission.records_charged(), admission.bytes_charged());
        delivered(&mut admission, foreign);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            charged,
            "a stale delivery creates no capacity"
        );
        assert_eq!(admission.invariant_violations(), 1, "and is counted");
    }

    /// The second tier owes the answer and no descriptor, which is the whole reason it exists: a query the
    /// record floor has no room for can still be answered rather than dropped.
    #[test]
    fn a_query_answered_here_owes_no_record_and_no_token() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let records = admission.records_charged();

        // Every descriptor is held by something else, so a submission cannot be admitted at all...
        let held = admission
            .reserve(Request::records(
                admission.record_total() - records,
                Class::Reserved,
            ))
            .expect("every record that is left");
        assert_eq!(
            submit(&mut admission, 5, EXCHANGE_BYTES).map(|_| ()),
            Err(Denied::Records)
        );
        // ...and the answer this daemon builds instead needs none.
        let debt = hold(&mut admission, 5, QUERY_BYTES + DELIVERY_BYTES).expect("granted");
        assert_eq!(
            admission.records_charged(),
            admission.record_total(),
            "no record was taken for it"
        );
        assert_eq!(admission.dns_tokens_charged(), 1, "and no second token");
        assert_eq!(
            admission.granted(debt.lease()).expect("held").bytes,
            QUERY_BYTES + DELIVERY_BYTES
        );

        // A reservation whose query is never answered ends whole, once.
        abandon(&mut admission, debt);
        assert_eq!(admission.bytes_charged(), empty + FLOW_BYTES);
        admission.release(held);
        close(&mut admission, connection, None).expect("closed idle");
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A query whose bytes do not fit is refused whole: no descriptor record is taken either.
    #[test]
    fn a_query_that_cannot_fit_its_bytes_takes_no_record() {
        let mut admission = admission();
        let connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let charged = (admission.records_charged(), admission.bytes_charged());
        assert_eq!(
            submit(&mut admission, 1, u64::MAX).map(|_| ()),
            Err(Denied::Arithmetic)
        );
        assert_eq!(
            submit(&mut admission, 1, 100 << 20).map(|_| ()),
            Err(Denied::Bytes)
        );
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            charged,
            "a refused query charges nothing at all"
        );
        close(&mut admission, connection, None).expect("closed idle");
    }
}
