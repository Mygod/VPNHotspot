//! DNS delivery identity from resolver submission through transport acknowledgement.

#[derive(Debug)]
pub struct QueryDebt {
    id: u64,
}

pub fn submit(id: u64) -> QueryDebt {
    QueryDebt { id }
}

/// Delivery identity retained after resolver settlement.
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

/// Creates the identity used to match a later delivery acknowledgment.
pub fn settle(debt: QueryDebt) -> Delivery {
    let QueryDebt { id } = debt;
    Delivery { id: DeliveryId(id) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_identity_survives_until_delivery_acknowledgement() {
        let delivery = settle(submit(7));

        let mut parked = Parked::default();
        let parking = parked.park(Settled::delivering(delivery, "answer"));
        let (id, answer) = parking.answer.expect("deliverable");
        assert_eq!(answer, "answer");
        assert_eq!(parked.acknowledge(id), Acked::Released);
        assert_eq!(parked.acknowledge(id), Acked::Absent);
    }

    #[test]
    fn parking_replacement_and_identity_matching_are_structural() {
        let first = settle(submit(1));
        let second = settle(submit(2));
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
