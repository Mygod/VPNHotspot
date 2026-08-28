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
//!   re-reserved, because that question is still outstanding *here*: it has not settled, so the token this
//!   session took for it is not free, and a moment where it looked free is a moment a second query could be
//!   admitted beyond what this session sized itself for. Whether Android is still working on it is a
//!   separate question nothing here asks. The query's bytes are not touched at all - the resolver task still
//!   holds the query and will still return an answer.
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
}

impl QueryDebt {
    #[cfg(test)]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// What this query is charged against, for a test that has to read the ledger row directly. Nothing in
    /// production reaches for it: every move and release here takes the debt itself, which is what makes a
    /// double release unrepresentable.
    #[cfg(test)]
    pub fn lease(&self) -> &Lease {
        &self.lease
    }
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

/// Ends one query's debt whole: its record, its bytes, and a token a closing transport had handed it.
///
/// Two callers, and nothing survives this for either. A reservation whose query will never be submitted -
/// the transport was swept between the moment its query was admitted and the moment it handed one back -
/// opened no descriptor and never reached the platform. A submitted query whose wrapper this daemon could
/// not build or watch did reach it, and this still gives everything back: `android_res_cancel` closed the
/// descriptor, Android's own operation ends when its resolver work returns, and the owner that met such a
/// failure is ending anyway, so there is nothing local left worth holding.
///
/// Whatever buffer the debt covered is the caller's to drop *before* calling this - the grant is what
/// accounts for it, and giving it back while the bytes are alive is a refund for memory this process is
/// still holding.
pub fn abandon(admission: &mut Admission, debt: QueryDebt) {
    admission.release(debt.lease);
}

/// Closes a connection, handing its token to the question still in flight if there is one.
///
/// The connection's own grant goes: its flow buffers and its record are what the closed transport owned. The
/// query's bytes are untouched, because the resolver still holds the query and will still return an answer -
/// releasing them here would be giving back memory that exists.
///
/// `debt` is the outstanding query's, and must be the one [Connection::outstanding] names.
///
/// # `false` is this process's own accounting contradicting itself
///
/// A connection that names an outstanding question is closed by the one owner that recorded that question,
/// and that owner records it only after the row exists, clears it before the row is removed, and does both
/// synchronously in its own serial order. So a close that finds no debt for the question it names, a debt
/// naming a *different* question, or a transfer the ledger refuses is not a resolver lifetime this module
/// failed to model - it is a state this daemon's own ownership rules say cannot happen.
///
/// There is no honest local recovery from that and no state worth inventing for it. Keeping the grant would
/// hold a descriptor record and a token nothing will ever release for the rest of the session, on the
/// strength of a contradiction that says the ledger cannot be trusted anyway. So the grant is released like
/// any other and `false` says the invariant broke, which the caller reports - see
/// [crate::shared::protocol] for what such a report has to carry.
///
/// `true` for every ordinary close, including a connection that is not actually holding a token - asked of
/// the ledger rather than assumed - which is what a transport that already handed its token to the question
/// settling now looks like.
pub fn close(admission: &mut Admission, connection: Connection, debt: Option<&QueryDebt>) -> bool {
    let Connection { lease, outstanding } = connection;
    let Some(query) = outstanding else {
        // Closed idle, or its question settled before it did: the token goes back with the rest of the
        // grant, which is what releasing it does.
        admission.release(lease);
        return true;
    };
    let moved = Request {
        dns_tokens: 1,
        ..Request::default()
    };
    let handed = match debt.filter(|debt| debt.id == query) {
        Some(debt) => admission.transfer(&lease, &debt.lease, moved).is_ok(),
        None => false,
    };
    // Asked of the ledger rather than assumed from how this grant was opened, and read before the release
    // that would make it unanswerable: a connection holding no token had nothing to hand over, so nothing
    // about it is a contradiction.
    let contradicted = !handed
        && admission
            .granted(&lease)
            .is_some_and(|granted| granted.dns_tokens > 0);
    admission.release(lease);
    !contradicted
}

/// What is still owed after the resolver worker has been joined: the answer, and the framed copy being built
/// from it.
///
/// A separate owner because those buffers outlive the transaction that produced them. The worker's terminal
/// says the descriptor is closed and that the question this process asked is over. It does not say that the
/// answer has been delivered: releasing the whole grant there gave back capacity for memory that was about
/// to be *created*, because the transport had yet to receive the answer, frame it, and write the framing
/// into its flow's bridge. Not `Clone`, like every other grant here, so there is exactly one thing that can
/// end it.
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

impl Delivery {
    /// The same as [QueryDebt::lease] and for the same reason: a test reads the row, production takes the
    /// owner.
    #[cfg(test)]
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
    /// delivery is not somewhere an acknowledgment will find it. The answer came from a query refused before
    /// the platform was asked, under the grant that already covered it and reconciled down to what physically
    /// survives.
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
/// and write the framing into its flow's bridge, and every one of those exists after this returns.
///
/// So `delivery_bytes` is split out first and released only when that is finished. It is the conservative
/// peak of what remains - the result and the framed copy built beside it - reserved as part of the original
/// submission, so nothing here is a new charge and nothing is charged after an allocation. There is no third
/// term, because the framing is written into a bridge whose capacity the flow was charged for before it
/// existed.
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
    const DELIVERY_BYTES: u64 = 65_535 + (65_535 + 2);
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

        assert!(close(&mut admission, connection, None), "closed idle");
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

        assert!(
            close(&mut admission, connection, Some(&debt)),
            "the token moves"
        );
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

        assert!(
            close(&mut admission, connection, Some(&debt)),
            "the token moves"
        );
        // The flow's own buffers and record are gone...
        assert_eq!(
            admission.bytes_charged(),
            before + EXCHANGE_BYTES,
            "only the flow's bytes went"
        );
        assert_eq!(admission.records_charged(), 1, "the query's record stands");
        // ...and the token went *with the question*, not with the transport: that question is still
        // outstanding, so the token it stands for may not look free for even an instant.
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
            assert!(
                close(&mut admission, connection, Some(debt)),
                "the token moves"
            );
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

    /// A close naming a question that is not this connection's moves nothing, disturbs neither question's
    /// grant, and says the invariant broke.
    ///
    /// The one owner that records a connection's outstanding question records it only after that question's
    /// row exists, clears it before the row is removed, and does both synchronously - so reaching here means
    /// this process's own bookkeeping has contradicted itself rather than that a resolver lifetime went
    /// unmodelled. There is nothing honest to keep for that: the closed transport's grant is released like
    /// any other, and `false` is what its caller turns into one structured report.
    #[test]
    fn a_close_cannot_hand_its_token_to_someone_elses_question() {
        let mut admission = admission();
        let before = admission.bytes_charged();
        let mut connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let mine = submit(&mut admission, 1, EXCHANGE_BYTES).expect("granted");
        let theirs = submit(&mut admission, 2, EXCHANGE_BYTES).expect("granted");
        connection.asking(Some(mine.id()));

        assert!(
            !close(&mut admission, connection, Some(&theirs)),
            "a debt naming another question is the caller's invariant, not a move"
        );
        assert_eq!(
            admission.granted(theirs.lease()).expect("held").dns_tokens,
            0,
            "and it did not land on a question that never owned it"
        );
        assert_eq!(admission.granted(mine.lease()).expect("held").dns_tokens, 0);
        // Both questions still own exactly their own bytes, and the closed transport's grant - token and all
        // - is gone rather than held for a session against a contradiction.
        assert_eq!(
            admission.granted(mine.lease()).expect("held").bytes,
            EXCHANGE_BYTES
        );
        assert_eq!(admission.dns_tokens_charged(), 0);

        let delivery = settle(&mut admission, mine, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        let delivery = settle(&mut admission, theirs, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(
            admission.bytes_charged(),
            before,
            "and nothing this close was holding stayed behind"
        );
        // The transfer never happened, so nothing here is charged to the ledger's own violation counter: the
        // contradiction is the *caller's* to report, from the answer above.
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A query this daemon's own wrapper failed on gives its whole debt back, in both places one can be
    /// holding a token.
    ///
    /// The two sequences the DNS-over-TCP owner performs when it ends on such a failure, spelled with the
    /// same functions it calls: a submission that never became a live row, and a settled transaction whose
    /// transport had already closed onto it. Neither may keep anything - the platform's operation ends on its
    /// own, and there is no local state worth holding for a session that is about to end - so what this pins
    /// down is that `abandon` really is the whole of it, records, bytes and token alike.
    #[test]
    fn a_wrapper_failure_gives_a_querys_whole_debt_back() {
        let mut admission = admission();
        let baseline = (
            admission.records_charged(),
            admission.bytes_charged(),
            admission.dns_tokens_charged(),
        );

        // The submission path: a row was taken, the platform was asked, and this process could not wrap what
        // it returned. Its own transport is still live and still holding the one token, which is not this
        // debt's to give back.
        let connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let debt = submit(&mut admission, 1, EXCHANGE_BYTES).expect("granted");
        assert_eq!(admission.records_charged(), baseline.0 + 2);
        abandon(&mut admission, debt);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            (baseline.0 + 1, baseline.1 + FLOW_BYTES),
            "the query's record and every byte of it went; the connection's did not"
        );
        assert_eq!(
            admission.dns_tokens_charged(),
            baseline.2 + 1,
            "and the token is still the transport's, to release when it closes"
        );
        assert!(
            close(&mut admission, connection, None),
            "which it does with the rest of its grant"
        );
        assert_eq!(
            (
                admission.records_charged(),
                admission.bytes_charged(),
                admission.dns_tokens_charged()
            ),
            baseline
        );

        // The settled path: the transport closed while the question was outstanding, so the token is on the
        // debt by the time the failure is classified - and `abandon` has to give that one back too, or a
        // session would end holding a token nothing will ever release.
        let mut connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let debt = submit(&mut admission, 2, EXCHANGE_BYTES).expect("granted");
        connection.asking(Some(debt.id()));
        assert!(
            close(&mut admission, connection, Some(&debt)),
            "handed over"
        );
        assert_eq!(
            admission.granted(debt.lease()).expect("held").dns_tokens,
            1,
            "the token is the question's now"
        );
        abandon(&mut admission, debt);
        assert_eq!(
            (
                admission.records_charged(),
                admission.bytes_charged(),
                admission.dns_tokens_charged()
            ),
            baseline,
            "and abandoning the question gives it back with everything else"
        );
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

    /// A conservative delivery reservation covers the largest answer there is and the framed copy built
    /// beside it - and covers them from the submission rather than from an allocation.
    #[test]
    fn a_full_capacity_result_and_framing_stay_inside_the_reservation() {
        // The real peak the transport can reach after the join: the result the platform returned and the
        // length-prefixed copy built beside it. There is no third term, because the framed copy is written
        // straight into the flow's bridge, whose capacity the flow was charged for before it existed.
        const PREFIX: u64 = 2;
        let peak = 65_535 + (65_535 + PREFIX);
        assert!(
            peak <= DELIVERY_BYTES,
            "a maximum answer and its framing: {peak} > {DELIVERY_BYTES}"
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

        assert!(
            close(&mut admission, connection, Some(&debt)),
            "the token moves"
        );
        assert_eq!(admission.dns_tokens_charged(), 1, "with the question");
        assert_eq!(admission.bytes_charged(), empty + EXCHANGE_BYTES);

        // The join then ends the token with the descriptor, while the answer's bytes carry on into the
        // delivery. What that proves is this daemon's own accounting and nothing more: the descriptor is
        // closed and the query's debt is over, so the local token may be reused. Android's operation for the
        // same query is its own, ends when its resolver work returns, and is neither observed nor waited for
        // here.
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        assert_eq!(
            admission.dns_tokens_charged(),
            0,
            "this daemon's own token is free again"
        );
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
        assert!(close(&mut admission, connection, None), "closed idle");
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
        assert!(close(&mut admission, connection, None), "closed idle");
    }
}
