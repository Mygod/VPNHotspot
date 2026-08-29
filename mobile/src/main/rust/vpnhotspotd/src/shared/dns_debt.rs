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
pub fn hold(admission: &mut Admission, id: u64, bytes: u64) -> Result<QueryDebt, Denied> {
    let lease = admission.reserve(Request::bytes(bytes, Class::Reserved))?;
    Ok(QueryDebt { id, lease })
}

/// Ends one query's debt whole: its record, its bytes, and a token a closing transport had handed it.
pub fn abandon(admission: &mut Admission, debt: QueryDebt) {
    admission.release(debt.lease);
}

/// Closes a connection, handing its token to the question still in flight if there is one.
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
#[derive(Debug)]
pub struct Delivery {
    id: DeliveryId,
    lease: Lease,
}

/// Names one delivery exactly.
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
#[derive(Debug)]
pub struct Settled<R> {
    delivery: Delivery,
    answer: Option<R>,
}

impl<R> Settled<R> {
    /// A delivery whose answer this daemon built itself, for a query the platform was never asked about.
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
    pub fn replace_answer(&mut self, replace: impl FnOnce(R) -> Option<R>) -> bool {
        let Some(answer) = self.answer.take() else {
            return false;
        };
        self.answer = replace(answer);
        self.answer.is_some()
    }

    /// Decides what this settled result really is, in place, and carries that decision in the type.
    pub fn classify<S>(self, classify: impl FnOnce(R) -> Option<S>) -> Settled<S> {
        let Self { delivery, answer } = self;
        Settled {
            delivery,
            answer: answer.and_then(classify),
        }
    }

    /// Nobody will consume this - the flow is gone, or there was never an answer. Drops the answer, then
    /// releases the delivery that covered it, exactly once.
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

    #[test]
    fn an_idle_connection_owes_no_exchange_bytes() {
        let mut admission = admission();
        let before = admission.bytes_charged();
        let connection = open(&mut admission, FLOW_BYTES, true).expect("granted");

        assert_eq!(admission.bytes_charged(), before + FLOW_BYTES);
        assert_eq!(admission.records_charged(), 1);
        assert_eq!(admission.dns_tokens_charged(), 1);
        assert_eq!(connection.outstanding(), None);
        assert!(admission.bytes_charged() < before + EXCHANGE_BYTES);

        assert!(close(&mut admission, connection, None));
        assert_eq!(admission.bytes_charged(), before);
        assert_eq!(
            admission.dns_tokens_charged(),
            0,
            "an idle close returns it"
        );
        assert_eq!(admission.records_charged(), 0);
    }

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
        assert_eq!(admission.records_charged(), 2);
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
        assert_eq!(
            admission.bytes_charged(),
            before + EXCHANGE_BYTES,
            "only the flow's bytes went"
        );
        assert_eq!(admission.records_charged(), 1);
        assert_eq!(admission.dns_tokens_charged(), 1);
        assert_eq!(admission.granted(debt.lease()).expect("held").dns_tokens, 1);

        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        assert_eq!(admission.bytes_charged(), before);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn every_token_holding_connection_may_own_one_query() {
        let mut admission = admission();
        let mut connections = Vec::new();
        for _ in 0..32 {
            connections.push(open(&mut admission, FLOW_BYTES, true).expect("granted"));
        }
        assert_eq!(admission.dns_tokens_charged(), 32);
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
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_wrapper_failure_gives_a_querys_whole_debt_back() {
        let mut admission = admission();
        let baseline = (
            admission.records_charged(),
            admission.bytes_charged(),
            admission.dns_tokens_charged(),
        );

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
    }

    #[test]
    fn a_full_capacity_result_and_framing_stay_inside_the_reservation() {
        const PREFIX: u64 = 2;
        let peak = 65_535 + (65_535 + PREFIX);
        assert!(
            peak <= DELIVERY_BYTES,
            "a maximum answer and its framing: {peak} > {DELIVERY_BYTES}"
        );
        assert!(QUERY_BYTES + peak <= EXCHANGE_BYTES);

        let mut admission = admission();
        let debt = submit(&mut admission, 1, EXCHANGE_BYTES).expect("granted");
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);
        assert!(admission.granted(delivery.lease()).expect("held").bytes >= peak);
        delivered(&mut admission, delivery);
    }

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
        assert_eq!(admission.dns_tokens_charged(), 1);
        assert_eq!(admission.bytes_charged(), empty + EXCHANGE_BYTES);

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

    #[test]
    fn a_lost_consumer_refunds_once_and_a_stale_terminal_adds_nothing() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let debt = submit(&mut admission, 13, EXCHANGE_BYTES).expect("granted");
        let delivery = settle(&mut admission, debt, DELIVERY_BYTES);

        delivered(&mut admission, delivery);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.invariant_violations(), 0);

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
        assert_eq!(admission.invariant_violations(), 1);
    }

    #[test]
    fn a_query_answered_here_owes_no_record_and_no_token() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let connection = open(&mut admission, FLOW_BYTES, true).expect("granted");
        let records = admission.records_charged();

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
        let debt = hold(&mut admission, 5, QUERY_BYTES + DELIVERY_BYTES).expect("granted");
        assert_eq!(
            admission.records_charged(),
            admission.record_total(),
            "no record was taken for it"
        );
        assert_eq!(admission.dns_tokens_charged(), 1);
        assert_eq!(
            admission.granted(debt.lease()).expect("held").bytes,
            QUERY_BYTES + DELIVERY_BYTES
        );

        abandon(&mut admission, debt);
        assert_eq!(admission.bytes_charged(), empty + FLOW_BYTES);
        admission.release(held);
        assert!(close(&mut admission, connection, None));
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

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
        assert!(close(&mut admission, connection, None));
    }
}
