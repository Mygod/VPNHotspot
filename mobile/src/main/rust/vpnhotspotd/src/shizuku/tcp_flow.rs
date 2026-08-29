//! Splices an upstream TCP socket to a bounded bridge; retirement is abortive and cancellation-safe.
use std::io;
use std::time::Duration;

use socket2::SockRef;
use tokio::io::copy_bidirectional_with_sizes;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::bridge::Worker;
use vpnhotspotd::shared::flow::FlowId;
use vpnhotspotd::shared::workers::Ended;

use smoltcp::iface::SocketHandle;
use vpnhotspotd::shared::flow_budget::READ_CHUNK;

use crate::report;

/// One flow, named on the requests its DNS-over-TCP transport makes of the ingress owner.
pub(crate) type Event = FlowId<SocketHandle>;

/// `sweep` is the engine's own token rather than this flow's: it is cancelled only when the whole table is
/// being retired, which is the one case where this socket must be torn down abortively rather than closed.
pub(crate) async fn splice(
    mut upstream: TcpStream,
    mut bridge: Worker,
    cancel: CancellationToken,
    sweep: CancellationToken,
) -> Ended {
    // One scratch per direction, at the size the flow was charged for. The bridge is a byte stream, so this
    // is no longer a segment boundary - it is how much of the upstream socket one read takes.
    let failure = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        copied = copy_bidirectional_with_sizes(&mut upstream, &mut bridge, READ_CHUNK, READ_CHUNK) => {
            match copied {
                // Both directions reached the end of their stream and both were shut down: the upstream has
                // this flow's FIN, and the bridge has reported the end of the stream to the engine strictly
                // after the last byte it delivered.
                Ok(_) => None,
                // Named for the upstream because that is what it is. The bridge's own halves cannot fail
                // while this task runs - the engine drops its half only after joining this one - so every
                // error this copy can produce came from the selected-network socket.
                Err(e) => Some(("shizuku.tcp_upstream_relay", e)),
            }
        }
    };
    // Abortive only on a sweep, and that distinction is the whole point: an ordinary close is a stream that
    // ended, while a swept one must not keep transmitting queued bytes, retransmissions and the FIN over the
    // `Network` the session is leaving. Set before the socket is dropped, because dropping it is the close.
    if sweep.is_cancelled() {
        if let Err(e) = SockRef::from(&upstream).set_linger(Some(Duration::ZERO)) {
            // Reported and closed anyway: the residue is a drained send queue on a network the session is
            // leaving, which is not worth ending a working session over.
            report::io("shizuku.tcp_sweep", e);
        }
    }
    // Explicit, because dropping this is the close: the engine joins this task before it may refund, so the
    // descriptor has to be gone by the time this returns rather than merely unreferenced. The refund itself
    // may come later still - after a clean completion whose client is still open, the flow goes on closing
    // client-side rather than ending.
    drop(upstream);
    // And the bridge with it, which leaves everything this task wrote readable and then reports the end of
    // the stream - so the engine goes on delivering exactly what it was delivering.
    drop(bridge);
    match failure {
        Some((context, error)) if !expected(&error) => Ended::Failed { context, error },
        // A peer that resets, times out or vanishes is the network being the network, and every flow's client
        // learns of it the one way a terminated flow can say it: a reset. One line names it, and the engine
        // prints that rather than raising a report per hostile peer.
        Some((_, error)) => Ended::Reported(error.to_string()),
        None => Ended::Expected,
    }
}

/// Whether a failure is the peer ending the exchange rather than the daemon failing at it. Only these are
/// classified as expected: anything else is the daemon's own I/O going wrong and is raised as a report.
fn expected(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NetworkDown
    )
}
