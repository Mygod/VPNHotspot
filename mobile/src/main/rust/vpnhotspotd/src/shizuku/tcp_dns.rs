//! DNS-over-TCP transport.
//!
//! Submitted transactions are table-owned and may outlive their flow; results go only to the exact
//! originating flow identity. Admission refusals return SERVFAIL, while invalid messages and transport
//! failures end the stream.
use std::io;

use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::dns_wire::{self, frame, Body, DnsStream, Framed, Refused};
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
    pub(crate) fn new(settled: dns_debt::Settled<Resolved>) -> Self {
        Self {
            answering: Answering { settled },
        }
    }

    pub(crate) fn has_answer(&self) -> bool {
        self.answering.settled.has_answer()
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

// Keep transaction-row sizing and the essential DNS floor aligned with this charge.
const _: () = assert!(DELIVERY_BYTES >= dns_debt::MINIMUM_SUBMITTED_BYTES);

const _: () = assert!(MAX_DATAGRAM as u64 + DELIVERY_BYTES == dns_debt::MAXIMUM_SUBMITTED_BYTES);

pub(crate) fn exchange_bytes(length: usize) -> Option<u64> {
    (length as u64).checked_add(DELIVERY_BYTES)
}

pub(crate) struct Serving {
    control: mpsc::Sender<Control>,
    filled: mpsc::Receiver<Owned>,
    reserved: Option<Reserved>,
    delivery: Parked,
    unreachable: u64,
}

impl Serving {
    pub(crate) fn new(control: mpsc::Sender<Control>, filled: mpsc::Receiver<Owned>) -> Self {
        Self {
            control,
            filled,
            reserved: None,
            delivery: Parked::default(),
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

    pub(crate) fn acknowledge(
        &mut self,
        admission: &mut Admission,
        acked: DeliveryId,
    ) -> dns_debt::Acked {
        self.delivery.acknowledge(admission, acked)
    }

    /// Releases only DNS state still owned by this flow; submitted transactions remain table-owned.
    pub(crate) fn close(self, admission: &mut Admission) {
        let Self {
            control,
            mut filled,
            reserved,
            mut delivery,
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
    }
}

/// Builds SERVFAIL for an unsubmitted query. Invalid input returns `None`; an uncovered delivery grant is an
/// error.
pub(crate) fn answered_here(
    reserved: Reserved,
    query: Owned,
    serving: &mut Serving,
    admission: &mut Admission,
) -> io::Result<Option<Answering>> {
    let servfail = dns_wire::servfail_response(&query).map(Owned::new);
    drop(query);
    let Some(servfail) = servfail else {
        serving.answer(Answered::Refused(Failure::Expected(io::Error::other(
            "a DNS-over-TCP message that is not an answerable query",
        ))));
        reserved.end(admission);
        return Ok(None);
    };
    let transaction = reserved.id();
    let delivery = match reserved.settle(admission, delivery_bytes(servfail.capacity())) {
        dns_debt::Split::Covered(delivery, denied) => {
            transactions::report_split(transaction, denied);
            delivery
        }
        dns_debt::Split::Uncovered(debt, denied) => {
            // Drop the answer before abandoning the grant that should cover it.
            drop(servfail);
            dns_debt::abandon(admission, debt);
            return Err(transactions::uncovered(transaction, denied));
        }
    };
    Ok(Some(Answering {
        settled: dns_debt::Settled::delivering(
            delivery,
            Resolved {
                result: Ok(servfail),
                message: None,
            },
        ),
    }))
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

/// Storage for the current framed body.
enum Filling {
    /// Fully admitted body buffer.
    Admitted(Owned),
    /// Header-only sink used to drain a refused body and build SERVFAIL.
    Refused(Refused),
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
    let mut filling: Option<Filling> = None;
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
                filling.as_mut().map(|filling| match filling {
                    Filling::Admitted(query) => query as &mut dyn Body,
                    Filling::Refused(refused) => refused as &mut dyn Body,
                }),
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
                        Some(Control::Granted(Granted::Admitted(query))) => {
                            Some(Filling::Admitted(query))
                        }
                        // Drain the refused body while retaining enough header state to return SERVFAIL.
                        Some(Control::Granted(Granted::Denied)) => {
                            Some(Filling::Refused(Refused::default()))
                        }
                        Some(Control::Answered(_)) | None => return Ended::Expected,
                    };
                }
                Framed::Message => {
                    let query =
                        match filling.take() {
                            Some(Filling::Admitted(query)) => query,
                            Some(Filling::Refused(refused)) => {
                                // Build SERVFAIL from the fixed header sink without allocating or echoing the
                                // query.
                                let Some(framed) = refused.framed_servfail() else {
                                    // Invalid DNS input cannot be refused with SERVFAIL.
                                    return Ended::Reported(
                                        "a DNS-over-TCP message that is not an answerable query"
                                            .to_owned(),
                                    );
                                };
                                match write_all(&mut bridge, &framed, &cancel).await {
                                    Written::Done => continue,
                                    Written::Cancelled => return Ended::Expected,
                                    Written::Failed(error) => {
                                        return Ended::Failed {
                                            context: "shizuku.tcp_dns_refuse",
                                            error,
                                        }
                                    }
                                }
                            }
                            // Every reservation must install a sink; otherwise consuming it silently drops the
                            // query.
                            None => return Ended::Reported(
                                "a DNS-over-TCP message was framed with no sink admitted for it"
                                    .to_owned(),
                            ),
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
