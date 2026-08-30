//! DNS descriptor ownership from query admission through resolver settlement.
//!
//! A submitted query owns one DNS-class descriptor unit while the platform resolver may retain a returned
//! descriptor. Once its terminal arrives, descriptor debt ends immediately. Answer parking has no separate
//! admission lifetime because it owns ordinary userspace buffers rather than descriptors.
use crate::shared::admission::{Admission, Class, Denied, Lease};

#[derive(Debug)]
pub struct QueryDebt {
    id: u64,
    lease: Lease,
}

/// Admits one submitted query under the DNS descriptor class.
pub fn submit(admission: &mut Admission, id: u64) -> Result<QueryDebt, Denied> {
    let lease = admission.reserve(Class::Reserved)?;
    Ok(QueryDebt { id, lease })
}

/// Ends a query before its resolver terminal is consumed.
pub fn abandon(admission: &mut Admission, debt: QueryDebt) {
    admission.release(debt.lease);
}

/// Delivery identity retained after resolver descriptor debt has ended.
#[derive(Debug)]
pub struct Delivery {
    id: DeliveryId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryId(u64);

#[derive(Debug, PartialEq, Eq)]
pub enum Acked {
    Released,
    Mismatched,
    Absent,
}

#[derive(Debug)]
pub struct Settled<R> {
    delivery: Delivery,
    answer: Option<R>,
}

impl<R> Settled<R> {
    pub fn delivering(delivery: Delivery, answer: R) -> Self {
        Self {
            delivery,
            answer: Some(answer),
        }
    }

    pub fn has_answer(&self) -> bool {
        self.answer.is_some()
    }

    pub fn replace_answer(&mut self, replace: impl FnOnce(R) -> Option<R>) -> bool {
        let Some(answer) = self.answer.take() else {
            return false;
        };
        self.answer = replace(answer);
        self.answer.is_some()
    }

    pub fn classify<S>(self, classify: impl FnOnce(R) -> Option<S>) -> Settled<S> {
        let Self { delivery, answer } = self;
        Settled {
            delivery,
            answer: answer.and_then(classify),
        }
    }

    pub fn discard(self) {
        drop(self);
    }
}

#[derive(Debug)]
pub struct Parking<R> {
    pub answer: Option<(DeliveryId, R)>,
    pub replaced: bool,
}

#[derive(Debug, Default)]
pub struct Parked {
    delivery: Option<Delivery>,
}

impl Parked {
    pub fn park<R>(&mut self, settled: Settled<R>) -> Parking<R> {
        let Settled { delivery, answer } = settled;
        let Some(answer) = answer else {
            return Parking {
                answer: None,
                replaced: false,
            };
        };
        let id = delivery.id;
        let replaced = self.delivery.replace(delivery).is_some();
        Parking {
            answer: Some((id, answer)),
            replaced,
        }
    }

    pub fn acknowledge(&mut self, acked: DeliveryId) -> Acked {
        match &self.delivery {
            Some(delivery) if delivery.id == acked => {
                self.delivery.take();
                Acked::Released
            }
            Some(_) => Acked::Mismatched,
            None => Acked::Absent,
        }
    }

    pub fn close(&mut self) {
        self.delivery.take();
    }
}

/// Ends resolver descriptor debt and creates the identity used to match a later delivery acknowledgment.
pub fn settle(admission: &mut Admission, debt: QueryDebt) -> Delivery {
    let QueryDebt { id, lease } = debt;
    admission.release(lease);
    Delivery { id: DeliveryId(id) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::admission::Totals;

    fn admission() -> Admission {
        Admission::new(Totals {
            admission_id: 1,
            descriptor_total: 3,
            dns_descriptor_floor: 1,
        })
        .expect("valid totals")
    }

    #[test]
    fn query_debt_owns_a_descriptor_until_settlement() {
        let mut admission = admission();
        let debt = submit(&mut admission, 7).expect("DNS descriptor");
        assert_eq!(admission.descriptors_charged(), 1);
        let delivery = settle(&mut admission, debt);
        assert_eq!(admission.descriptors_charged(), 0);

        let mut parked = Parked::default();
        let parking = parked.park(Settled::delivering(delivery, "answer"));
        let (id, answer) = parking.answer.expect("deliverable");
        assert_eq!(answer, "answer");
        assert_eq!(parked.acknowledge(id), Acked::Released);
        assert_eq!(parked.acknowledge(id), Acked::Absent);
    }

    #[test]
    fn general_work_cannot_consume_the_dns_floor() {
        let mut admission = admission();
        let first = admission
            .reserve(Class::General)
            .expect("first general descriptor");
        let second = admission
            .reserve(Class::General)
            .expect("second general descriptor");
        let debt = submit(&mut admission, 1).expect("reserved DNS descriptor");
        assert_eq!(admission.descriptors_charged(), 3);
        abandon(&mut admission, debt);
        admission.release(first);
        admission.release(second);
    }

    #[test]
    fn parking_replacement_and_identity_matching_are_structural() {
        let mut admission = admission();
        let first = submit(&mut admission, 1).expect("first");
        let first = settle(&mut admission, first);
        let second = submit(&mut admission, 2).expect("second");
        let second = settle(&mut admission, second);
        let mut parked = Parked::default();
        let first = parked
            .park(Settled::delivering(first, 1))
            .answer
            .expect("first")
            .0;
        let replacement = parked.park(Settled::delivering(second, 2));
        assert!(replacement.replaced);
        let second = replacement.answer.expect("second").0;
        assert_eq!(parked.acknowledge(first), Acked::Mismatched);
        assert_eq!(parked.acknowledge(second), Acked::Released);
    }
}
