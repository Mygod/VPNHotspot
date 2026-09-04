use super::bridge::Attention;
use super::{lifetime, Engine, Flow};
use crate::report;
use crate::shizuku::output::Output;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{Socket, State};
use vpnhotspotd::shared::lifetime::owes_reset;
use vpnhotspotd::shared::workers::{Ended, Terminal};

impl Engine {
    /// Finds a retained client flow that is closed and no longer owes a reset.
    pub(super) fn next_client_closed(&self) -> Option<Attention> {
        self.flows.iter().find_map(|(handle, held)| {
            let socket = self.sockets.get::<Socket>(*handle);
            (held.record.client_closing && socket.state() == State::Closed && !owes_reset(socket))
                .then_some(Attention::ClientClosed {
                    handle: *handle,
                    incarnation: held.id,
                })
        })
    }

    /// Finishes one flow's client-side close, once that client has reached `Closed`.
    pub(crate) fn finish_client_close(&mut self, handle: SocketHandle, incarnation: u64) {
        if !self.flows.current(&handle, incarnation) {
            self.counters.ingress.stale += 1;
            return;
        }
        self.reclaim(handle, incarnation);
    }

    /// Takes one finished transport task's terminal, which either leaves its flow closing client-side or ends
    /// it.
    pub(crate) fn close(&mut self, terminal: Terminal<SocketHandle>, output: &mut Output) {
        let Terminal { key, id, ended } = terminal;
        // Validate both handle and incarnation before a stale terminal can touch a successor flow.
        if !self.flows.current(&key, id) {
            self.counters.ingress.stale += 1;
            return;
        }
        // Worker completion closes the upstream descriptor; client-only state may remain.
        // Keep a clean client-facing close until its remaining bytes and FIN are acknowledged.
        let cancelled = self
            .flows
            .get(&key)
            .is_some_and(|held| held.cancel.is_cancelled());
        if ended.retains_client_side(
            cancelled,
            lifetime::opened(self.sockets.get::<Socket>(key).state()),
        ) {
            if let Some(held) = self.flows.get_mut(&key) {
                held.record.client_closing = true;
                // Stop draining client bytes after the upstream writer is gone.
                held.record.bridge.stop_sending();
            }
            self.counters.client_closing += 1;
            return;
        }
        let reset = match ended {
            Ended::Expected => false,
            Ended::Reported(reason) => {
                // once per flow, not per packet, so this cannot flood
                report::stdout!("tcp flow closed: {reason}");
                self.reset(key)
            }
            Ended::Failed { context, error } => {
                match self.flows.get(&key) {
                    Some(flow) => report::io_with_details(
                        context,
                        error,
                        [
                            ("client", flow.record.client),
                            ("destination", flow.record.destination),
                        ],
                    ),
                    None => report::io(context, error),
                }
                self.reset(key)
            }
        };
        // Poll a reset before considering socket removal.
        if reset {
            self.poll(output);
        }
        // A full handoff leaves the reset in its socket; retain the flow until it is emitted.
        if owes_reset(self.sockets.get::<Socket>(key)) {
            if let Some(held) = self.flows.get_mut(&key) {
                held.record.client_closing = true;
            }
            return;
        }
        self.reclaim(key, id);
    }

    /// Releases one joined flow's socket and buffers.
    fn reclaim(&mut self, key: SocketHandle, id: u64) {
        self.outgoing.forget(key);
        let Some(flow) = self.flows.retire(&key, id) else {
            // Unreachable: every caller has validated this exact pair and nothing since has awaited.
            self.counters.ingress.stale += 1;
            return;
        };
        self.sockets.remove(key);
        // The set the stack's poll time is taken over just lost a member.
        self.stack_changed();
        // Submitted resolver transactions outlive their flow.
        let submitted = self.queries.len();
        let Flow {
            bridge,
            serving,
            armed,
            ..
        } = flow;
        // Remove the retired flow's deadline entry.
        if let Some(armed) = armed {
            self.idle.disarm(key, armed);
        }
        // Drop bridge bytes before the DNS delivery they may contain.
        drop(bridge);
        // Release flow-owned DNS state; submitted transactions remain table-owned.
        serving.close();
        debug_assert_eq!(
            self.queries.len(),
            submitted,
            "reclaiming a flow ends nothing a submitted query owns"
        );
        self.counters.closed += 1;
    }

    /// Tells the client its upstream half is gone, the one way a terminated flow can: a reset. This is also
    /// the unreachable-destination path, since the handshake already completed. `false` means there was no
    /// socket left to say it with.
    fn reset(&mut self, handle: SocketHandle) -> bool {
        match self.socket(handle) {
            Some(socket) => {
                socket.abort();
                self.counters.ingress.reset += 1;
                true
            }
            None => false,
        }
    }
}
