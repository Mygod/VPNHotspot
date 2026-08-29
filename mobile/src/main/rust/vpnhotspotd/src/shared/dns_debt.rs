//! DNS resource ownership from query admission through answer acknowledgment.
//!
//! Submitted queries own a DNS-class admission record and precharged query, answer and framing bytes. A
//! returned resolver descriptor is owned until completion or drop; a synchronous expected refusal retains
//! the admission record and byte charges without owning a descriptor. The daemon does not own or track the
//! Android resolver slot an accepted query may consume. A DNS-over-TCP flow owns its transport state and the
//! question currently being framed; submitted queries outlive transport closure.
use crate::shared::admission::{Admission, Class, Denied, Lease, Request};
use crate::shared::dns_wire::{MAX_MESSAGE, PREFIX};

/// Minimum submitted-query charge: one maximum question and one maximum answer.
/// Used to derive [rows]; this is a worst-case allowance, not the observed message size.
pub const MINIMUM_SUBMITTED_BYTES: u64 = 2 * MAX_MESSAGE as u64;

/// Maximum submitted-query charge: a maximum-length TCP question, maximum answer and its length-prefixed
/// framing copy. The reserved byte floor must hold this amount.
pub const MAXIMUM_SUBMITTED_BYTES: u64 = 3 * MAX_MESSAGE as u64 + PREFIX as u64;

/// Resource-derived bound on concurrent submitted queries.
///
/// Every table-retained query, including a synchronous expected refusal, owns one admission record and at
/// least [MINIMUM_SUBMITTED_BYTES], so records or bytes run out no later than this storage bound. It is not
/// an admission limit; those resources remain authoritative, and Android's asynchronous per-UID resolver
/// limit is unrelated.
pub fn rows(admission: &Admission) -> usize {
    let by_bytes = admission.byte_total() / MINIMUM_SUBMITTED_BYTES;
    // The minimum is at most u32::MAX, so it fits usize on supported targets.
    u64::from(admission.record_total()).min(by_bytes) as usize
}

/// Charges fixed DNS storage to general bytes before traffic bounds are derived.
pub fn tables(admission: &mut Admission, bytes: u64) -> Result<Lease, Denied> {
    admission.reserve(fixed(bytes))
}

/// Charges the fixed TUN writer queue and input buffer as general bytes.
pub fn fixed_io(writer_queue: u64, input_buffer: u64) -> Option<Request> {
    Some(fixed(writer_queue.checked_add(input_buffer)?))
}

/// Charges fixed daemon storage as General so it cannot spend the floor reserved for one essential exchange.
fn fixed(bytes: u64) -> Request {
    Request::bytes(bytes, Class::General)
}

/// What one actually submitted query owns: a DNS-class descriptor record, and the query, answer and framing
/// bytes that submission will hold.
#[derive(Debug)]
pub struct QueryDebt {
    id: u64,
    lease: Lease,
}

impl QueryDebt {
    /// What this query is charged against, for a test that has to read the ledger row directly. Nothing in
    /// production reaches for it: every move and release here takes the debt itself, which is what makes a
    /// double release unrepresentable.
    #[cfg(test)]
    pub fn lease(&self) -> &Lease {
        &self.lease
    }
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

/// Ends one query's debt whole: its record and every byte it was granted, exactly once.
pub fn abandon(admission: &mut Admission, debt: QueryDebt) {
    admission.release(debt.lease);
}

/// What is still owed once a query has reached its own terminal: the answer, and the framed copy being built
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

    /// Whether classification retained an answer for delivery.
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

/// Result of splitting a terminal query's debt into a delivery.
#[derive(Debug)]
pub enum Split {
    /// The delivery is covered. A denial means ledger or identity exhaustion left the source unchanged; the
    /// source is retained conservatively and the denial must still be reported.
    Covered(Delivery, Option<Denied>),
    /// No grant is known to cover the delivery. Drop its buffers, then abandon the returned debt.
    Uncovered(QueryDebt, Denied),
}

/// Settles one query at its terminal, and hands what is left to the delivery that follows it.
pub fn settle(admission: &mut Admission, debt: QueryDebt, delivery_bytes: u64) -> Split {
    let QueryDebt { id, lease } = debt;
    // The query's own identity, which its table never reuses. Naming the delivery by it is what lets an
    // acknowledgment say *which* answer it is about rather than only which flow.
    let delivery = DeliveryId(id);
    match admission.split(&lease, Request::bytes(delivery_bytes, Class::Reserved)) {
        Ok(taken) => {
            // Release the DNS-class record and resolver scratch left on the source lease.
            admission.release(lease);
            Split::Covered(
                Delivery {
                    id: delivery,
                    lease: taken,
                },
                None,
            )
        }
        // Ledger or identity exhaustion leaves the source intact, so retain it as conservative delivery
        // coverage and report the denial.
        Err(denied) if denied.leaves_source_intact() => Split::Covered(
            Delivery {
                id: delivery,
                lease,
            },
            Some(denied),
        ),
        // Other denials do not prove coverage; return the debt for cleanup without delivery.
        Err(denied) => Split::Uncovered(QueryDebt { id, lease }, denied),
    }
}

/// Ends one delivery, after every buffer it covered has actually been dropped or acknowledged.
pub fn delivered(admission: &mut Admission, delivery: Delivery) {
    admission.release(delivery.lease);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::admission::{Headroom, Totals};

    // Output packetization peak used by shizuku::budget.
    const OUTPUT_PEAK: u64 = 2 * MAX_MESSAGE as u64;

    const MTU: u64 = 1_500;

    // Representative TUN writer queue; only fixed_io's classification matters.
    const WRITER_QUEUE: u64 = 64 * 1_024;

    const FLOW_BYTES: u64 = 4_096;
    const QUERY_BYTES: u64 = 65_535;
    const DELIVERY_BYTES: u64 = 65_535 + (65_535 + 2);
    const EXCHANGE_BYTES: u64 = QUERY_BYTES + DELIVERY_BYTES;

    fn totals() -> Totals {
        Totals {
            admission_id: 1,
            record_total: 200,
            dns_record_floor: 64,
            byte_total: 8 << 20,
            reserved_byte_floor: 1 << 20,
            fragment_cap: 1 << 20,
            byte_only_owners: 4,
        }
    }

    fn admission() -> Admission {
        Admission::new(totals()).expect("the fixture totals hold their own accounting")
    }

    fn settled(admission: &mut Admission, debt: QueryDebt, delivery_bytes: u64) -> Delivery {
        match settle(admission, debt, delivery_bytes) {
            Split::Covered(delivery, None) => delivery,
            Split::Covered(_, Some(denied)) => {
                panic!("a split with room to make it is refused by nothing: {denied:?}")
            }
            Split::Uncovered(_, denied) => {
                panic!("a split with room to make it describes its own grant: {denied:?}")
            }
        }
    }

    #[test]
    fn the_row_bound_is_derived_from_whichever_resource_binds() {
        let bytes_bound = admission();
        assert_eq!(
            rows(&bytes_bound),
            ((8u64 << 20) / MINIMUM_SUBMITTED_BYTES) as usize
        );
        assert!(rows(&bytes_bound) < bytes_bound.record_total() as usize);

        let records_bound = Admission::new(Totals {
            record_total: 10,
            dns_record_floor: 1,
            ..totals()
        })
        .expect("granted");
        assert_eq!(rows(&records_bound), 10);

        let generous = Admission::new(Totals {
            record_total: 4_096,
            byte_total: 1 << 30,
            ..totals()
        })
        .expect("granted");
        assert_eq!(rows(&generous), 4_096);
    }

    #[test]
    fn parked_answers_accumulate_against_records_without_the_ledger_binding_first() {
        let mut admission = Admission::new(Totals {
            record_total: 20,
            dns_record_floor: 1,
            ..totals()
        })
        .expect("granted");
        let ceiling = admission.general_record_ceiling() as usize;

        // Each blocked flow parks one byte-only delivery successor beside its flow lease.
        let mut flows = Vec::new();
        let mut parked = Vec::new();
        let refusal = loop {
            let flow = match admission.reserve(Request {
                records: 1,
                record_class: Class::General,
                bytes: FLOW_BYTES,
                byte_class: Class::General,
                ..Request::default()
            }) {
                Ok(flow) => flow,
                Err(why) => break why,
            };
            let debt = submit(&mut admission, flows.len() as u64, EXCHANGE_BYTES)
                .expect("a question on the descriptor the flow left");
            let answer = Settled::delivering(settled(&mut admission, debt, DELIVERY_BYTES), ());
            let mut slot = Parked::default();
            let parking = slot.park(&mut admission, answer);
            assert!(parking.answer.is_some() && !parking.replaced);
            flows.push(flow);
            parked.push(slot);
        };

        assert!(
            matches!(refusal, Denied::Records | Denied::Bytes),
            "the ledger is sized for these successors, so it is never what runs out: {refusal:?}"
        );
        assert_eq!(flows.len(), ceiling, "records are what ran out");
        assert!(
            admission.outstanding_leases() > admission.record_total() as usize,
            "byte-only successors outnumber the records, which is exactly the shape a ledger sized at one \
             row per record could not hold"
        );

        // A further query still admits; the ledger was not the bound.
        let debt = submit(&mut admission, u64::MAX, EXCHANGE_BYTES)
            .expect("records and bytes are what a query is refused for");
        abandon(&mut admission, debt);

        for mut slot in parked {
            slot.close(&mut admission);
        }
        for flow in flows {
            admission.release(flow);
        }
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn the_reserved_floor_holds_one_maximum_exchange_in_the_composed_startup_state() {
        const RECORDS: u32 = 200;
        const OWNERS: u32 = 4;
        // Match the production floor: ledger, one maximum exchange, and output peak.
        let ledger_bytes =
            Admission::ledger_bytes(Admission::ledger_slots(RECORDS, OWNERS).expect("fits"))
                .expect("fits");
        let mut admission = Admission::new(Totals {
            record_total: RECORDS,
            dns_record_floor: 1,
            byte_total: 8 << 20,
            reserved_byte_floor: ledger_bytes + MAXIMUM_SUBMITTED_BYTES + OUTPUT_PEAK,
            byte_only_owners: OWNERS,
            ..totals()
        })
        .expect("the derived floor holds its own accounting");

        // Charge the output peak as reserved.
        let output = admission
            .reserve(Request::bytes(OUTPUT_PEAK, Class::Reserved))
            .expect("the floor was sized for it");
        // Charge input and DNS storage through the production general-class helpers.
        let io = admission
            .reserve(fixed_io(WRITER_QUEUE, MTU).expect("fits"))
            .expect("granted");
        let fixed = tables(&mut admission, WRITER_QUEUE).expect("granted");

        let Headroom { records, bytes } = admission.general_headroom();
        let saturated = admission
            .reserve(Request {
                records,
                record_class: Class::General,
                bytes,
                byte_class: Class::General,
                ..Request::default()
            })
            .expect("general fills its ceiling");
        assert_eq!(
            admission.general_headroom(),
            Headroom {
                records: 0,
                bytes: 0
            }
        );

        let debt = submit(&mut admission, 1, MAXIMUM_SUBMITTED_BYTES)
            .expect("the reserved floor holds exactly this");
        let second = submit(&mut admission, 2, MAXIMUM_SUBMITTED_BYTES)
            .map(|_| ())
            .expect_err("the floor guarantees one exchange, not two");
        assert!(
            matches!(second, Denied::Records | Denied::Bytes),
            "{second:?}"
        );

        let delivery = settled(&mut admission, debt, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        admission.release(saturated);
        admission.release(fixed);
        admission.release(io);
        admission.release(output);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn admission_refuses_a_submitted_query_before_the_row_bound_does() {
        let mut admission = admission();
        let bound = rows(&admission);
        // Minimum-size exchanges reach the derived bound latest.
        let mut debts = Vec::new();
        loop {
            match submit(&mut admission, debts.len() as u64, MINIMUM_SUBMITTED_BYTES) {
                Ok(debt) => debts.push(debt),
                Err(why) => {
                    assert!(
                        matches!(why, Denied::Records | Denied::Bytes),
                        "records or bytes are what a query is refused for. A ledger row is a count, and a \
                         count is the hidden boundary this whole model exists to remove: {why:?}"
                    );
                    break;
                }
            }
            assert!(
                debts.len() <= bound,
                "the table is sized for every query the ledger can charge: {} > {bound}",
                debts.len()
            );
        }
        assert!(
            !debts.is_empty() && debts.len() <= bound,
            "{} charged, bound {bound}",
            debts.len()
        );

        for debt in debts {
            abandon(&mut admission, debt);
        }
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_saturated_general_share_still_admits_one_maximum_exchange() {
        let mut admission = admission();
        // Choose a size that would break the maximum-exchange guarantee if misclassified.
        let table_bytes =
            admission.byte_total() - admission.general_byte_ceiling() - EXCHANGE_BYTES;
        let tables = tables(&mut admission, table_bytes).expect("the fixed DNS tables");

        let Headroom { records, bytes } = admission.general_headroom();
        let saturated = admission
            .reserve(Request {
                records,
                record_class: Class::General,
                bytes,
                byte_class: Class::General,
                ..Request::default()
            })
            .expect("general fills its ceiling");
        assert_eq!(
            admission.general_headroom(),
            Headroom {
                records: 0,
                bytes: 0
            }
        );

        let debt = submit(&mut admission, 1, EXCHANGE_BYTES)
            .expect("the DNS record floor and the essential byte floor are still there");
        let delivery = settled(&mut admission, debt, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        admission.release(saturated);
        admission.release(tables);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_query_that_cannot_have_its_whole_exchange_charges_nothing_at_all() {
        let mut admission = admission();
        let flow = admission
            .reserve(Request {
                records: 1,
                record_class: Class::General,
                bytes: FLOW_BYTES,
                byte_class: Class::General,
                ..Request::default()
            })
            .expect("granted");
        let charged = (admission.records_charged(), admission.bytes_charged());

        // Saturate records; submission must fail atomically.
        let held = admission
            .reserve(Request::records(
                admission.record_total() - admission.records_charged(),
                Class::Reserved,
            ))
            .expect("every record that is left");
        assert_eq!(
            submit(&mut admission, 5, EXCHANGE_BYTES).map(|_| ()),
            Err(Denied::Records)
        );
        admission.release(held);

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
        admission.release(flow);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn abandoning_a_query_locally_gives_its_whole_debt_back() {
        let mut admission = admission();
        let empty = (admission.records_charged(), admission.bytes_charged());

        // Abandon models local teardown, not resolver EBUSY, which settles normally.
        let debt = submit(&mut admission, 1, EXCHANGE_BYTES).expect("granted");
        assert_eq!(admission.records_charged(), empty.0 + 1);
        abandon(&mut admission, debt);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            empty,
            "the query's record and every byte of it went"
        );
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_split_whose_request_does_not_describe_its_grant_covers_nothing() {
        let mut admission = admission();
        let mut other = Admission::new(Totals {
            admission_id: 99,
            ..totals()
        })
        .expect("granted");
        let other_empty = other.bytes_charged();

        // A foreign debt has no delivery coverage in this admission.
        let foreign = submit(&mut other, 1, EXCHANGE_BYTES).expect("granted");
        let Split::Uncovered(debt, denied) = settle(&mut admission, foreign, DELIVERY_BYTES) else {
            panic!("a grant this admission does not hold covers nothing");
        };
        assert_eq!(denied, Denied::Unknown);
        assert_eq!(
            admission.invariant_violations(),
            1,
            "exactly the one the attempted split made"
        );
        // Release through the admission that issued the debt.
        abandon(&mut other, debt);
        assert_eq!(other.records_charged(), 0);
        assert_eq!(other.bytes_charged(), other_empty);
        assert_eq!(other.invariant_violations(), 0);

        // A delivery larger than its source grant is likewise uncovered.
        let violations = admission.invariant_violations();
        let debt = submit(&mut admission, 2, EXCHANGE_BYTES).expect("granted");
        let Split::Uncovered(debt, denied) = settle(&mut admission, debt, EXCHANGE_BYTES + 1)
        else {
            panic!("a delivery larger than its grant covers nothing");
        };
        assert_eq!(denied, Denied::Arithmetic);
        assert_eq!(admission.invariant_violations(), violations + 1);
        abandon(&mut admission, debt);
        assert_eq!(admission.records_charged(), 0);
    }

    #[test]
    fn a_ledger_that_ran_out_still_reports_the_split_it_refused() {
        let mut full = Admission::new(Totals {
            record_total: 2,
            dns_record_floor: 0,
            byte_only_owners: 0,
            ..totals()
        })
        .expect("granted");
        let debt = submit(&mut full, 1, EXCHANGE_BYTES).expect("granted");
        let mut held = Vec::new();
        while let Ok(lease) = full.reserve(Request::bytes(1, Class::General)) {
            held.push(lease);
        }

        // Ledger exhaustion leaves the source intact as conservative coverage; report the denial.
        let Split::Covered(delivery, denied) = settle(&mut full, debt, DELIVERY_BYTES) else {
            panic!("an intact source grant covers its delivery");
        };
        assert_eq!(
            denied,
            Some(Denied::Ledger),
            "silence is not the contract for a ledger that ran out"
        );
        delivered(&mut full, delivery);
        for lease in held {
            full.release(lease);
        }
        assert_eq!(full.records_charged(), 0);
        assert_eq!(full.invariant_violations(), 0);
    }

    #[test]
    fn a_settled_query_cannot_alter_capacity_twice() {
        let mut admission = admission();
        let before = (admission.records_charged(), admission.bytes_charged());
        let debt = submit(&mut admission, 3, EXCHANGE_BYTES).expect("granted");
        let delivery = settled(&mut admission, debt, DELIVERY_BYTES);
        delivered(&mut admission, delivery);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            before
        );
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_parked_delivery_is_released_by_its_own_acknowledgment_and_only_once() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let debt = submit(&mut admission, 17, EXCHANGE_BYTES).expect("granted");
        let delivery = Settled::delivering(settled(&mut admission, debt, DELIVERY_BYTES), ());
        let mut parked = Parked::default();

        let parking = parked.park(&mut admission, delivery);
        let (acked, ()) = parking.answer.expect("an answer to deliver");
        assert!(!parking.replaced);
        assert_eq!(admission.bytes_charged(), empty + DELIVERY_BYTES);

        assert_eq!(parked.acknowledge(&mut admission, acked), Acked::Released);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(parked.acknowledge(&mut admission, acked), Acked::Absent);
        parked.close(&mut admission);
        assert_eq!(admission.bytes_charged(), empty);
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
        let delivery = settled(&mut admission, debt, DELIVERY_BYTES);
        assert!(admission.granted(delivery.lease()).expect("held").bytes >= peak);
        delivered(&mut admission, delivery);
    }

    #[test]
    fn a_lost_consumer_refunds_once_and_a_stale_terminal_adds_nothing() {
        let mut admission = admission();
        let empty = admission.bytes_charged();
        let debt = submit(&mut admission, 13, EXCHANGE_BYTES).expect("granted");
        let delivery = settled(&mut admission, debt, DELIVERY_BYTES);

        delivered(&mut admission, delivery);
        assert_eq!(admission.bytes_charged(), empty);
        assert_eq!(admission.invariant_violations(), 0);

        let mut other = Admission::new(Totals {
            admission_id: 99,
            ..totals()
        })
        .expect("granted");
        let foreign = submit(&mut other, 1, EXCHANGE_BYTES).expect("granted");
        let foreign = settled(&mut other, foreign, DELIVERY_BYTES);
        let charged = (admission.records_charged(), admission.bytes_charged());
        delivered(&mut admission, foreign);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            charged,
            "a stale delivery creates no capacity"
        );
        assert_eq!(admission.invariant_violations(), 1);
    }
}
