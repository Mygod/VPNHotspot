//! Serializes nonblocking TUN writes after the retirement-stamp gate.
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
use vpnhotspotd::shared::protocol::IoErrorReportExt;
use vpnhotspotd::shared::reply_bound::{channel_footprint, reply_channel_footprint};

use crate::report;

/// Matches the kernel TUN transmit queue length (`TUN_READQ_SIZE`).
pub(crate) const QUEUE_DEPTH: usize = 500;

/// How many guarded packets can be waiting for their ending to be applied at once, and therefore how deep the
/// settlement channel is.
pub(crate) const TERMINAL_DEPTH: usize = QUEUE_DEPTH + 1;

/// Every Rust-visible byte this writer's construction owns, for the whole session.
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

/// Generation that bound the packet's upstream selection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) generation: u64,
}

pub(crate) struct Queued {
    /// Retirement stamp captured at enqueue and checked at dequeue.
    stamp: Stamp,
    packet: Vec<u8>,
    /// The guarded datagram this packet belongs to, or `None` for everything that carries no Identification
    /// this daemon issued. Every packet of one datagram carries the same identity, so a fragmented datagram
    /// is settled once per fragment against one entry.
    guarded: Option<Guarded>,
}

/// One retirement, and the half of the handover fence that lives on this side.
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
    // Drop all queue state before `terminals`; its closure is the ingress task's teardown fence.
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
    // Described here and delivered nowhere: attaching the report is this task's whole reporting duty, and
    // [crate::shizuku::app_session] is what puts it in front of the app - as the start call's terminal
    // `ErrorFrame` when that call is still owed one, and as a nonfatal when it is not. Raised here rather
    // than at the point of failure because this is the one place every fatal way out of the loop converges,
    // so the counts below are attached to whichever errno stopped it.
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
    // Zero, which no config carries, so the first config's retirement runs against an empty queue rather
    // than being skipped.
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
            // Produced under a generation that has since been retired. This is the purge and the catch at
            // once: what a sweep left queued, and what an old-generation task enqueued after the sweep had
            // already drained it.
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
            // A daemon packetization bug, not client input.
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
