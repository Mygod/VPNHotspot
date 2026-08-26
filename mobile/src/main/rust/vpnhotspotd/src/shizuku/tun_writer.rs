//! The common TUN writer: the single path every packet the daemon originates or relays leaves through.
//!
//! It exists so that the retirement gate, final size validation and the descriptor's own writability wait
//! happen in exactly one place rather than at each producer.
//!
//! **It decides nothing about what a packet contains.** The size policy against the downstream floor, the
//! Identification an oversized IPv4 datagram carries, and source fragmentation for both families all belong
//! to the ingress task's output owner and have already happened by the time a packet arrives here - see
//! [crate::shizuku::output] and [vpnhotspotd::shared::packet_writer]. What arrives is finished bytes; what this does
//! is dequeue them, compare the stamp they were produced under, validate them one last time, write them, and
//! report what became of them.
//!
//! The two backpressure sources are deliberately not the same thing:
//!
//! - **The daemon's queue filling** is an admission decision. [Writer::enqueue] refuses, the producer
//!   refunds whatever it reserved, and no packet is silently dropped after being promised.
//! - **The kernel's queue filling** is `EAGAIN` on a nonblocking descriptor. That is a wait for
//!   writability, not an admission decision: a packet already accepted here is not re-admitted or
//!   re-charged when the wait ends.
//!
//! Conflating them would either drop packets the daemon promised to send or let the queue grow past its
//! budget.
//!
//! It is, however, the only owner that knows whether a packet reached the wire, which is why the IPv4
//! Identification allocator's window is driven from here. A packet accepted into the queue above may be
//! dropped at the dequeue stamp gate, refused by final validation, abandoned because a retirement preempted
//! its blocked write, or written - and only the last of those may ever stop that value being issued again.
//!
//! **While the session continues, every guarded packet this writer accepts ends in exactly one [Terminal]
//! back to the ingress task.** The exceptions are the endings of the session itself: a fatal write, a lost
//! settlement path, cancellation, and an orphaned writer all stop the loop with packets possibly still in
//! the queue, and those registrations are never settled. That is deliberate rather than overlooked - the
//! session is over, the allocator is about to be dropped, and the successor session's opening quarantine is
//! what covers whatever those packets did or did not do on the wire.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::ipv4_identification::{Guarded, Terminal};
use vpnhotspotd::shared::packet_writer::validate;
use vpnhotspotd::shared::protocol::{describe_io_error, IoErrorReportExt};
use vpnhotspotd::shared::reply_bound::{channel_footprint, reply_channel_footprint};

use crate::report;

/// Matches the TUN device's own transmit queue length, so the daemon buffers no more toward a client than
/// the interface itself would. Taken from the kernel's `TUN_READQ_SIZE`, which `tun_setup` assigns to
/// `dev->tx_queue_len`, rather than picked: a deeper queue would add latency the device does not, and a
/// shallower one would drop where the device would not.
///
/// https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/include/uapi/linux/if_tun.h#25
/// https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/drivers/net/tun.c#2342
pub(crate) const QUEUE_DEPTH: usize = 500;

/// How many guarded packets can be waiting for their ending to be applied at once, and therefore how deep the
/// settlement channel is.
///
/// Derived, not picked: every guarded packet this writer has been given is in exactly one of three places -
/// a queue slot, the writer's own hand, or this channel - and the allocator refuses to register more than
/// this many at once. So a terminal can never find the channel full, which is what lets it be sent without
/// waiting. A writer that waited here could not reach its retirement arm, and the ingress task waiting for
/// that acknowledgement is the one thing that must never be blocked behind feedback it is itself the
/// consumer of.
pub(crate) const TERMINAL_DEPTH: usize = QUEUE_DEPTH + 1;

/// Every Rust-visible byte this writer's construction owns, for the whole session.
///
/// One equation rather than a figure assembled at the reserve site, because the terms are facts about the
/// types below and nothing outside this module can enumerate them correctly. Four terms:
///
/// - the packet channel's own blocks and shared state, plus `(QUEUE_DEPTH + 1)` maximum packets - a full
///   queue and the one the writer has taken out of it and is writing, because taking a message returns its
///   permit and the queue can refill behind it. Both halves come from [reply_channel_footprint]: this module
///   used to add the in-hand packet itself, and it is in the shared helper now because every reply queue
///   needs it for the same reason and the others were short of it;
/// - the retirement channel at depth one. Its 1 KiB state bound is far more than a one-slot channel needs
///   and comfortably covers the single `oneshot` the ingress task has outstanding inside it;
/// - the settlement channel at [TERMINAL_DEPTH].
///
/// Checked throughout, and `None` when any term would wrap: a writer whose cost cannot be stated is one that
/// must not be built.
pub(crate) fn footprint(mtu: usize) -> Option<u64> {
    // One producer each. The packet queue and the retirement channel are the ingress task's alone - it holds
    // the only [Writer] and hands out references rather than clones - and the settlement channel is the writer
    // task's alone. So none of the three can have a second sender racing a block growth.
    reply_channel_footprint::<Queued>(QUEUE_DEPTH, 1, mtu as u64)?
        .checked_add(channel_footprint::<Retirement>(1, 1)?)?
        .checked_add(channel_footprint::<Terminal>(TERMINAL_DEPTH, 1)?)
}

/// Rejected at admission, so the producer still owns the packet and whatever it reserved for it.
#[derive(Debug)]
pub(crate) struct Rejected;

/// Which retirement a packet belongs to: both axes, because either one advancing retires the state that
/// produced it. The generation says which `Network` its upstream socket was bound to, the epoch whether its
/// TUN-visible tuple still names the same client, and a handover moves only the first.
///
/// This is what makes the purge free. A packet already queued when a sweep runs carries the retired pair, so
/// dropping it at dequeue needs no second pass over the queue - and the terminal packets a sweep writes are
/// stamped with the new pair, which is why they leave while the non-terminal ones behind them do not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) generation: u64,
    pub(crate) epoch: u64,
}

pub(crate) struct Queued {
    /// The retirement the packet was produced under, attached at enqueue and compared at dequeue. There is
    /// deliberately no second comparison at enqueue: every producer runs on the ingress task, which is also
    /// what publishes a retirement and drains its own sweeps to completion, so a packet cannot be enqueued
    /// under a retirement that has already passed. What the dequeue comparison catches is the queue a sweep
    /// inherits.
    stamp: Stamp,
    packet: Vec<u8>,
    /// The guarded datagram this packet belongs to, or `None` for everything that carries no Identification
    /// this daemon issued. Every packet of one datagram carries the same identity, so a fragmented datagram
    /// is settled once per fragment against one entry.
    guarded: Option<Guarded>,
}

/// One retirement, and the half of the handover fence that lives on this side.
///
/// A stamp published where the writer merely *reads* it is not a fence. It closes the enqueue side, because
/// every producer runs on the ingress task and every dequeue compares against the current value - but it says
/// nothing about the write already in flight when it changed. `write_all` can be parked in
/// `AsyncFd::writable` for as long as the kernel's queue stays full, and when that wait ends it would put an
/// old-stamp packet on the wire after the session had already been told the retirement was complete.
///
/// So retirement is a command with an answer instead. The writer adopts the new stamp, abandons any write it
/// was parked in, and only then acknowledges - after which no packet of a retired stamp can reach the wire,
/// because every remaining one fails the dequeue comparison.
pub(crate) struct Retirement {
    stamp: Stamp,
    ack: oneshot::Sender<()>,
}

/// The two receiving ends the writer task owns, kept together so a caller cannot wire up one without the
/// other.
pub(crate) struct Queue {
    packets: mpsc::Receiver<Queued>,
    retirements: mpsc::Receiver<Retirement>,
    /// Where a guarded packet's ending goes. Owned by the writer task because the writer is what produces
    /// one; the ingress task holds the other half and applies it to the allocator.
    terminals: mpsc::Sender<Terminal>,
}

#[derive(Clone)]
pub(crate) struct Writer {
    sender: mpsc::Sender<Queued>,
    retirements: mpsc::Sender<Retirement>,
}

impl Writer {
    /// Refuses rather than waits: a producer that blocks here would hold whatever lock or budget it took
    /// to build the packet, and the caller is the only thing that knows how to refund it.
    pub(crate) fn enqueue(
        &self,
        stamp: Stamp,
        packet: Vec<u8>,
        guarded: Option<Guarded>,
    ) -> Result<(), Rejected> {
        self.sender
            .try_send(Queued {
                stamp,
                packet,
                guarded,
            })
            .map_err(|_| Rejected)
    }

    /// Retires everything produced under the previous stamp and returns only once the writer says so.
    ///
    /// Awaited rather than fired: the acknowledgement is the proof, and without it the session could
    /// acknowledge a config while a packet from the retired generation was still on its way to a client.
    /// Called only when the stamp actually changes, so a write in flight when this arrives is necessarily one
    /// of the retired stamp and abandoning it is right.
    pub(crate) async fn retire(&self, stamp: Stamp) -> io::Result<()> {
        let (ack, answered) = oneshot::channel();
        self.retirements
            .send(Retirement { stamp, ack })
            .await
            .map_err(|_| io::Error::other("tun egress stopped before ingress"))?;
        answered
            .await
            .map_err(|_| io::Error::other("tun egress abandoned a retirement"))
    }
}

/// The three channels one session's TUN writer owns, and the receiving end the ingress task keeps.
///
/// All three are built together and charged together as one fixed owner, because they exist for exactly as
/// long as each other and a denial naming one of them would say nothing a denial naming the writer does not.
///
/// **This allocates, so production may only reach it after those bytes are reserved.** The one production
/// caller is [crate::shizuku::tun_reader::prepare], which builds these on the far side of a successful
/// [footprint]-sized reserve and hands them out inside a bundle; there is no production path that produces a
/// [Writer] or a [Queue] any other way.
pub(crate) fn channel() -> (Writer, Queue, mpsc::Receiver<Terminal>) {
    let (sender, packets) = mpsc::channel(QUEUE_DEPTH);
    // One at a time, because the ingress task issues one and awaits its answer before doing anything else.
    let (retirements, retiring) = mpsc::channel(1);
    let (terminals, settled) = mpsc::channel(TERMINAL_DEPTH);
    (
        Writer {
            sender,
            retirements,
        },
        Queue {
            packets,
            retirements: retiring,
            terminals,
        },
        settled,
    )
}

/// How far one packet got.
enum Progress {
    /// On the wire, at the moment the write returned. The instant is read *after* the syscall succeeded and
    /// nowhere else: it is the only honest answer to "when was this Identification used".
    Written(Instant),
    /// A retirement arrived while the write was parked. The packet is abandoned rather than finished: it
    /// carries the stamp being retired, and writing it after the acknowledgement is the one thing the fence
    /// exists to prevent.
    Preempted(Retirement),
    /// The ingress task is gone, so nothing will ever retire or produce again.
    Orphaned,
}

/// Hands one guarded packet's ending back to the allocator, answering whether the writer may carry on.
///
/// [mpsc::Sender::try_send] rather than a wait, and that is the deadlock this shape avoids: the ingress task
/// is the only consumer of this channel, and it stops consuming while it awaits a retirement acknowledgement.
/// A writer parked on a full settlement channel would never reach the retirement arm that would release it.
/// It cannot be full - [TERMINAL_DEPTH] holds one terminal for every guarded packet the allocator will
/// register at once - so a `Full` here is an accounting fault, and the session ends on it rather than losing
/// an ending and carrying on with a window it can no longer trust.
fn settle(
    terminals: &mpsc::Sender<Terminal>,
    guarded: Option<Guarded>,
    written: Option<Instant>,
) -> io::Result<bool> {
    let Some(guarded) = guarded else {
        return Ok(true);
    };
    let terminal = match written {
        Some(at) => Terminal::wrote(guarded, at),
        None => Terminal::unwritten(guarded),
    };
    match terminals.try_send(terminal) {
        Ok(()) => Ok(true),
        // The ingress task is gone, which already ends this session through its own result.
        Err(TrySendError::Closed(_)) => Ok(false),
        Err(TrySendError::Full(_)) => Err(io::Error::other(
            "the identification settlement channel filled, which its depth makes impossible",
        )),
    }
}

pub(crate) async fn run(
    fd: Arc<AsyncFd<OwnedFd>>,
    mtu: usize,
    queue: Queue,
    cancel: CancellationToken,
) -> io::Result<()> {
    let Queue {
        packets,
        retirements,
        terminals,
    } = queue;
    let mut counts = Counts::default();
    // Everything that must be physically gone *before* the settlement sender is, in its own scope. The
    // ingress task's teardown fence waits for that sender to close and treats it as proof that this task no
    // longer owns the queue, either receiver, or a packet taken out of the queue - which is exactly what the
    // writer's share of the fixed reservation paid for. Left to the compiler this would come out the wrong
    // way round: the destructuring above declares `terminals` last, so an end-of-function drop would close
    // the settlement channel *first* and the proof would be worthless.
    let result = {
        let mut packets = packets;
        let mut retirements = retirements;
        writing(
            &fd,
            mtu,
            &mut packets,
            &mut retirements,
            &terminals,
            &cancel,
            &mut counts,
        )
        .await
    };
    let Counts {
        stale,
        oversized,
        written,
        retired,
        settled,
    } = counts;
    report::stdout!(
        "tun egress: written {written} stale {stale} rejected {oversized} retired {retired} \
         settled {settled}"
    );
    // Reported as well as returned, and the two are not alternatives. The result ends the session, which
    // tells the app that something stopped; this is what tells it which errno stopped it, in which context,
    // with what the writer had managed beforehand. Raised here rather than at the point of failure because
    // this is the one place every fatal way out of the loop converges, and raised before the session's own
    // teardown finishes the reporter, so it is still carried rather than dropped.
    //
    // Attached first and emitted from the attachment, so there is one report rather than two equal ones: the
    // error carries exactly what was sent, which is how [crate::shizuku::app_session] knows this failure has already
    // been described and does not describe it again. The errno survives the attachment, because that is what
    // the report records.
    let result = result.map_err(|e| {
        e.with_report_context_details(
            "shizuku.tun_egress",
            [
                ("written", written),
                ("stale", stale),
                ("rejected", oversized),
                ("settled", settled),
            ],
        )
    });
    if let Err(e) = &result {
        report::report(describe_io_error(
            "shizuku.tun_egress",
            e,
            std::iter::empty::<(&str, &str)>(),
        ));
    }
    // Last, and explicitly. The lines above are deliberately inside the sender's life so that a reader cannot
    // reorder them without noticing what the ingress task is waiting on.
    drop(terminals);
    result
}

/// What one writer did, kept out of the loop below so that the loop can own nothing that outlives it and the
/// closing line can still be printed on every way out.
#[derive(Default)]
struct Counts {
    stale: u64,
    oversized: u64,
    written: u64,
    retired: u64,
    settled: u64,
}

/// The write loop itself, split out only so that everything it owns - including the packet it has in hand -
/// is dropped when it returns rather than at the end of [run].
async fn writing(
    fd: &AsyncFd<OwnedFd>,
    mtu: usize,
    packets: &mut mpsc::Receiver<Queued>,
    retirements: &mut mpsc::Receiver<Retirement>,
    terminals: &mpsc::Sender<Terminal>,
    cancel: &CancellationToken,
    counts: &mut Counts,
) -> io::Result<()> {
    // Zero on both axes, which no config carries, so the first config's retirement runs against an empty
    // queue rather than being skipped.
    let mut current = Stamp::default();
    loop {
        let queued = tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(()),
            // Ahead of the packet arm, so a retirement is adopted before another packet of the stamp it
            // retires can be taken out of the queue.
            retirement = retirements.recv() => match retirement {
                Some(retirement) => {
                    current = retirement.stamp;
                    counts.retired += 1;
                    // Failure means the ingress task stopped waiting, which its own error already explains.
                    let _ = retirement.ack.send(());
                    continue;
                }
                None => break Ok(()),
            },
            queued = packets.recv() => match queued {
                Some(queued) => queued,
                None => break Ok(()),
            },
        };
        // Only a guarded packet is a settlement: an unguarded one carries no Identification, so there is
        // nothing about it for the allocator to hear and nothing to count.
        let settlement = u64::from(queued.guarded.is_some());
        if queued.stamp != current {
            // Produced under a generation or epoch that has since been retired. This is the purge and the
            // catch at once: what a sweep left queued, and what an old-generation task enqueued after the
            // sweep had already drained it.
            counts.stale += 1;
            // Terminal without a write: the packet never reached the TUN, so whatever Identification it
            // carried is free to be issued again as soon as the rest of its datagram has ended too.
            match settle(terminals, queued.guarded, None) {
                Ok(true) => counts.settled += settlement,
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
            continue;
        }
        if let Err(e) = validate(&queued.packet, mtu) {
            // a packetization bug, not client input: the producer built something that does not fit or
            // does not describe itself, and sending it would corrupt a client's view
            counts.oversized += 1;
            report::message_with_details(
                "shizuku.tun_egress",
                format!("refused a packet the daemon built: {e:?}"),
                "packetization",
                [("bytes", queued.packet.len())],
            );
            match settle(terminals, queued.guarded, None) {
                Ok(true) => counts.settled += settlement,
                Ok(false) => break Ok(()),
                Err(e) => break Err(e),
            }
            continue;
        }
        let progress = match write_all(fd, &queued.packet, retirements).await {
            Ok(progress) => progress,
            // A write that neither succeeded nor asked to be retried is fatal - see [write_all] - so no
            // terminal is sent for this packet and the session ends with its registration outstanding.
            Err(e) => break Err(e),
        };
        // Settled before the acknowledgement below, so a preempted packet's ending is already on its way when
        // the ingress task wakes from the retirement it was waiting on.
        let at = match progress {
            Progress::Written(at) => Some(at),
            _ => None,
        };
        match settle(terminals, queued.guarded, at) {
            Ok(true) => counts.settled += settlement,
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
        match progress {
            Progress::Written(_) => counts.written += 1,
            Progress::Preempted(retirement) => {
                counts.stale += 1;
                current = retirement.stamp;
                counts.retired += 1;
                let _ = retirement.ack.send(());
            }
            Progress::Orphaned => break Ok(()),
        }
    }
}

/// One packet per write, because a TUN descriptor delivers a write as one packet or fails: there is no
/// short write to resume, so the only partial case is a multi-fragment datagram whose later fragments
/// fail, and that is the producer's to handle.
///
/// That is a contract, so this checks it rather than assuming it. A count that is neither the whole packet
/// nor an error is a descriptor behaving in a way the whole design rests on it not doing, and there is no
/// safe reading of it: the bytes on the wire are a truncated packet the client will parse as something else,
/// and resuming would put the remainder out as a second one. So it is a fatal error naming both counts, and
/// the packet gets no successful timestamp and no terminal - the session ends instead.
async fn write_all(
    fd: &AsyncFd<OwnedFd>,
    packet: &[u8],
    retirements: &mut mpsc::Receiver<Retirement>,
) -> io::Result<Progress> {
    loop {
        // The parked wait is the only place a retirement can find a packet already accepted but not yet on
        // the wire, so it is also the only place the fence needs a preemption. Biased, because a retirement
        // that arrived is more current than a writability that also did.
        let mut guard = tokio::select! {
            biased;
            retirement = retirements.recv() => return Ok(match retirement {
                Some(retirement) => Progress::Preempted(retirement),
                None => Progress::Orphaned,
            }),
            guard = fd.writable() => guard?,
        };
        match guard
            .try_io(|inner| rustix::io::write(inner.get_ref(), packet).map_err(io::Error::from))
        {
            // Read here and nowhere earlier: the write has returned, so this is when the Identification it
            // carries actually became visible to a receiver. Anything before the syscall - the top of this
            // loop, or before the writability wait - would date the value to a moment it was still queued.
            Ok(Ok(count)) if count == packet.len() => return Ok(Progress::Written(Instant::now())),
            Ok(Ok(count)) => {
                return Err(io::Error::other(format!(
                    "the TUN took {count} of a {}-byte packet, which a packet-atomic \
                     descriptor may not do",
                    packet.len()
                )))
            }
            Ok(Err(e)) => return Err(e),
            // the kernel queue was full after all; wait for writability again without re-charging
            Err(_would_block) => continue,
        }
    }
}
