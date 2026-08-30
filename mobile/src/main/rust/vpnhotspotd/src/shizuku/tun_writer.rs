//! Serializes nonblocking TUN writes.
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
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
    // One producer each. The packet queue is the ingress task's alone - it holds the only [Writer] and hands
    // out references rather than clones - and the settlement channel is the writer task's alone. So neither
    // can have a second sender racing a block growth.
    reply_channel_footprint::<Queued>(QUEUE_DEPTH, 1, mtu as u64)?.checked_add(channel_footprint::<
        Terminal,
    >(
        TERMINAL_DEPTH, 1
    )?)
}

/// Rejected at admission, so the producer still owns the packet and whatever it reserved for it.
#[derive(Debug)]
pub(crate) struct Rejected;

pub(crate) struct Queued {
    packet: Vec<u8>,
    /// The guarded datagram this packet belongs to, or `None` for everything that carries no Identification
    /// this daemon issued. Every packet of one datagram carries the same identity, so a fragmented datagram
    /// is settled once per fragment against one entry.
    guarded: Option<Guarded>,
}

/// The receiving ends the writer task owns, kept together so a caller cannot wire up one without the other.
pub(crate) struct Queue {
    packets: mpsc::Receiver<Queued>,
    /// Where a guarded packet's ending goes. Owned by the writer task because the writer is what produces
    /// one; the ingress task holds the other half and applies it to the allocator.
    terminals: mpsc::Sender<Terminal>,
}

pub(crate) struct Writer {
    sender: mpsc::Sender<Queued>,
}

impl Writer {
    /// Refuses rather than waits: a producer that blocks here would hold whatever lock or budget it took
    /// to build the packet, and the caller is the only thing that knows how to refund it.
    pub(crate) fn enqueue(
        &self,
        packet: Vec<u8>,
        guarded: Option<Guarded>,
    ) -> Result<(), Rejected> {
        self.sender
            .try_send(Queued { packet, guarded })
            .map_err(|_| Rejected)
    }
}

/// The two channels one session's TUN writer owns, and the receiving end the ingress task keeps.
pub(crate) fn channel() -> (Writer, Queue, mpsc::Receiver<Terminal>) {
    let (sender, packets) = mpsc::channel(QUEUE_DEPTH);
    let (terminals, settled) = mpsc::channel(TERMINAL_DEPTH);
    (Writer { sender }, Queue { packets, terminals }, settled)
}

/// How far one packet got.
enum Progress {
    /// On the wire, at the moment the write returned. The instant is read *after* the syscall succeeded and
    /// nowhere else: it is the only honest answer to "when was this Identification used".
    Written(Instant),
    /// The session ended while this packet was waiting for the TUN to become writable.
    Cancelled,
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
    let Queue { packets, terminals } = queue;
    let mut counts = Counts::default();
    // Drop all queue state before `terminals`; its closure is the ingress task's teardown fence.
    let result = {
        let mut packets = packets;
        writing(&fd, mtu, &mut packets, &terminals, &cancel, &mut counts).await
    };
    let Counts {
        oversized,
        written,
        settled,
    } = counts;
    report::stdout!("tun egress: written {written} rejected {oversized} settled {settled}");
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
    oversized: u64,
    written: u64,
    settled: u64,
}

/// The write loop itself, split out only so that everything it owns - including the packet it has in hand -
/// is dropped when it returns rather than at the end of [run].
async fn writing(
    fd: &AsyncFd<OwnedFd>,
    mtu: usize,
    packets: &mut mpsc::Receiver<Queued>,
    terminals: &mpsc::Sender<Terminal>,
    cancel: &CancellationToken,
    counts: &mut Counts,
) -> io::Result<()> {
    loop {
        let queued = tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(()),
            queued = packets.recv() => match queued {
                Some(queued) => queued,
                None => break Ok(()),
            },
        };
        // Only a guarded packet is a settlement: an unguarded one carries no Identification, so there is
        // nothing about it for the allocator to hear and nothing to count.
        let settlement = u64::from(queued.guarded.is_some());
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
        let progress = match write_all(fd, &queued.packet, cancel).await {
            Ok(progress) => progress,
            // A write that neither succeeded nor asked to be retried is fatal - see [write_all] - so no
            // terminal is sent for this packet and the session ends with its registration outstanding.
            Err(e) => break Err(e),
        };
        let at = match progress {
            Progress::Written(at) => Some(at),
            Progress::Cancelled => None,
        };
        match settle(terminals, queued.guarded, at) {
            Ok(true) => counts.settled += settlement,
            Ok(false) => break Ok(()),
            Err(e) => break Err(e),
        }
        match progress {
            Progress::Written(_) => counts.written += 1,
            Progress::Cancelled => break Ok(()),
        }
    }
}

/// One packet per write, because a TUN descriptor delivers a write as one packet or fails: there is no
/// short write to resume, so the only partial case is a multi-fragment datagram whose later fragments
/// fail, and that is the producer's to handle.
async fn write_all(
    fd: &AsyncFd<OwnedFd>,
    packet: &[u8],
    cancel: &CancellationToken,
) -> io::Result<Progress> {
    loop {
        let mut guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(Progress::Cancelled),
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
