//! Serializes nonblocking TUN writes.
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::packet_writer::validate;
use vpnhotspotd::shared::protocol::IoErrorReportExt;

use crate::report;

/// Rejected because the writer task has already dropped its receiving end. There is no capacity refusal:
/// the session-owned handoff grows with the logical datagrams awaiting its sole serial writer.
#[derive(Debug)]
pub(crate) struct Rejected;

#[derive(Debug)]
pub(crate) struct Queued {
    packets: Vec<Vec<u8>>,
}

/// The logical-datagram receiving end owned by the writer task.
pub(crate) struct Queue {
    datagrams: mpsc::UnboundedReceiver<Queued>,
}

pub(crate) struct Writer {
    sender: mpsc::UnboundedSender<Queued>,
}

impl Writer {
    /// Hands off one complete logical datagram without waiting. The unbounded session queue cannot refuse
    /// for capacity; failure means the writer receiver is closed and none of this batch was handed over.
    pub(crate) fn enqueue(&self, packets: Vec<Vec<u8>>) -> Result<(), Rejected> {
        self.sender.send(Queued { packets }).map_err(|_| Rejected)
    }
}

/// An unbounded logical-datagram handoff for one session's sole serial TUN writer. The downstream client is
/// trusted, and stopping the session drops the receiver and every queued batch together.
pub(crate) fn channel() -> (Writer, Queue) {
    let (sender, datagrams) = mpsc::unbounded_channel();
    (Writer { sender }, Queue { datagrams })
}

/// How far one packet got.
enum Progress {
    Written,
    /// The session ended while this packet was waiting for the TUN to become writable.
    Cancelled,
}

pub(crate) async fn run(
    fd: Arc<AsyncFd<OwnedFd>>,
    mtu: usize,
    queue: Queue,
    cancel: CancellationToken,
) -> io::Result<()> {
    let Queue { mut datagrams } = queue;
    let mut counts = Counts::default();
    let result = writing(&fd, mtu, &mut datagrams, &cancel, &mut counts).await;
    let Counts {
        invalid_datagrams,
        written_packets,
    } = counts;
    report::stdout!(
        "tun egress: written packets {written_packets} invalid datagrams {invalid_datagrams}"
    );
    // Described here and delivered nowhere: attaching the report is this task's whole reporting duty, and
    // [crate::shizuku::app_session] is what puts it in front of the app - as the start call's terminal
    // `ErrorFrame` when that call is still owed one, and as a nonfatal when it is not. Raised here rather
    // than at the point of failure because this is the one place every fatal way out of the loop converges,
    // so the counts below are attached to whichever errno stopped it.
    result.map_err(|e| {
        e.with_report_context_details(
            "shizuku.tun_egress",
            [
                ("written_packets", written_packets),
                ("invalid_datagrams", invalid_datagrams),
            ],
        )
    })
}

#[derive(Default)]
struct Counts {
    invalid_datagrams: u64,
    written_packets: u64,
}

async fn writing(
    fd: &AsyncFd<OwnedFd>,
    mtu: usize,
    datagrams: &mut mpsc::UnboundedReceiver<Queued>,
    cancel: &CancellationToken,
    counts: &mut Counts,
) -> io::Result<()> {
    'datagrams: loop {
        let queued = tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(()),
            queued = datagrams.recv() => match queued {
                Some(queued) => queued,
                None => break Ok(()),
            },
        };
        // Validate every packet before the first write. A packetization defect therefore rejects the complete
        // logical datagram instead of writing a valid prefix and discovering the bad fragment afterward.
        if let Some((index, packet, error)) =
            queued
                .packets
                .iter()
                .enumerate()
                .find_map(|(index, packet)| {
                    validate(packet, mtu)
                        .err()
                        .map(|error| (index, packet, error))
                })
        {
            counts.invalid_datagrams += 1;
            report::message_with_details(
                "shizuku.tun_egress",
                format!("refused a datagram the daemon built: {error:?}"),
                "packetization",
                [
                    ("packet_index", index),
                    ("packet_bytes", packet.len()),
                    ("packet_count", queued.packets.len()),
                ],
            );
            continue;
        }
        // This task is the only writer, and it consumes the whole batch before receiving the next one, so
        // fragments belonging to different logical datagrams cannot interleave.
        for packet in queued.packets {
            match write_all(fd, &packet, cancel).await? {
                Progress::Written => counts.written_packets += 1,
                Progress::Cancelled => break 'datagrams Ok(()),
            }
        }
    }
}

/// One packet per write, because a TUN descriptor delivers a write as one packet or fails: there is no short
/// write to resume. A fatal descriptor failure can leave a prefix of this datagram on the wire, but no other
/// datagram can be interleaved with that prefix because [writing] owns the batch until it finishes or exits.
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
            Ok(Ok(count)) if count == packet.len() => return Ok(Progress::Written),
            Ok(Ok(count)) => {
                return Err(io::Error::other(format!(
                    "the TUN took {count} of a {}-byte packet, which a packet-atomic \
                     descriptor may not do",
                    packet.len()
                )))
            }
            Ok(Err(e)) => return Err(e),
            // The kernel queue was full after all; wait for writability again.
            Err(_would_block) => continue,
        }
    }
}
