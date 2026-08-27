//! The one thing this daemon fixes about how it was launched.
//!
//! Which policy may be changed into which is [vpnhotspotd::shared::scheduling]'s, because it is a decision;
//! what is here is the pair of syscalls that read and write this thread's own policy, kept at the boundary
//! that needs them rather than behind a general "sys" module.
//!
//! **Before the runtime, not inside it.** A Tokio worker thread inherits the policy of the thread that
//! created it, and the multi-threaded builder creates every one of them while `build()` runs
//! (`tokio 1.53.1`, `runtime/builder.rs:2052-2054`). So this is called on the main thread before the runtime
//! is built: the workers then inherit the normalized policy, and so does every blocking thread spawned later
//! from one of them. Calling it from inside the runtime would leave the threads that actually run the
//! dataplane on the policy that was inherited.

use std::io;

use libc::{c_int, sched_param};
use vpnhotspotd::shared::scheduling::normalized;

/// Puts this thread back on the ordinary fair policy when it was launched on a non-interactive one.
///
/// `Ok(None)` means the inherited policy was already this daemon's to keep. `Ok(Some(policy))` names what it
/// was normalized away from, which is worth saying out loud once: it is a fact about how the app launched
/// this process rather than about this process.
///
/// A failure is returned rather than swallowed and rather than fatal. The daemon runs correctly under any
/// fair-class policy - this is latency, not correctness - so a kernel or seccomp policy that refuses the
/// change is a session that still starts, with a line on this process's startup output saying which policy
/// it is running under. There is no structured report to send yet: this runs before the runtime, the control
/// socket and the conversation's reporter all exist.
pub(crate) fn normalize() -> io::Result<Option<c_int>> {
    // SAFETY: both calls name the calling thread with pid 0 and read or write only the parameter block
    // below, which is initialized here and outlives the call.
    let inherited = unsafe { libc::sched_getscheduler(0) };
    if inherited < 0 {
        return Err(io::Error::last_os_error());
    }
    let Some(wanted) = normalized(inherited) else {
        return Ok(None);
    };
    // Zero is the only static priority a fair-class policy accepts, and the nice value - which is what
    // actually orders fair-class threads - is deliberately not touched: `sched_setscheduler` keeps the
    // caller's own.
    let parameters = sched_param { sched_priority: 0 };
    if unsafe { libc::sched_setscheduler(0, wanted, &parameters) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Some(inherited))
}
