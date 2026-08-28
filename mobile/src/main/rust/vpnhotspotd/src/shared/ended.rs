//! How a worker stopped, and what its owner owes the app about it.
//!
//! Here rather than beside the worker table because [crate::shared::failure] produces one: a classified
//! failure answers what a task's ending means, and the table that collects those endings lives with the
//! owners that run tasks.

use std::io;

/// Why a worker stopped, and what its owner owes the app about it.
#[derive(Debug)]
pub enum Ended {
    /// The owner asked for it, or the exchange ended the way its protocol says it ends. Nothing to say.
    Expected,
    /// An outcome worth one line per record - a peer that reset, a socket that stopped being usable. Printed
    /// by the owner, which is what knows the record's name, and once per record rather than per packet.
    Reported(String),
    /// The daemon's own I/O or the task itself failed. Raised as a structured report rather than logged,
    /// because nothing about it is the peer's doing.
    Failed {
        context: &'static str,
        error: io::Error,
    },
}

impl Ended {
    /// Whether this ending leaves the flow's client-facing side to finish rather than ending the flow.
    ///
    /// The one decision a terminating flow's owner makes when a transport task finishes, taken here because
    /// it is a classification rather than I/O - and because getting it wrong is silent. That task completes
    /// as soon as *its* ordered work is done: with a bounded byte bridge between the two halves that means as
    /// soon as its last bytes and its ordered end of stream are **in the bridge**, not delivered, so a task
    /// that ended cleanly routinely leaves its client everything the bridge is holding. Keeping the
    /// client-facing side keeps all of it - the socket, the bridge, the grant - and the owner goes on
    /// delivering; ending discards it.
    ///
    /// `cancelled` is whether somebody asked this task to stop, and `opened` whether the client's own half
    /// ever got past its handshake and is not already closed. Both are exclusions of the same kind: there is
    /// no client-side close left here to protect. A cancelled task also reports [Ended::Expected], and there
    /// the socket has already been aborted by whoever cancelled it; a half that never opened, or is already
    /// closed, has no closing that could be cut short.
    pub fn retains_client_side(&self, cancelled: bool, opened: bool) -> bool {
        matches!(self, Self::Expected) && !cancelled && opened
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_terminal_from_an_open_flow_nobody_stopped_leaves_the_client_side() {
        assert!(Ended::Expected.retains_client_side(false, true));
    }

    #[test]
    fn everything_with_no_client_side_close_left_to_protect_ends_the_flow() {
        // A task somebody stopped: its socket has already been aborted.
        assert!(!Ended::Expected.retains_client_side(true, true));
        // A client half that never opened, or that is already closed.
        assert!(!Ended::Expected.retains_client_side(false, false));
        // And an ending that is not a clean completion at all, which resets its client either way.
        assert!(!Ended::Reported("a peer that reset".to_owned()).retains_client_side(false, true));
        assert!(!Ended::Failed {
            context: "shizuku.tcp_upstream_relay",
            error: io::Error::other("the daemon's own I/O"),
        }
        .retains_client_side(false, true));
    }
}
