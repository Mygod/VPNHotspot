//! Serializes nonblocking TUN writes.
use std::io;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::packet_writer::validate;
use vpnhotspotd::shared::protocol::IoErrorReportExt;
use vpnhotspotd::shared::tun_handoff::{Progress, Queue};

use crate::report;
use crate::shizuku::tun_reader::now;

/// Why the batch loop moved on, or stopped.
enum Ending {
    Finished,
    Cancelled,
    Failed(io::Error),
}

/// Why one batch stopped and how far it reached the wire.
struct Batched {
    ending: Ending,
    progress: Progress,
}

pub(crate) async fn run(
    fd: Arc<AsyncFd<OwnedFd>>,
    mtu: usize,
    queue: Queue,
    cancel: CancellationToken,
) -> io::Result<()> {
    let mut queue = queue;
    let mut counts = Counts::default();
    let result = writing(&fd, mtu, &mut queue, &cancel, &mut counts).await;
    let Counts {
        invalid_datagrams,
        written_packets,
        settlement_enqueued,
    } = counts;
    report::stdout!(
        "tun egress: written packets {written_packets} invalid datagrams {invalid_datagrams} \
         settlement-enqueued {settlement_enqueued}"
    );
    // All fatal exits converge here so the terminal report includes the final counters.
    result.map_err(|e| {
        e.with_report_context_details(
            "shizuku.tun_egress",
            [
                ("written_packets", written_packets),
                ("invalid_datagrams", invalid_datagrams),
                ("settlement_enqueued", settlement_enqueued),
            ],
        )
    })
}

#[derive(Default)]
struct Counts {
    invalid_datagrams: u64,
    written_packets: u64,
    /// Endings enqueued for the owner, not confirmed as applied.
    settlement_enqueued: u64,
}

async fn writing(
    fd: &AsyncFd<OwnedFd>,
    mtu: usize,
    queue: &mut Queue,
    cancel: &CancellationToken,
    counts: &mut Counts,
) -> io::Result<()> {
    loop {
        let Some(batch) = queue.next(cancel).await else {
            break Ok(());
        };
        let guarded = batch.guarded();
        // Validate the whole batch before writing any prefix.
        let invalid = batch
            .packets()
            .iter()
            .enumerate()
            .find_map(|(index, packet)| {
                validate(packet, mtu)
                    .err()
                    .map(|error| (index, packet.len(), error))
            });
        let Batched { ending, progress } = match invalid {
            Some((index, bytes, error)) => {
                counts.invalid_datagrams += 1;
                report::message_with_details(
                    "shizuku.tun_egress",
                    format!("refused a datagram the daemon built: {error:?}"),
                    "packetization",
                    [
                        ("packet_index", index),
                        ("packet_bytes", bytes),
                        ("packet_count", batch.packets().len()),
                    ],
                );
                Batched {
                    ending: Ending::Finished,
                    // Refused before its first write, so no packet of it reached the wire.
                    progress: Progress::default(),
                }
            }
            // The sole writer finishes a batch before receiving the next.
            None => write_batch(fd, batch.into_packets(), cancel, counts).await,
        };
        // Settle before taking the next batch. Partial writes use the last successful write time because their
        // Identification reached the wire.
        if let Some(terminal) = progress.terminal(guarded) {
            if !queue.settle(terminal, cancel).await {
                // Preserve a descriptor failure even if settlement can no longer be delivered.
                break match ending {
                    Ending::Failed(e) => Err(e),
                    _ => Ok(()),
                };
            }
            counts.settlement_enqueued += 1;
        }
        match ending {
            Ending::Finished => {}
            Ending::Cancelled => break Ok(()),
            Ending::Failed(e) => break Err(e),
        }
    }
}

/// Writes one complete logical datagram, in order, and says how far it got.
async fn write_batch(
    fd: &AsyncFd<OwnedFd>,
    packets: Vec<Vec<u8>>,
    cancel: &CancellationToken,
    counts: &mut Counts,
) -> Batched {
    let mut progress = Progress::default();
    for packet in packets {
        match write_all(fd, &packet, cancel).await {
            // Shared progress owns partial-datagram Identification settlement.
            Ok(at) => {
                if !progress.packet(at) {
                    return Batched {
                        ending: Ending::Cancelled,
                        progress,
                    };
                }
                counts.written_packets += 1;
            }
            Err(e) => {
                return Batched {
                    ending: Ending::Failed(e),
                    progress,
                }
            }
        }
    }
    Batched {
        ending: Ending::Finished,
        progress,
    }
}

/// Writes one packet atomically. `None` means cancellation before the write; [writing] prevents inter-batch
/// interleaving even when a later packet fails.
async fn write_all(
    fd: &AsyncFd<OwnedFd>,
    packet: &[u8],
    cancel: &CancellationToken,
) -> io::Result<Option<Instant>> {
    loop {
        let mut guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(None),
            guard = fd.writable() => guard?,
        };
        match guard
            .try_io(|inner| rustix::io::write(inner.get_ref(), packet).map_err(io::Error::from))
        {
            // Date Identification exposure after the successful descriptor write.
            Ok(Ok(count)) if count == packet.len() => return Ok(Some(now())),
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
