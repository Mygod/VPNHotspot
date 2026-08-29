use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{Socket, State};
use vpnhotspotd::shared::admission::Admission;

use super::bridge::Attention;
use super::{lifetime, Engine, Flow};
use crate::report;
use crate::shizuku::output::Output;
use vpnhotspotd::shared::workers::{Ended, Terminal};

impl Engine {
    /// The next flow closing client-side whose client has now finished, if any has.
    pub(super) fn next_client_closed(&self) -> Option<Attention> {
        self.flows.iter().find_map(|(handle, held)| {
            (held.record.client_closing
                && self.sockets.get::<Socket>(*handle).state() == State::Closed)
                .then_some(Attention::ClientClosed {
                    handle: *handle,
                    incarnation: held.id,
                })
        })
    }

    /// Finishes one flow's client-side close, once that client has reached `Closed`.
    pub(crate) fn finish_client_close(
        &mut self,
        handle: SocketHandle,
        incarnation: u64,
        admission: &mut Admission,
    ) {
        if !self.flows.current(&handle, incarnation) {
            self.counters.ingress.stale += 1;
            return;
        }
        self.reclaim(handle, incarnation, admission);
    }

    /// Takes one finished transport task's terminal, which either leaves its flow closing client-side or ends
    /// it.
    pub(crate) fn close(
        &mut self,
        terminal: Terminal<SocketHandle>,
        admission: &mut Admission,
        output: &mut Output,
    ) {
        let Terminal { key, id, ended } = terminal;
        // Validate both handle and incarnation before a stale terminal can touch a successor flow.
        if !self.flows.current(&key, id) {
            self.counters.ingress.stale += 1;
            return;
        }
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
                // Nothing reads this owner's write half any more - it went with the completed transport - so
                // the bridge reports a broken pipe for it, and the crossing that sees one stops draining the
                // receive buffer for a flow with nowhere to put it.
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
        // Emitted before the socket is removed, because a reset is a packet the stack has not sent yet and a
        // smoltcp socket only advances when polled: removing it first would abort the connection and tell the
        // client nothing. Only when there was one, so a mass retirement does not poll once per flow.
        if reset {
            self.poll(output);
        }
        self.reclaim(key, id, admission);
    }

    /// Gives back everything one flow owns, once the task that served it has completed and its socket, bridge
    /// and channel endpoints can go.
    fn reclaim(&mut self, key: SocketHandle, id: u64, admission: &mut Admission) {
        self.outgoing.forget(key);
        let Some(flow) = self.flows.retire(&key, id) else {
            // Unreachable: every caller has validated this exact pair and nothing since has awaited.
            self.counters.ingress.stale += 1;
            return;
        };
        self.sockets.remove(key);
        // Submitted resolver transactions outlive their flow.
        let submitted = self.queries.len();
        let Flow {
            lease,
            bridge,
            serving,
            ..
        } = flow;
        // Drop bridge bytes before refunding the DNS delivery they may contain.
        drop(bridge);
        // Release flow-owned DNS state before its flow lease; submitted transactions remain table-owned.
        serving.close(admission);
        admission.release(lease);
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
