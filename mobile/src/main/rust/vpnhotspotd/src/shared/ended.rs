//! How a worker stopped, and what its owner owes the app about it.
//!
//! Here rather than beside the worker table because [crate::shared::failure] produces one: a classified
//! failure answers what a task's ending means, and the table that collects those endings lives with the
//! owners that run tasks.

use std::io;

/// Why a worker stopped, and what its owner owes the app about it.
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
