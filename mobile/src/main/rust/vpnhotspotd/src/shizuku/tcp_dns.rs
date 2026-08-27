//! One DNS-over-TCP flow, answered by the platform resolver instead of by an upstream connection.
//!
//! The transport is the same shape as [crate::shizuku::tcp_flow]'s splice and deliberately interchangeable with it: the
//! engine opens a flow the same way, hands over the same receiver, and gets back the same events and the same
//! terminal. What differs is only where the answers come from - which is the whole point of the virtual-DNS
//! design, since a query answered here keeps private DNS, caching and per-network resolver configuration
//! instead of reimplementing them.
//!
//! Framing is RFC 1035 section 4.2.2's two-byte length prefix, and it is why this cannot simply be spliced: a
//! stream carries a sequence of messages rather than bytes to forward, and each has to be whole before the
//! resolver can be asked about it. The framing itself is [vpnhotspotd::shared::dns_wire]'s, which is where
//! the attacker-facing half of this flow can be answered for without a device.
//!
//! **The length is admitted before the message is stored.** The prefix is parsed first, its length is handed
//! to the ingress owner, and only a granted buffer of exactly that length is filled. Nothing is copied, grown
//! or handed to the platform before that grant exists, so the largest message a client can announce is an
//! allocation the aggregate agreed to rather than one it is told about afterwards. A query the descriptor
//! floor has no room for is still answered - a SERVFAIL costs the answer and no descriptor at all - and only
//! one whose bytes do not fit is skipped, which leaves the stream framed for the next question.
//!
//! **The selected network and the config a query belongs to are fixed at the owner's acceptance**, not at the
//! flow's. A virtual-DNS transport owns no selected-network socket, so a generation change does not retire
//! it (see [crate::shizuku::tcp]'s retirement split), and the same connection therefore outlives the selection its
//! earlier questions went out on. Each query carries the stamp and the handle current when the owner accepted
//! *it*, and settlement classifies the answer against those. A brand-new transport does not need a selected
//! network to *open* either: it holds no socket bound to one, so it is admitted with none and its questions
//! get their own SERVFAIL until a successor config supplies one - see [crate::shizuku::tcp]'s flow source.
//!
//! A resolver outcome is not a stream outcome. Everything the platform answers - a refusal, a timeout, its own
//! per-UID limiter, a name that does not resolve - is what one query the client chose to send answers, so it
//! becomes a SERVFAIL for that message and the connection carries on. So does an answer resolved on a
//! selection the session has since left, and so does a query there was no selected network for. Only this
//! daemon's own wrapper failing, a query too malformed to answer at all, or framing that can never
//! resynchronize ends the flow.
//!
//! **The transaction is owned apart from the transport, and it outlives it.** A retirement has to be abortive -
//! the config acknowledgement waits for it - while a submitted resolver transaction must not be cancelled to
//! reclaim capacity: cancelling returns this process's descriptor and nothing of the resolver's work, so the
//! platform's own per-UID slot stays taken either way, and cancelling would also destroy the completion that
//! made the debt exact. The two requirements only fit if the transaction is not part of the transport's
//! lifetime. So a swept transport's question keeps running in the ingress owner's own transaction table -
//! see [transactions] - and its answer is then discarded, because the client it was for has been reset. That
//! is a lifetime, not a task: the table is a prepared map the owner polls, which is what keeps a transaction
//! from costing a spawn, a token and three oneshots nothing charged for.
//!
//! Queries are answered one at a time. A resolver that pipelines would be faster, but the reserved capacity is
//! one slot per flow - see the engine - so a second concurrent query would be one this flow never paid for.
//!
//! https://www.rfc-editor.org/rfc/rfc1035#section-4.2.2

use std::io;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::dns_wire::{self, frame, Body, DnsStream, Framed};
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::preempt::hand_over;

use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::dns_debt::{self, DeliveryId, Parked};

use crate::shizuku::budget::MAX_DATAGRAM;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_flow::{hand_over_in_pieces, Chunk, Event, Handed, Mailbox};
use crate::shizuku::workers::Ended;
use vpnhotspotd::shared::flow_budget::READ_CHUNK;

mod transactions;

pub(crate) use transactions::{Reserved, Settlement, Submitted, Transactions};

/// What the resolver produced for one question, and the question itself.
///
/// The query comes back with the answer rather than dying at the terminal, because settlement may need it. A
/// stale-generation answer is discarded and replaced by that query's own SERVFAIL, which cannot be built
/// without it - and building it at the owner is what keeps the decision beside the config that drives it.
pub(crate) struct Resolved {
    result: Result<Owned, Failure>,
    /// The message as it was framed out of the client's stream. Absent for an answer this daemon built
    /// itself, where the query has already been built from and dropped - see [answered_here].
    message: Option<Owned>,
}

impl Resolved {
    pub(crate) fn new(result: Result<Owned, Failure>, message: Option<Owned>) -> Self {
        Self { result, message }
    }
}

/// One answer that is really going to a client, and the query it was built from.
///
/// The type is the classification. A [Resolved] may be a failure; this cannot, and the only way to get one is
/// [classify], which runs *before* anything is parked. That is what stops an unacknowledgeable value from
/// taking a delivery slot: a delivery is a lease its consumer gives back by naming it, so parking something
/// nobody will ever name is a grant that can only end when the whole flow closes.
pub(crate) struct Deliverable {
    answer: Owned,
    /// Dropped at the handoff, inside the grant that covered it, before the answer goes anywhere.
    message: Option<Owned>,
}

/// What one settled result really is, decided before a delivery slot is taken for it.
///
/// `ending` collects the one outcome that has no answer *and* has to be said out loud: this daemon's own
/// wrapper failing, or a query too malformed for even a SERVFAIL. Both end the flow, and neither parks
/// anything.
fn classify(resolved: Resolved, ending: &mut Option<Failure>) -> Option<Deliverable> {
    let Resolved { result, message } = resolved;
    match result {
        Ok(answer) => Some(Deliverable { answer, message }),
        Err(failure) => {
            // The query dies here, before the failure travels: nothing downstream needs it, and holding it
            // across the report would be a buffer alive for no reason inside a grant about to be released.
            drop(message);
            *ending = Some(failure);
            None
        }
    }
}

/// One settled answer on its way to the transport that asked for it.
///
/// The answer is inside [dns_debt::Settled] and the only way out is [Answering::hand_over], which classifies
/// and then parks. That is what makes the wrong order unspellable rather than merely unwritten.
pub(crate) struct Answering {
    settled: dns_debt::Settled<Resolved>,
}

/// One settled *transaction*, and what the engine needs to classify it against the config current now.
pub(crate) struct Delivered {
    answering: Answering,
    /// Carried out of settlement so the engine can classify this answer against the config current now.
    stamp: crate::shizuku::tun_writer::Stamp,
    flow: Event,
    network: vpnhotspotd::shared::model::Network,
    /// Whether this process lost the ability to watch a transaction the platform had accepted. The table has
    /// already moved whatever token it was holding; what is left for the engine is the token a *live*
    /// transport is still holding, and ending that transport.
    unobservable: bool,
}

/// What the transport receives back from its owner.
///
/// Two shapes rather than one with a sentinel identity, because a refusal has no delivery: nothing was
/// parked, nothing will be acknowledged, and a `DeliveryId` that merely *looks* invalid is one an
/// acknowledgment path has to be trusted to recognise. Making it unrepresentable is cheaper than checking.
pub(crate) enum Answered {
    /// An answer, and the exact delivery it must name when it acknowledges. Also how a SERVFAIL arrives,
    /// whoever built it: what ends a stream is never an answer, so a client that is told to try again keeps
    /// the connection it would have to reopen.
    Delivered { delivery: DeliveryId, result: Owned },
    /// There is no answer and there never will be: this daemon's own wrapper around the transaction failed,
    /// the query was too malformed for anything to be echoed back, or the platform kept a slot this process
    /// can no longer watch. There is no delivery and nothing to acknowledge; the transport ends its flow,
    /// which is what the client is told.
    Refused(Failure),
}

/// What the owner sends one DNS-over-TCP transport on the transport's own control channel.
///
/// Depth one, and one channel rather than a pair of oneshots per query: a transport asks one thing at a time
/// and waits for the reply before asking again, so a slot per flow is the whole of what can be in flight. The
/// oneshots this replaces were per-query heap that appeared *before* the query's own grant existed, which is
/// exactly the shape the aggregate exists to prevent.
pub(crate) enum Control {
    /// What came of a reservation.
    Granted(Granted),
    /// What came of a published query.
    Answered(Answered),
}

impl Answering {
    /// Classifies, parks the delivery on the flow that asked, and only then passes the answer to its
    /// transport.
    ///
    /// One call, so a transport can never learn an identity for something an acknowledgment could not find -
    /// and nothing that *has* no identity is ever parked. `true` means this displaced a delivery already
    /// parked there - a second answer for a question that was never asked - which the caller counts.
    pub(crate) fn hand_over(self, admission: &mut Admission, serving: &mut Serving) -> bool {
        let Self { settled } = self;
        // Before the park, which is the whole of the fix. What is answerable is already an answer by this
        // point - [vpnhotspotd::shared::dns_wire::resolved] turned every expected platform outcome into that
        // query's own SERVFAIL at the terminal - and what is left is terminal for the stream.
        let mut ending = None;
        let settled = settled.classify(|resolved| classify(resolved, &mut ending));
        let parking = serving.delivery.park(admission, settled);
        match parking.answer {
            Some((delivery, deliverable)) => {
                let Deliverable { answer, message } = deliverable;
                // The query dies here, inside the grant that covered it, before the answer goes anywhere.
                drop(message);
                serving.answer(Answered::Delivered {
                    delivery,
                    result: answer,
                });
            }
            // Nothing was parked, so there is nothing to acknowledge. A terminal failure is still the one
            // thing the transport has to be told, because it ends the stream rather than waiting for an
            // answer that is not coming.
            None => {
                if let Some(failure) = ending {
                    serving.answer(Answered::Refused(failure));
                }
            }
        }
        parking.replaced
    }

    /// Nobody will consume this - the flow is gone, or there was never an answer. Ends the delivery here,
    /// which is the last owner that can.
    pub(crate) fn discard(self, admission: &mut Admission) {
        self.settled.discard(admission);
    }
}

impl Delivered {
    pub(crate) fn new(
        settled: dns_debt::Settled<Resolved>,
        stamp: crate::shizuku::tun_writer::Stamp,
        flow: Event,
        network: vpnhotspotd::shared::model::Network,
        unobservable: bool,
    ) -> Self {
        Self {
            answering: Answering { settled },
            stamp,
            flow,
            network,
            unobservable,
        }
    }

    /// Whether the transport that asked may carry on, and whether the token it holds may ever be reused.
    ///
    /// `true` is the platform holding a resolver slot whose end nothing here can observe: the transport is
    /// ended and its token is quarantined, exactly as for a submission that could not be watched from the
    /// moment it was made.
    pub(crate) fn unobservable(&self) -> bool {
        self.unobservable
    }

    /// The local failure this settlement is carrying, if it is carrying one.
    ///
    /// A borrow, so the answer still leaves only through [Answering::hand_over] and nothing here can deliver
    /// anything. What it is for is the discard paths in [crate::shizuku::tcp::Engine::settle]: a transport that is
    /// gone, reused or epoch-stale never reaches its own terminal with this failure, so the owner about to
    /// drop the settlement is the last one that can say what happened.
    pub(crate) fn refusal(&self) -> Option<&Failure> {
        self.answering.settled.answer()?.result.as_ref().err()
    }

    /// Whether there is an answer at all. `false` for a transaction that produced nothing, which parks
    /// nothing.
    pub(crate) fn has_answer(&self) -> bool {
        self.answering.settled.has_answer()
    }

    /// Which config this answer belongs to, and which exact transport asked for it.
    pub(crate) fn stamp(&self) -> crate::shizuku::tun_writer::Stamp {
        self.stamp
    }

    pub(crate) fn flow(&self) -> Event {
        self.flow
    }

    pub(crate) fn network(&self) -> vpnhotspotd::shared::model::Network {
        self.network
    }

    /// Replaces a predecessor generation's answer with that query's own SERVFAIL, before anything is parked.
    ///
    /// The real result was resolved on a selection this session has stopped claiming, so it is dropped -
    /// but the client's transport survived the handover and is still waiting, and a stream that simply
    /// stalled would be worse than one told to try again. The replacement is built from the query the
    /// transaction kept for exactly this, and it happens *in place*: the answer still only leaves through
    /// parking, so the ordering the delivery grant depends on is untouched.
    ///
    /// `false` when there is nothing to replace or no SERVFAIL can be formed from the query, which leaves
    /// the caller to discard.
    pub(crate) fn stale(&mut self) -> bool {
        self.answering.settled.replace_answer(|resolved| {
            let Resolved { result, message } = resolved;
            // Dropped first and explicitly: the grant covers the query and one answer-sized buffer, so the
            // obsolete result goes before its replacement is built rather than beside it.
            drop(result);
            let message = message?;
            let servfail = dns_wire::servfail_response(&message).map(Owned::new);
            servfail.map(|servfail| Resolved {
                result: Ok(servfail),
                message: Some(message),
            })
        })
    }

    /// The answer and the transport it is owed to, with the classification done.
    pub(crate) fn answering(self) -> Answering {
        self.answering
    }

    /// Nobody will consume this - the flow is gone, or there was never an answer.
    pub(crate) fn discard(self, admission: &mut Admission) {
        self.answering.discard(admission);
    }
}

/// What one answer still owns after it has been built: the answer itself, the length-prefixed copy the
/// transport frames beside it, and the one piece on its way into the mailbox.
///
/// Every term is real and none of them may be dropped. The prefix in particular: `frame` allocates
/// `answer.len() + PREFIX`, so a bound of two maximum messages is two bytes short of a maximum answer -
/// small, and exactly the kind of shortfall that turns a bound into an approximation.
pub(crate) const fn delivery_bytes(answer: usize) -> u64 {
    2 * answer as u64 + dns_wire::PREFIX as u64 + READ_CHUNK as u64
}

/// What is still owed once the resolver transaction has reached its terminal, which is the peak the
/// *transport* reaches afterwards, for the largest answer the platform can return.
///
/// A 16-bit length prefix is what bounds a DNS-over-TCP message, so `u16::MAX` is the ceiling rather than an
/// estimate - and unlike the query, the answer's size is not knowable before the platform returns it, so this
/// one stays the conservative maximum.
pub(crate) const DELIVERY_BYTES: u64 = delivery_bytes(MAX_DATAGRAM);

/// What one submitted exchange owes: exactly the query the client announced, and the peak the answer path
/// reaches after it.
///
/// Charged to the *query's debt* rather than to the connection, because that is what actually owns it. An
/// idle connection has no query, no answer and nothing to frame; charging it anyway is a debt for work that
/// has not happened, taken from the floor that exists so real resolver work is never crowded out. It is also
/// what makes a transport closing over a question still in flight come out right: the flow's buffers go with
/// the flow, and these stay charged until the answer has actually been framed and consumed.
///
/// A transport is sequential by construction - it asks one question and awaits its answer before framing the
/// next - so one debt at a time is one exchange at a time.
pub(crate) fn exchange_bytes(length: usize) -> Option<u64> {
    (length as u64).checked_add(DELIVERY_BYTES)
}

/// What answering one query here owes: the query, the SERVFAIL built from it, that answer's framed copy, and
/// the one piece on its way into the mailbox.
///
/// A SERVFAIL echoes its query's header and question and nothing else, so it cannot exceed the query it was
/// built from - which is what makes this tier genuinely cheaper than [exchange_bytes] rather than merely
/// different, and why a query the descriptor floor has no room for can still be answered.
pub(crate) fn answered_here_bytes(length: usize) -> Option<u64> {
    (length as u64).checked_add(delivery_bytes(length))
}

/// One DNS-over-TCP flow's owner-side state: the two depth-one control channels built with the flow, the
/// reservation it currently holds, the transaction it opened, and the delivery parked for its answer.
///
/// Both channels are built with the flow and charged with it - see [crate::shizuku::tcp]'s per-flow footprint - which
/// is the point: no control heap appears before the fixed lease that covers it. The *filled* end is this
/// owner's rather than the transport's, which is what makes a sweep mid-query exact: the buffer travels back
/// on a channel this owner holds, so this flow's own close takes it, drops it, and only then gives the grant
/// back. A grant a worker held would be one a cancellation stranded.
pub(crate) struct Serving {
    /// Where this owner answers the transport. Depth one, because a transport asks one thing at a time.
    control: mpsc::Sender<Control>,
    /// The exact query buffer the transport filled, on its way back to the owner that granted it.
    filled: mpsc::Receiver<Owned>,
    /// Capacity admitted for a query whose length the client announced and whose bytes have not all arrived.
    reserved: Option<Reserved>,
    /// What the answer still owns after its terminal: the result, the framed copy and the chunk on its way
    /// into the mailbox. Held here rather than released at the terminal, because every one of those buffers
    /// exists *after* the transaction has finished - the transport has yet to receive, classify, frame and
    /// hand them over. Ended when the transport says the last chunk was acknowledged, or when this flow
    /// closes without it.
    delivery: Parked,
    /// The resolver transaction this flow opened, if its question is still outstanding. Named so that a
    /// transport closing over one can hand that question its token rather than charging a second. Cleared
    /// when that transaction settles, because the token ends there.
    transaction: Option<u64>,
    /// A control message the transport could not be given, which means the flow is already on its way out.
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

    /// Whether this transport already has a reservation outstanding. One query at a time per transport, which
    /// is what its single logical token means.
    pub(crate) fn reserving(&self) -> bool {
        self.reserved.is_some()
    }

    pub(crate) fn reserve(&mut self, reserved: Reserved) {
        self.reserved = Some(reserved);
    }

    /// The reservation and the exact query the transport filled, if it got that far.
    ///
    /// Both together, because either alone is wrong: a reservation with no query is a transport that has not
    /// finished framing, and a query with no reservation is a buffer nothing accounted for.
    pub(crate) fn accept(&mut self) -> Option<(Reserved, Option<Owned>)> {
        let reserved = self.reserved.take()?;
        Some((reserved, self.filled.try_recv().ok()))
    }

    /// Answers the transport. Never awaited and never blocking: the transport consumes each reply before it
    /// asks again, so the one slot is free - and a full one or a gone receiver both mean the flow is on its
    /// way out, which its own terminal settles.
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

    /// Tells this transport there will be no answer and the stream is over. Nothing is parked, so there is
    /// nothing to acknowledge.
    pub(crate) fn refuse(&mut self, failure: Failure) {
        self.answer(Answered::Refused(failure));
    }

    /// The transaction this transport has outstanding.
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

    /// The flow is closing. Everything physical goes first - the parked delivery, whatever query was still
    /// travelling back, both channel ends - and only then the reservation's grant, so nothing is refunded
    /// while the memory it covered is still alive.
    ///
    /// Answers with the transaction this transport left outstanding, which its close has to hand a token to.
    pub(crate) fn close(self, admission: &mut Admission) -> Closed {
        let Self {
            control,
            filled,
            reserved,
            mut delivery,
            transaction,
            ..
        } = self;
        // The consumer is gone, so nothing will ever acknowledge whatever it was still delivering.
        delivery.close(admission);
        let drained = Closing {
            control,
            filled,
            reserved,
        }
        .drained();
        // Nothing external was started for a reservation: no descriptor was opened and the platform was never
        // asked, which is exactly why it may be ended rather than left to settle like a transaction.
        if let Some(reserved) = drained.reserved {
            reserved.end(admission);
        }
        Closed { transaction }
    }
}

/// What a closing flow still physically owns, and the reservation whose grant covers part of it.
///
/// The reservation is *inside*, and [Closing::drained] is the only way out - which destroys both channel ends
/// and whatever query was still travelling on one of them first. That makes the order structural rather than
/// commented: an owner cannot reach the grant without having dropped the buffer it pays for. Same shape as
/// the delivery terminal in [crate::shizuku::virtual_dns], for the same reason - a balance cannot tell the two orders
/// apart, because the buffer dies once and the grant is released once either way.
struct Closing {
    control: mpsc::Sender<Control>,
    filled: mpsc::Receiver<Owned>,
    reserved: Option<Reserved>,
}

/// A reservation whose covered buffers are provably gone.
struct Drained {
    reserved: Option<Reserved>,
}

impl Closing {
    /// Destroys both control endpoints and whatever query was still on one of them, and only then yields the
    /// reservation.
    fn drained(self) -> Drained {
        let Self {
            control,
            mut filled,
            reserved,
        } = self;
        // A query this transport was admitted for and handed back into a channel this owner had not read yet
        // dies here, and before the grant covering it does.
        filled.close();
        while filled.try_recv().is_ok() {}
        drop(filled);
        drop(control);
        Drained { reserved }
    }
}

/// What a closing flow's DNS state answered with.
pub(crate) struct Closed {
    /// The transaction this transport left outstanding, which its close has to hand a token to.
    pub(crate) transaction: Option<u64>,
}

/// Answers one query the platform will never be asked about, under the grant that already covers it.
///
/// Four refusals reach here and they are the same answer to the client: no selected network to resolve on, a
/// descriptor floor with no room for a transaction, a transaction table that would have had to grow, and a
/// submission the platform refused outright before this owner asked it anything. Each is a SERVFAIL for that
/// message on a stream that stays open, which is the difference between a client that asks again at once and
/// one that waits out its own timeout.
///
/// `None` is a query too malformed for a SERVFAIL to be built from: there is no question to echo, so there is
/// nothing to answer with and nothing to park. The transport is told so here rather than left waiting.
pub(crate) fn answered_here(
    reserved: Reserved,
    query: Owned,
    serving: &mut Serving,
    admission: &mut Admission,
) -> Option<Answering> {
    // Built while the reservation still covers the query *and* the answer allowance taken beside it, so this
    // allocation is one that was charged before it existed.
    let servfail = dns_wire::servfail_response(&query).map(Owned::new);
    // ...and the query dies as soon as it has been built from, before anything is reconciled downward.
    drop(query);
    let Some(servfail) = servfail else {
        serving.answer(Answered::Refused(Failure::Expected(io::Error::other(
            "a DNS-over-TCP query too malformed to answer",
        ))));
        reserved.end(admission);
        return None;
    };
    // Reconciled to exactly what physically survives this call: the answer, the framed copy the transport
    // builds beside it, and the one piece on its way into the mailbox. Nothing here is a new charge - the
    // reservation covered all of it - and what is left of that reservation ends inside the split.
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

/// What a DNS-over-TCP transport asks its owner for. Three things, and every one of them is an accounting
/// decision a worker cannot make for itself.
pub(crate) enum Ask {
    /// A length prefix has been framed, and not one byte of the message it announces has been stored. Admit
    /// that length, or refuse it.
    ///
    /// Both halves of the flow's identity travel with it: smoltcp reuses handles, so a request naming only a
    /// handle could be admitted against whatever flow reused it. The reply goes back on the flow's own
    /// control channel rather than on a oneshot made for this question, because that oneshot was heap
    /// allocated before the query it belongs to had any grant at all.
    Reserve {
        flow: Event,
        /// Exactly what the client's length prefix announced.
        length: usize,
    },
    /// The admitted buffer is full: an exact validated query, at the boundary that publishes it.
    ///
    /// Payload-free, because the query itself travels on the depth-one channel the owner kept when it granted
    /// the capacity. A query carried here instead would be a buffer in a shared queue when its flow's close
    /// ran, and the close would refund the grant covering bytes that were still in flight.
    Query(Event),
    /// An answer whose last chunk the client's stack has acknowledged, and whose result and framing buffers
    /// are dropped. The delivery grant may end - and only the owner may end it.
    ///
    /// Both identities, because either alone is wrong. The flow says which transport is speaking; the
    /// delivery says *which answer* it is about. A transport asks one question after another on one flow, so
    /// an acknowledgment naming only the flow would match whichever delivery happened to be parked - and a
    /// late one for a question already finished would release its successor's grant while the bytes that
    /// grant covers were still being framed.
    Delivered { flow: Event, delivery: DeliveryId },
}

/// What the owner answered a reservation with.
pub(crate) enum Granted {
    /// Capacity for exactly this query and for the answer that follows it. The buffer to fill, and nothing
    /// else: where it goes back and where its answer arrives are the flow's own channels, built and charged
    /// with the flow.
    Admitted(Owned),
    /// Nothing could be granted, not even an answer built here. The transport skips the announced bytes, so
    /// the stream stays framed and the client may ask again; nothing was allocated and the platform was never
    /// asked.
    Denied,
}

/// Serves one flow's stream until the client stops asking or the engine cancels it.
///
/// This task holds no descriptor, so its terminal is not what releases the flow. On the clean path - the
/// client has finished asking and this hands over an ordered end of stream - the engine *detaches* the flow
/// and the client's own teardown finishes first; see `shizuku/tcp/terminal.rs`. The resolver slot belongs to the
/// transaction either way, which the ingress owner holds in a table of its own when this is swept. Nothing
/// terminal travels on the events channel, exactly as for an ordinary flow.
pub(crate) async fn serve(
    mut mailbox: Mailbox,
    mut downstream: mpsc::Receiver<Owned>,
    asks: mpsc::Sender<Ask>,
    mut control: mpsc::Receiver<Control>,
    filled: mpsc::Sender<Owned>,
    cancel: CancellationToken,
) -> Ended {
    let flow = mailbox.identity;
    // Two bytes and a count between reads: the length prefix is framed before anything is stored, so a client
    // that dribbles a query cannot grow anything here at all.
    let mut stream = DnsStream::default();
    // The message being filled, once its length has been admitted. Absent while nothing is being framed, and
    // absent for an announced message nothing could be granted for - whose bytes are skipped.
    let mut filling: Option<Owned> = None;
    loop {
        let chunk = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ended::Expected,
            chunk = downstream.recv() => chunk,
        };
        // The engine dropping the sender is how a client's half-close reaches here: no more queries are
        // coming.
        let Some(chunk) = chunk else {
            // Bytes arrived that never completed a message, so the client truncated its own request. There is
            // nothing to answer and no boundary to end cleanly on; the reset the engine writes for a reported
            // ending is what tells the client, where a clean FIN would suggest its query had been served.
            if stream.partial() {
                return Ended::Reported("DNS-over-TCP request ended mid-message".to_owned());
            }
            // Everything asked for has been answered and the client is done asking, so the stream ends
            // cleanly - and *ordered* after the answers already in the mailbox. Awaited, not fired and
            // forgotten: returning before the client's stack has taken the end of stream would let the
            // lifecycle terminal overtake the bytes it is supposed to follow.
            if !mailbox.hand_over(Chunk::Finished, &cancel).await {
                return Ended::Expected;
            }
            return Ended::Expected;
        };
        // One chunk may carry several messages, a fraction of one, or the tail of the last. All are ordinary.
        let mut chunk = Some(chunk);
        let mut offset = 0usize;
        while let Some(held) = chunk.take() {
            let mut rest = &held[offset..];
            let framed = stream.advance(
                &mut rest,
                filling.as_mut().map(|query| query as &mut dyn Body),
            );
            offset = held.len() - rest.len();
            // Kept only while it still has bytes in it. A spent chunk is dropped *here*, before any of the
            // waits below: a transport parked on an answer while still holding bytes it has already framed
            // is a chunk of this flow's grant held for as long as the platform takes to answer.
            if offset < held.len() {
                chunk = Some(held);
            } else {
                offset = 0;
                drop(held);
            }
            match framed {
                // More bytes are needed, and this chunk has none left.
                Framed::Hungry => break,
                // Reset rather than ignored: nothing after a length that can never complete is at an offset
                // this could resynchronize on. Two causes and one outcome - a zero-length message, which a
                // client can send, and a body shorter than the length it was admitted for, which is this
                // daemon disagreeing with itself and cannot happen while the capacity comes from the
                // announced length.
                Framed::Broken => {
                    return Ended::Reported(
                        "a DNS-over-TCP length this stream cannot resynchronize after".to_owned(),
                    )
                }
                // Announced and not stored. What comes back decides where its bytes go - into a buffer the
                // owner admitted, or nowhere at all.
                Framed::Length(length) => {
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
                        // Nothing was granted, so these bytes are skipped rather than stored: the stream stays
                        // framed and the client's next question is read as if this one had been answered.
                        Some(Control::Granted(Granted::Denied)) => None,
                        // An answer where a grant belongs, which this owner does not send - or the owner gone,
                        // which happens only once the session itself is ending.
                        Some(Control::Answered(_)) | None => return Ended::Expected,
                    };
                }
                Framed::Message => {
                    // A message that was skipped - nothing was admitted for it - is over, and there is nothing
                    // to answer.
                    let Some(query) = filling.take() else {
                        continue;
                    };
                    // The exact query goes back to the owner first, and only then is the owner told. A closed
                    // channel means the flow is already being torn down.
                    if filled.try_send(query).is_err() {
                        return Ended::Expected;
                    }
                    // Both waits race the token, because a retirement must not wait on the resolver: saying so
                    // waits only on this owner, but the answer is bounded by the platform's timers alone.
                    if !hand_over(&asks, Ask::Query(flow), &cancel).await {
                        return Ended::Expected;
                    }
                    let delivered = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return Ended::Expected,
                        delivered = control.recv() => delivered,
                    };
                    // The identity of the delivery the owner parked for this answer, carried through framing
                    // and every chunk so the acknowledgment at the end names the answer it is actually about.
                    let (delivery, answer) = match delivered {
                        Some(Control::Answered(Answered::Delivered { delivery, result })) => {
                            (delivery, result)
                        }
                        // Everything a client can drive has already become this message's own SERVFAIL, so
                        // what is left here is the daemon's own wrapper failing - a structured report - a
                        // query too malformed to answer at all, or a platform slot this process can no longer
                        // watch. All of them end the flow, and a reset beats a silent stall either way: a
                        // client retries elsewhere on a closed connection and waits on an open one. No
                        // delivery was named, so nothing is acknowledged.
                        Some(Control::Answered(Answered::Refused(failure))) => {
                            return failure.ended("resolver query")
                        }
                        // A grant where an answer belongs, which this owner does not send - or the owner gone,
                        // which happens only once the session itself is ending.
                        Some(Control::Granted(_)) | None => return Ended::Expected,
                    };
                    // Framed once, then handed over one piece at a time - and the two halves of that are what
                    // the delivery grant covers. `frame` allocates the length-prefixed copy, which stays alive
                    // because every piece is copied out of it; each piece is built immediately before its
                    // handover and gone before the next exists, which is what [hand_over_in_pieces] is for.
                    // Building every piece first would satisfy the mailbox's depth and hold a second whole
                    // copy of the response while doing it. The framed copy is owned the same way the answer
                    // is: the count is inside the buffer, so it ends when the buffer does and cannot be ended
                    // early by mistake.
                    let Some(framed) = frame(&answer).map(Owned::new) else {
                        return Ended::Reported("resolver answer exceeds a DNS message".to_owned());
                    };
                    let consumed = matches!(
                        hand_over_in_pieces(&mut mailbox, &framed, READ_CHUNK, &cancel).await,
                        Handed::Complete
                    );
                    // Dropped explicitly, and before the owner is told the delivery is over: what the delivery
                    // grant covers is the answer, the framed copy built beside it and the piece in flight, so
                    // the owner may only end it once all three are really gone.
                    drop(framed);
                    drop(answer);
                    if !consumed {
                        // Cancelled part-way. The buffers are gone, and the owner ends the delivery on the
                        // close path rather than here - a report from a task that is going away could arrive
                        // after the flow it names has been retired.
                        return Ended::Expected;
                    }
                    if !hand_over(&asks, Ask::Delivered { flow, delivery }, &cancel).await {
                        return Ended::Expected;
                    }
                }
            }
        }
    }
}
