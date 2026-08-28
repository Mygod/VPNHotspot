//! This engine's side of the packet owner's boundary: where its tables are, and nothing about what to do
//! with them.
//!
//! Every decision one client segment provokes - the order the steps run in, which transition counts as a
//! reset, which figure moves, what a refused terminal tail is reported as, whether the socket or the worker
//! goes first - is [vpnhotspotd::shared::ingress]'s, because that is a platform-neutral module the host
//! builds and tests and this is a binary target that runs none. What is left here is field access: a poll, a
//! device slot, an iterator over the table, one flow's pieces reachable together, the counters, the flow
//! builder, and the reporter.
//!
//! The one step whose *body* a host cannot execute is [Handling::open], which builds the client-side socket
//! and bridge, takes the charged grant and starts the flow's transport task. Only an ordinary relay's task
//! then opens an upstream descriptor; a virtual-DNS transport opens none. Its ordering - when it may run, and
//! what happens to what it built when the stack then refuses the segment - is decided in the shared module
//! and covered there.

use std::net::SocketAddr;
use std::time::Instant;

use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::Socket;
use smoltcp::time::Instant as SmolInstant;
use vpnhotspotd::shared::admission::Admission;
use vpnhotspotd::shared::ingress::{self, Counters, Held};
use vpnhotspotd::shared::proto::daemon::DaemonErrorReport;
use vpnhotspotd::shared::tcp_wire::Segment;

use super::Engine;
use crate::report;
use crate::shizuku::output::Output;

impl Engine {
    /// One client segment, sequenced by [vpnhotspotd::shared::ingress::accept] over the tables below.
    pub(crate) fn accept(
        &mut self,
        packet: &[u8],
        segment: Segment,
        resolver: bool,
        now: Instant,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        // Pinned once, so every settle in that call polls the stack at one instant.
        let at = self.now();
        let mut handling = Handling {
            engine: self,
            output,
            admission,
            resolver,
            now,
            at,
        };
        ingress::accept(&mut handling, packet, &segment);
    }
}

impl ingress::Owner for Engine {
    type Handle = SocketHandle;

    fn held(&mut self, handle: SocketHandle) -> Option<Held<'_>> {
        // Two disjoint fields, which is what lets the socket be reached while the record is held.
        let Engine { flows, sockets, .. } = self;
        let held = flows.get_mut(&handle)?;
        Some(Held {
            socket: sockets.get_mut::<Socket>(handle),
            bridge: &mut held.record.bridge,
            cancel: &held.cancel,
            deadline: &mut held.record.deadline,
            client: held.record.client,
            destination: held.record.destination,
        })
    }

    fn counters(&mut self) -> &mut Counters {
        &mut self.counters.ingress
    }

    /// The one primitive whose body no host test can execute: it reaches this process's reporter
    /// registries, and on their own failure path a platform log this daemon only has on Android. It is
    /// deliberately one line and decides nothing - what is delivered, when, and from which of the two paths
    /// that can raise one, is all [vpnhotspotd::shared::ingress]'s and covered there.
    fn deliver(&mut self, report: DaemonErrorReport) {
        report::report(report);
    }

    fn reclaim_closed(&mut self) {
        Engine::reclaim_closed(self);
    }
}

/// One `accept` call's borrow of the engine and of everything that call may write to.
///
/// A borrow rather than the engine itself, because three of the steps need the session's output, its
/// admission or the wall clock, and none of those belongs to the engine's own state.
struct Handling<'a> {
    engine: &'a mut Engine,
    output: &'a mut Output,
    admission: &'a mut Admission,
    /// Whether this segment's destination is the session's virtual resolver address, which is what a `SYN`
    /// needs to know to open the right kind of flow.
    resolver: bool,
    /// The wall clock this packet's idle floor is measured from.
    now: Instant,
    /// The stack's own clock, pinned for the whole call.
    at: SmolInstant,
}

impl ingress::Owner for Handling<'_> {
    type Handle = SocketHandle;

    fn held(&mut self, handle: SocketHandle) -> Option<Held<'_>> {
        ingress::Owner::held(self.engine, handle)
    }

    fn counters(&mut self) -> &mut Counters {
        ingress::Owner::counters(self.engine)
    }

    fn deliver(&mut self, report: DaemonErrorReport) {
        ingress::Owner::deliver(self.engine, report);
    }

    fn reclaim_closed(&mut self) {
        ingress::Owner::reclaim_closed(self.engine);
    }
}

impl ingress::Ingress for Handling<'_> {
    fn settle(&mut self) {
        // The pure primitive: the stack, the device and the output, and nothing of any flow's - see
        // [Engine::quiesce].
        self.engine.quiesce(self.at, self.output);
    }

    fn push(&mut self, packet: &[u8]) -> bool {
        self.engine.device.push(packet)
    }

    fn endpoints(&self) -> impl Iterator<Item = (SocketHandle, SocketAddr, SocketAddr)> + '_ {
        self.engine
            .flows
            .iter()
            .map(|(handle, held)| (*handle, held.record.client, held.record.destination))
    }

    fn open(&mut self, segment: &Segment) -> Option<SocketHandle> {
        self.engine.open(
            segment.source,
            segment.destination,
            segment.hop_limit,
            self.resolver,
            self.now,
            self.admission,
        )
    }

    fn now(&self) -> Instant {
        self.now
    }
}
