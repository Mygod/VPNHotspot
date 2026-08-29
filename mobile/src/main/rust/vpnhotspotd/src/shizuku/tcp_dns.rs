//! DNS-over-TCP transport whose submitted resolver transactions outlive transport retirement.
use std::io;

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::dns_wire::{self, frame, Body, DnsStream, Framed};
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::preempt::{hand_over, shutdown, write_all, Written};

use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::bridge::Worker;
use vpnhotspotd::shared::dns_debt::{self, DeliveryId, Parked};

use crate::shizuku::budget::MAX_DATAGRAM;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_flow::Event;
use vpnhotspotd::shared::flow_budget::READ_CHUNK;
use vpnhotspotd::shared::workers::Ended;

mod transactions;

pub(crate) use transactions::{Reserved, Settlement, Submitted, Transactions};

pub(crate) struct Resolved {
    result: Result<Owned, Failure>,
    message: Option<Owned>,
}

impl Resolved {
    pub(crate) fn new(result: Result<Owned, Failure>, message: Option<Owned>) -> Self {
        Self { result, message }
    }
}

pub(crate) struct Deliverable {
    answer: Owned,
    message: Option<Owned>,
}

fn classify(resolved: Resolved, ending: &mut Option<Failure>) -> Option<Deliverable> {
    let Resolved { result, message } = resolved;
    match result {
        Ok(answer) => Some(Deliverable { answer, message }),
        Err(failure) => {
            drop(message);
            *ending = Some(failure);
            None
        }
    }
}

pub(crate) struct Answering {
    settled: dns_debt::Settled<Resolved>,
}

pub(crate) struct Delivered {
    answering: Answering,
    stamp: crate::shizuku::tun_writer::Stamp,
    network: vpnhotspotd::shared::model::Network,
}

pub(crate) enum Answered {
    Delivered { delivery: DeliveryId, result: Owned },
    Refused(Failure),
}

pub(crate) enum Control {
    Granted(Granted),
    Answered(Answered),
}

impl Answering {
    pub(crate) fn hand_over(self, admission: &mut Admission, serving: &mut Serving) -> bool {
        let Self { settled } = self;
        let mut ending = None;
        let settled = settled.classify(|resolved| classify(resolved, &mut ending));
        let parking = serving.delivery.park(admission, settled);
        match parking.answer {
            Some((delivery, deliverable)) => {
                let Deliverable { answer, message } = deliverable;
                drop(message);
                serving.answer(Answered::Delivered {
                    delivery,
                    result: answer,
                });
            }
            None => {
                if let Some(failure) = ending {
                    serving.answer(Answered::Refused(failure));
                }
            }
        }
        parking.replaced
    }

    pub(crate) fn discard(self, admission: &mut Admission) {
        self.settled.discard(admission);
    }
}

impl Delivered {
    pub(crate) fn new(
        settled: dns_debt::Settled<Resolved>,
        stamp: crate::shizuku::tun_writer::Stamp,
        network: vpnhotspotd::shared::model::Network,
    ) -> Self {
        Self {
            answering: Answering { settled },
            stamp,
            network,
        }
    }

    pub(crate) fn has_answer(&self) -> bool {
        self.answering.settled.has_answer()
    }

    pub(crate) fn stamp(&self) -> crate::shizuku::tun_writer::Stamp {
        self.stamp
    }

    pub(crate) fn network(&self) -> vpnhotspotd::shared::model::Network {
        self.network
    }

    pub(crate) fn stale(&mut self) -> bool {
        self.answering.settled.replace_answer(|resolved| {
            let Resolved { result, message } = resolved;
            drop(result);
            let message = message?;
            let servfail = dns_wire::servfail_response(&message).map(Owned::new);
            servfail.map(|servfail| Resolved {
                result: Ok(servfail),
                message: Some(message),
            })
        })
    }

    pub(crate) fn answering(self) -> Answering {
        self.answering
    }

    pub(crate) fn discard(self, admission: &mut Admission) {
        self.answering.discard(admission);
    }
}

pub(crate) const fn delivery_bytes(answer: usize) -> u64 {
    2 * answer as u64 + dns_wire::PREFIX as u64
}

pub(crate) const DELIVERY_BYTES: u64 = delivery_bytes(MAX_DATAGRAM);

pub(crate) fn exchange_bytes(length: usize) -> Option<u64> {
    (length as u64).checked_add(DELIVERY_BYTES)
}

pub(crate) fn answered_here_bytes(length: usize) -> Option<u64> {
    (length as u64).checked_add(delivery_bytes(length))
}

pub(crate) struct Serving {
    control: mpsc::Sender<Control>,
    filled: mpsc::Receiver<Owned>,
    reserved: Option<Reserved>,
    delivery: Parked,
    transaction: Option<u64>,
    unreachable: u64,
}

impl Serving {
    pub(crate) fn new(control: mpsc::Sender<Control>, filled: mpsc::Receiver<Owned>) -> Self {
        Self {
            control,
            filled,
            reserved: None,
            delivery: Parked::default(),
            transaction: None,
            unreachable: 0,
        }
    }

    pub(crate) fn reserving(&self) -> bool {
        self.reserved.is_some()
    }

    pub(crate) fn reserve(&mut self, reserved: Reserved) {
        self.reserved = Some(reserved);
    }

    pub(crate) fn accept(&mut self) -> Option<(Reserved, Option<Owned>)> {
        let reserved = self.reserved.take()?;
        Some((reserved, self.filled.try_recv().ok()))
    }

    fn send(&mut self, control: Control) {
        if self.control.try_send(control).is_err() {
            self.unreachable += 1;
        }
    }

    pub(crate) fn grant(&mut self, granted: Granted) {
        self.send(Control::Granted(granted));
    }

    fn answer(&mut self, answered: Answered) {
        self.send(Control::Answered(answered));
    }

    pub(crate) fn transaction(&self) -> Option<u64> {
        self.transaction
    }

    pub(crate) fn asking(&mut self, transaction: Option<u64>) {
        self.transaction = transaction;
    }

    pub(crate) fn acknowledge(
        &mut self,
        admission: &mut Admission,
        acked: DeliveryId,
    ) -> dns_debt::Acked {
        self.delivery.acknowledge(admission, acked)
    }

    pub(crate) fn close(self, admission: &mut Admission) -> Option<u64> {
        let Self {
            control,
            mut filled,
            reserved,
            mut delivery,
            transaction,
            ..
        } = self;
        delivery.close(admission);
        filled.close();
        while filled.try_recv().is_ok() {}
        drop(filled);
        drop(control);
        if let Some(reserved) = reserved {
            reserved.end(admission);
        }
        transaction
    }
}

pub(crate) fn answered_here(
    reserved: Reserved,
    query: Owned,
    serving: &mut Serving,
    admission: &mut Admission,
) -> Option<Answering> {
    let servfail = dns_wire::servfail_response(&query).map(Owned::new);
    drop(query);
    let Some(servfail) = servfail else {
        serving.answer(Answered::Refused(Failure::Expected(io::Error::other(
            "a DNS-over-TCP query too malformed to answer",
        ))));
        reserved.end(admission);
        return None;
    };
    let delivery = reserved.settle(admission, delivery_bytes(servfail.capacity()));
    Some(Answering {
        settled: dns_debt::Settled::delivering(
            delivery,
            Resolved {
                result: Ok(servfail),
                message: None,
            },
        ),
    })
}

pub(crate) enum Ask {
    Reserve { flow: Event, length: usize },
    Query(Event),
    Delivered { flow: Event, delivery: DeliveryId },
}

pub(crate) enum Granted {
    Admitted(Owned),
    Denied,
}

pub(crate) async fn serve(
    flow: Event,
    mut bridge: Worker,
    asks: mpsc::Sender<Ask>,
    mut control: mpsc::Receiver<Control>,
    filled: mpsc::Sender<Owned>,
    cancel: CancellationToken,
) -> Ended {
    let mut stream = DnsStream::default();
    let mut filling: Option<Owned> = None;
    let mut scratch = vec![0u8; READ_CHUNK];
    loop {
        let read = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ended::Expected,
            read = bridge.read(&mut scratch) => read,
        };
        let Ok(read) = read else {
            return Ended::Expected;
        };
        if read == 0 {
            if stream.partial() {
                return Ended::Reported("DNS-over-TCP request ended mid-message".to_owned());
            }
            match shutdown(&mut bridge, &cancel).await {
                Written::Done | Written::Cancelled => return Ended::Expected,
                Written::Failed(error) => {
                    return Ended::Failed {
                        context: "shizuku.tcp_dns_shutdown",
                        error,
                    }
                }
            }
        }
        let mut rest = &scratch[..read];
        loop {
            match stream.advance(
                &mut rest,
                filling.as_mut().map(|query| query as &mut dyn Body),
            ) {
                Framed::Hungry => break,
                Framed::Broken => {
                    return Ended::Reported(
                        "a DNS-over-TCP length this stream cannot resynchronize after".to_owned(),
                    )
                }
                Framed::Length(length) => {
                    // Admit the announced body before allocating or storing any of it.
                    if !hand_over(&asks, Ask::Reserve { flow, length }, &cancel).await {
                        return Ended::Expected;
                    }
                    let admitted = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ended::Expected,
                        admitted = control.recv() => admitted,
                    };
                    filling = match admitted {
                        Some(Control::Granted(Granted::Admitted(query))) => Some(query),
                        Some(Control::Granted(Granted::Denied)) => None,
                        Some(Control::Answered(_)) | None => return Ended::Expected,
                    };
                }
                Framed::Message => {
                    let Some(query) = filling.take() else {
                        continue;
                    };
                    if filled.try_send(query).is_err() {
                        return Ended::Expected;
                    }
                    if !hand_over(&asks, Ask::Query(flow), &cancel).await {
                        return Ended::Expected;
                    }
                    let delivered = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ended::Expected,
                        delivered = control.recv() => delivered,
                    };
                    let (delivery, answer) = match delivered {
                        Some(Control::Answered(Answered::Delivered { delivery, result })) => {
                            (delivery, result)
                        }
                        Some(Control::Answered(Answered::Refused(failure))) => {
                            return failure.ended("resolver query")
                        }
                        Some(Control::Granted(_)) | None => return Ended::Expected,
                    };
                    let Some(framed) = frame(&answer).map(Owned::new) else {
                        return Ended::Reported("resolver answer exceeds a DNS message".to_owned());
                    };
                    let written = write_all(&mut bridge, &framed, &cancel).await;
                    drop(framed);
                    drop(answer);
                    match written {
                        Written::Done => {}
                        Written::Cancelled => return Ended::Expected,
                        Written::Failed(error) => {
                            return Ended::Failed {
                                context: "shizuku.tcp_dns_deliver",
                                error,
                            }
                        }
                    }
                    // Release delivery accounting only after the complete framed answer reached the bridge.
                    if !hand_over(&asks, Ask::Delivered { flow, delivery }, &cancel).await {
                        return Ended::Expected;
                    }
                }
            }
        }
    }
}
