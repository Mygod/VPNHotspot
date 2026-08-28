//! One terminated TCP flow's upstream half: the selected-network socket, and the copy between it and the
//! client-side stack.
//!
//! There is nothing of this daemon's between the two. The engine hands this task an ordinary bounded Tokio
//! stream - one half of the flow's bridge - and what runs against it is Tokio's own bidirectional copy, so
//! every property this half needs is the library's:
//!
//! - **backpressure**, in both directions and lossless. Client to upstream: when this task cannot keep up,
//!   the bridge fills, the engine stops draining the stack's receive buffer, and the client's advertised
//!   window closes - and the engine learns that room is back because the bridge wakes it, rather than leaving
//!   the window closed until something unrelated happens. Upstream to client: when the engine cannot keep up,
//!   the bridge fills, the copy stops reading the upstream socket, and the *upstream's* window closes. Up to
//!   [vpnhotspotd::shared::flow_budget::BRIDGE_BUFFER] bytes may be in the bridge ahead of the engine, which
//!   is what lets this half read while the engine is writing what it read before rather than waiting to be
//!   told each piece was consumed.
//! - **half-close, both ways.** `copy_bidirectional` shuts down the *other* side when a reader reaches the
//!   end of its stream and goes on copying the direction that is still open, which is exactly what a
//!   request-then-response protocol needs: the client finishes asking, its FIN reaches the upstream, and the
//!   response still comes back.
//!
//! Neither direction drops data to relieve pressure, which is what separates a terminated stream from the
//! relayed datagrams next door: a datagram nobody promised to deliver may be dropped, a byte in an
//! acknowledged stream may not. **An abortive ending is the exception.** A flow reset by a retirement, an
//! idle expiry or an upstream that failed or vanished discards whatever the engine has not written into the
//! client's send buffer yet, which is up to one bridge's worth. The client learns of it the one way a
//! terminated flow can say it, a reset, and nothing about the clean path changes: an orderly end of stream
//! reaches the engine only after every byte queued in front of it, and a clean completion leaves the flow
//! closing client-side so the engine goes on delivering what this task left in the bridge.
//!
//! The copy therefore races the flow's token, and that is not defensive. It can block for as long as a peer
//! chooses: a write into a full send buffer waits on a remote that stopped reading, a read waits on one that
//! says nothing, and a write into the bridge waits on an engine that has stopped draining in order to retire
//! this very flow. A retirement has to be abortive, so none of them may be what a retirement waits for - and
//! this task *finishing* is what the engine joins. Finishing is not the same as the flow being removed: after
//! a clean completion whose client is still closing, the flow keeps its client-facing socket and its charge
//! until that close ends. See `shizuku/tcp/terminal.rs`.

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
///
/// Not a wake: nothing travels between a flow's task and its owner to say payload is waiting, because the
/// flow's own bridge is what wakes the owner - see [crate::shizuku::tcp::bridge].
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
