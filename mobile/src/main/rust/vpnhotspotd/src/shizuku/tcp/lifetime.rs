use std::time::{Duration, Instant};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::Socket;

use super::{Engine, Flow};
use crate::shizuku::flow_setup::Sockets;
use vpnhotspotd::shared::deadlines::Deadlines;
use vpnhotspotd::shared::lifetime::rearmed;
pub(super) use vpnhotspotd::shared::lifetime::{deadline, opened};
use vpnhotspotd::shared::workers::{Held, Workers};

/// Synchronizes one flow's remembered idle deadline with the ordered index.
pub(super) fn reindex(
    held: &mut Held<Flow>,
    idle: &mut Deadlines<SocketHandle>,
    handle: SocketHandle,
) {
    // Cancelled flows and smoltcp-owned timeout phases have no owner idle deadline.
    let due = if held.cancel.is_cancelled() {
        None
    } else {
        held.record.deadline
    };
    match due {
        Some(due) => idle.arm(handle, held.record.armed, due),
        None => {
            if let Some(armed) = held.record.armed {
                idle.disarm(handle, armed);
            }
        }
    }
    held.record.armed = due;
}

/// Rearms one exact flow from the state its socket is really in.
pub(super) fn rearm(
    flows: &mut Workers<SocketHandle, Flow>,
    sockets: &Sockets,
    idle: &mut Deadlines<SocketHandle>,
    handle: SocketHandle,
    incarnation: u64,
    now: Instant,
) {
    // Handle plus incarnation fences smoltcp handle reuse.
    if !flows.current(&handle, incarnation) {
        return;
    }
    // Already retiring; do not rearm past its worker terminal.
    let Some(held) = flows.get(&handle) else {
        return;
    };
    if held.cancel.is_cancelled() {
        return;
    }
    // Read phase and bridge ending together; they briefly differ while a received FIN awaits extraction.
    let state = sockets.get::<Socket>(handle).state();
    let armed = rearmed(
        held.record.deadline,
        state,
        held.record.bridge.ending(state),
        now,
    );
    if let Some(held) = flows.get_mut(&handle) {
        held.record.deadline = armed;
        reindex(held, idle, handle);
    }
}

impl Engine {
    /// When this engine next needs to run regardless of traffic. `accepting` is whether the interface handoff
    /// can take another datagram right now.
    pub(crate) fn next_deadline(&mut self, accepting: bool) -> Option<Instant> {
        if self.stack_stale {
            self.stack = self
                .interface
                .poll_delay(self.now(), &self.sockets)
                // Cache an absolute deadline in the runtime clock used by `sleep_until`.
                .map(|delay| {
                    tokio::time::Instant::now().into_std()
                        + Duration::from_micros(delay.total_micros())
                });
            self.stack_stale = false;
        }
        // `stack_changed` invalidates this cache after every relevant mutation. While output is full, omit
        // smoltcp's usually-zero send wake and wait for handoff capacity instead.
        let stack = accepting.then_some(self.stack).flatten();
        [stack, self.idle.next()].into_iter().flatten().min()
    }

    /// Retires due idle flows without polling. Aborted sockets retain resets for a later delivering turn.
    pub(crate) fn expire(&mut self, now: Instant) {
        // Consume only indexed due rows; allocate no traffic-sized scratch list.
        let mut expired = 0u64;
        while let Some(handle) = self.idle.due(now) {
            let retiring = {
                let Engine {
                    flows,
                    sockets,
                    counters,
                    ..
                } = self;
                match flows.get_mut(&handle) {
                    Some(held) => {
                        // `due` removed this index entry.
                        held.record.armed = None;
                        // A flow cancelled since arming is only disarmed here.
                        if held.cancel.is_cancelled()
                            || !held.record.deadline.is_some_and(|deadline| deadline <= now)
                        {
                            false
                        } else {
                            // Expiry is abortive; every worker wait races this token.
                            held.cancel.cancel();
                            // Count a reset only when a remote endpoint can receive one.
                            let socket = sockets.get_mut::<Socket>(handle);
                            if socket.remote_endpoint().is_some() {
                                counters.ingress.reset += 1;
                            }
                            socket.abort();
                            true
                        }
                    }
                    None => false,
                }
            };
            expired += u64::from(retiring);
            self.rearm_index(handle);
        }
        self.counters.expired += expired;
        if expired == 0 {
            return;
        }
        // Aborts make the stack immediately pollable.
        self.stack_changed();
    }
}
