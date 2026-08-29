use std::io;

use libc::{c_int, sched_param};
use vpnhotspotd::shared::scheduling::normalized;

/// Puts this thread back on the ordinary fair policy when it was launched on a non-interactive one.
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
