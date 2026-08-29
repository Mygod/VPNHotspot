use std::io;

use crate::socket::is_kernel_icmp_error;

/// Why a send was refused, in terms the relays act on rather than in errnos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The transmit queue is full. Waiting here would stall TUN ingress for every other client, so the packet is
    /// dropped rather than retried - which is what a relay owes an unreliable transport.
    Blocked,
    /// DF was set and the packet is larger than the upstream path MTU. The MTU itself is not in the errno, so the
    /// caller reads it off the error queue: that is what makes an honest Fragmentation Needed possible rather
    /// than a guessed one.
    TooBig,
    /// The selected network is gone, so this socket is bound to something that no longer exists and can never
    /// send again. Nothing here can be retried; the socket has to go.
    NetworkGone,
    /// An error the remote's own network reported, so it is per-destination and the socket survives it.
    Unreachable,
    /// An errno nobody named. Diagnosable only by being printed, and only once - this path is driven by whoever
    /// puts packets on the interface, so a report per packet would be a flood.
    Unexpected,
}

/// Reads one send error.
pub(crate) fn classify(e: &io::Error) -> Failure {
    if e.kind() == io::ErrorKind::WouldBlock {
        Failure::Blocked
    } else if e.raw_os_error() == Some(libc::EMSGSIZE) {
        Failure::TooBig
    } else if e.raw_os_error() == Some(libc::ENONET) {
        Failure::NetworkGone
    } else if is_kernel_icmp_error(e) {
        Failure::Unreachable
    } else {
        Failure::Unexpected
    }
}
