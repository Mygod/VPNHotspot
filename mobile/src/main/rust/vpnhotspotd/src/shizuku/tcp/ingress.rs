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

    /// Android-owned reporting boundary used by production ingress.
    fn deliver(&mut self, report: DaemonErrorReport) {
        report::report(report);
    }

    fn reclaim_closed(&mut self) {
        Engine::reclaim_closed(self);
    }
}

/// One `accept` call's borrow of the engine and of everything that call may write to.
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
