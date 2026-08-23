//! What a refused egress send means.
//!
//! Shared by the UDP relay and Echo because both send on selected-network sockets and both see the same errnos,
//! and the mapping from errno to meaning is the part worth having in one place. What each *does* about a meaning
//! differs - a UDP mapping cancels itself when its network is gone, while Echo cancels every ping socket at once
//! - so the actions stay with the relays and only the reading lives here.

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
///
/// The order matters where the cases overlap: `EMSGSIZE` is also one of the errnos a kernel-reported ICMP error
/// arrives as, so the local refusal has to be recognised first or a path-MTU failure would be mistaken for a
/// remote's complaint and never reported to the client.
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
