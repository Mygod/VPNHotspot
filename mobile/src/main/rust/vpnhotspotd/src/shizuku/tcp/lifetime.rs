use std::time::{Duration, Instant};

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::Socket;

use super::{Engine, Flow};
use crate::shizuku::flow_setup::Sockets;
use crate::shizuku::output::Output;
use vpnhotspotd::shared::lifetime::rearmed;
pub(super) use vpnhotspotd::shared::lifetime::{deadline, opened};
use vpnhotspotd::shared::workers::Workers;

/// Rearms one exact flow from the state its socket is really in.
pub(super) fn rearm(
    flows: &mut Workers<SocketHandle, Flow>,
    sockets: &Sockets,
    handle: SocketHandle,
    incarnation: u64,
    now: Instant,
) {
    // Both halves, because smoltcp reuses handles: a packet or a delivery naming a replaced flow's handle
    // would otherwise hand the successor its predecessor's lease of life.
    if !flows.current(&handle, incarnation) {
        return;
    }
    // Already retiring and waiting only on its transport task. A refreshed deadline would outlive the
    // record it belongs to, and [Engine::next_deadline] excludes it from the schedule anyway.
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
    }
}

impl Engine {
    /// When this engine next needs to run regardless of traffic.
    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        let stack = self
            .interface
            .poll_delay(self.now(), &self.sockets)
            // Added to the runtime's own reading of now, because that is the clock the owner's
            // `sleep_until` measures this against - see [crate::shizuku::tun_reader::run].
            .map(|delay| {
                tokio::time::Instant::now().into_std() + Duration::from_micros(delay.total_micros())
            });
        let idle = self
            .flows
            .values()
            .filter(|held| !held.cancel.is_cancelled())
            .filter_map(|held| held.record.deadline)
            .min();
        [stack, idle].into_iter().flatten().min()
    }

    /// Retires every flow whose outer idle deadline has passed.
    pub(crate) fn expire(&mut self, now: Instant, output: &mut Output) {
        // Walked over the round-robin order rather than into a list of what is due. That order is registered
        // with every admitted flow and deregistered with every closed one, so it already holds each live
        // handle exactly once - see [vpnhotspotd::shared::flow::admit_flow] - and a list built here would be
        // scratch sized by traffic that no lease covers, allocated on a path a stopping session still runs.
        // Destructured because the walk reads one field while the steps below write three others.
        let expired = {
            let Engine {
                flows,
                sockets,
                outgoing,
                counters,
                ..
            } = self;
            debug_assert_eq!(
                outgoing.len(),
                flows.len(),
                "the round-robin order indexes exactly the live flows"
            );
            let mut expired = 0u64;
            for handle in outgoing.iter() {
                let Some(held) = flows.get_mut(handle) else {
                    continue;
                };
                // Do not begin retirement twice; client-side closing remains live until its deadline.
                if held.cancel.is_cancelled()
                    || !held.record.deadline.is_some_and(|deadline| deadline <= now)
                {
                    continue;
                }
                // Cancelling is the whole of it, and the whole of it is abortive: every wait a worker can be
                // in races this token, and whatever either direction of the bridge still holds is discarded
                // with the bridge when the flow is reclaimed.
                held.cancel.cancel();
                // At most one reset per expired flow, built while the socket that carries it still exists,
                // so a client fails fast instead of waiting out its own retransmissions. Counted only where the
                // stack really has somewhere to send one: a socket with no remote endpoint - one still
                // listening, or one already closed - is aborted silently, and counting that would overstate
                // what was sent.
                let socket = sockets.get_mut::<Socket>(*handle);
                if socket.remote_endpoint().is_some() {
                    counters.ingress.reset += 1;
                }
                socket.abort();
                expired += 1;
            }
            counters.expired += expired;
            expired
        };
        if expired == 0 {
            return;
        }
        // Under the stamp current now and before anything is freed, for the same reason [Engine::retire]
        // polls here: a reset is a packet the stack has not built yet, and removing the socket first would
        // abort the connection and tell the client nothing. Whether it reaches the wire is the writer's
        // ordinary business - a config that changes the stamp before the writer dequeues it purges this
        // packet exactly as it purges every other one of the retired stamp.
        self.poll(output);
    }
}
