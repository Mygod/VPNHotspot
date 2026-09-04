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
        assert!(!Ended::Expected.retains_client_side(true, true));
        assert!(!Ended::Expected.retains_client_side(false, false));
        assert!(!Ended::Reported("a peer that reset".to_owned()).retains_client_side(false, true));
        assert!(!Ended::Failed {
            context: "shizuku.tcp_upstream_relay",
            error: io::Error::other("the daemon's own I/O"),
        }
        .retains_client_side(false, true));
    }
}
