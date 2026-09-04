//! Shared client-ingress ordering and packet classification.
use std::net::SocketAddr;
use std::time::Instant;

use smoltcp::socket::tcp::{Socket, State};
use tokio_util::sync::CancellationToken;

use crate::shared::bridge::{Bridge, Sealed};
use crate::shared::flow;
use crate::shared::lifetime::rearmed;
use crate::shared::proto::daemon::DaemonErrorReport;
use crate::shared::protocol::daemon_error_report_with_details;
use crate::shared::tcp_wire::Segment;

/// What one owner counts about the packets it takes. One home per figure, so no caller can move the wrong
/// one and no two callers can disagree about which.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// Packets refused before stack delivery because the device was occupied or output capacity vanished.
    /// Any tentative state is unwound.
    pub unconsumed: u64,
    /// Resets the stack accepted from clients.
    pub reset: u64,
    /// Bytes taken out of clients' receive buffers.
    pub to_upstream: u64,
    /// A worker's half of a bridge had gone before its ending could be handed over.
    pub stale: u64,
    /// Reserved terminal tails that refused an ending. Counted apart from every ordinary ending, because it
    /// is a broken invariant rather than something a client did.
    pub tail_failed: u64,
}

/// One flow's mutable pieces, reachable together.
pub struct Held<'a> {
    pub socket: &'a mut Socket<'static>,
    pub bridge: &'a mut Bridge,
    pub cancel: &'a CancellationToken,
    pub deadline: &'a mut Option<Instant>,
    pub client: SocketAddr,
    pub destination: SocketAddr,
}

/// The flow table, for the decisions that need nothing of the stack's plumbing.
pub trait Owner {
    /// Names one flow's transport slot. A `smoltcp` `SocketHandle` in production.
    type Handle: Copy + Eq;

    /// One flow's socket, bridge, token and deadline. `None` once the flow has been reclaimed.
    fn held(&mut self, handle: Self::Handle) -> Option<Held<'_>>;

    fn counters(&mut self) -> &mut Counters;

    /// Delivers one structured non-fatal report. The only step whose body cannot run on a host, because the
    /// reporter it reaches is the daemon's own conversation - see the module note.
    fn deliver(&mut self, report: DaemonErrorReport);

    /// Asks every flow whose client-side socket the stack has finished with what that means for the worker
    /// still attached to it - [crate::shared::bridge::Bridge::teardown]'s decision, applied.
    fn reclaim_closed(&mut self);

    /// Notifies the owner that this flow's idle deadline or armed state changed.
    fn rearmed(&mut self, handle: Self::Handle);
}

/// What [accept] needs beyond the table: the stack, and the ability to open a flow.
pub trait Ingress: Owner {
    /// Runs the stack to quiescence **at the instant this call pinned**, emitting whatever it produces.
    fn settle(&mut self);

    /// Offers the packet to the stack. On `false`, [accept] unwinds tentative state without stack delivery.
    fn push(&mut self, packet: &[u8]) -> bool;

    /// Every live flow's slot and the endpoints it is keyed by. Which one a segment *names* is decided here,
    /// by [crate::shared::flow::named_by], and not by the owner.
    fn endpoints(&self) -> impl Iterator<Item = (Self::Handle, SocketAddr, SocketAddr)> + '_;

    /// Opens a flow. `None` means it could not be constructed. The one step that is genuinely the engine's,
    /// because none of what it builds exists here.
    fn open(&mut self, segment: &Segment) -> Option<Self::Handle>;

    /// The wall clock this packet's idle floor is measured from.
    fn now(&self) -> Instant;
}

/// One client segment, from the wire to this owner's tables.
pub fn accept<I: Ingress>(owner: &mut I, packet: &[u8], segment: &Segment) {
    // A `SYN` naming no live flow opens one. A duplicate for a flow that exists falls through to the stack,
    // which reuses the half-open state it already has rather than allocating a second.
    let opening = if segment.syn && !segment.rst && named(owner, segment).is_none() {
        match owner.open(segment) {
            Some(handle) => Some(handle),
            // Nothing was built, so there is nothing to unwind and nothing for the stack to receive - and
            // nothing has changed, so nothing needs scanning either.
            None => return,
        }
    } else {
        None
    };
    // Only for a segment claiming to be a reset, and only when one could actually be about a flow this owner
    // holds - see [candidate], which is where the settling poll that buys the attribution lives.
    let candidate = if segment.rst {
        candidate(owner, segment)
    } else {
        None
    };
    if !owner.push(packet) {
        // Counted rather than queued, because a queue here would hide the bug. Nothing of the reset has been
        // acted on - a packet the stack never saw resets nothing - but a flow opened for it exists and now
        // never will, so it goes back the ordinary abortive way rather than waiting out its floor.
        owner.counters().unconsumed += 1;
        if let Some(handle) = opening {
            fence(owner, handle);
        }
        reclaim(owner);
        return;
    }
    owner.settle();
    if let Some((handle, before)) = candidate {
        let after = held_state(owner, handle);
        if after.is_some_and(|after| accepted_reset(before, after)) {
            owner.counters().reset += 1;
            fence(owner, handle);
            // No poll follows, and deliberately: the stack has already cleared this socket's tuple, so there
            // is nothing for it to emit. This daemon does not answer an accepted reset with one of its own.
            reclaim(owner);
            return;
        }
    }
    // An opening the stack refused: a bad checksum, a malformed header, a `SYN` it would not take. The socket
    // is still only listening, so nothing will ever arrive on it, and the flow behind it holds a descriptor,
    // a grant and a worker. Unwound now rather than at the transitory floor - and only the flow *this* packet
    // created, so a duplicate `SYN` for a listener that was already there is left alone.
    if let Some(handle) = opening {
        if held_state(owner, handle) == Some(State::Listen) {
            fence(owner, handle);
            reclaim(owner);
            return;
        }
    }
    // Resolved after the poll rather than before it: this segment may have opened the flow, and the phase it
    // produced is the state the socket ends up in. A segment naming no flow - answered with a reset by the
    // stack, or dropped - rearms nothing.
    let Some(handle) = named(owner, segment) else {
        reclaim(owner);
        return;
    };
    let sealed = arm_and_seal(owner, handle);
    // Whether this owner has left the stack something to do. An ordinary segment has not, and polling again
    // for it would double the cost of the throughput path.
    let disturbed = match sealed {
        None | Some(Sealed::NotDue) => false,
        Some(Sealed::Whole { moved }) => {
            owner.counters().to_upstream += moved as u64;
            true
        }
        // Ordinary. A worker's task terminal and its half of the bridge disappear at nearly the same moment,
        // and this is the race between them; the terminal is what ends the flow.
        Some(Sealed::WorkerGone { moved }) => {
            let counters = owner.counters();
            counters.to_upstream += moved as u64;
            counters.stale += 1;
            true
        }
        Some(Sealed::Broken { moved, why }) => {
            owner.counters().to_upstream += moved as u64;
            tail_failed(owner, handle, why);
            true
        }
    };
    if disturbed {
        // The extraction emptied the receive buffer, so the window has reopened and this daemon's own FIN is
        // allowed out; a fence above has a reset to put on the wire while an endpoint still exists.
        owner.settle();
    }
    reclaim(owner);
}

/// The Closed-socket scan, at the one point in a packet's handling where it is safe.
fn reclaim<O: Owner>(owner: &mut O) {
    owner.reclaim_closed();
}

/// Arms this flow's idle floor and then extracts its ending, in that order and with nothing between.
fn arm_and_seal<I: Ingress>(owner: &mut I, handle: I::Handle) -> Option<Sealed> {
    let now = owner.now();
    let held = owner.held(handle)?;
    let state = held.socket.state();
    if !held.cancel.is_cancelled() {
        *held.deadline = rearmed(*held.deadline, state, held.bridge.ending(state), now);
    }
    let sealed = held.bridge.seal(held.socket);
    // Sealing is the only packet path that moves this flow's idle deadline.
    owner.rearmed(handle);
    Some(sealed)
}

/// Ends one flow abortively: its socket first, then its worker.
fn fence<O: Owner>(owner: &mut O, handle: O::Handle) {
    let Some(held) = owner.held(handle) else {
        return;
    };
    held.socket.abort();
    held.cancel.cancel();
    // A cancelled flow is no longer one its owner expires, which is a change to what it schedules on.
    owner.rearmed(handle);
}

/// One flow's turn on the **traffic pass**: the crossing, and what a refused terminal tail there means.
pub fn crossed<O: Owner>(
    owner: &mut O,
    handle: O::Handle,
    cx: &mut std::task::Context<'_>,
) -> Option<crate::shared::bridge::Crossing> {
    let held = owner.held(handle)?;
    let crossing = held.bridge.cross(held.socket, cx);
    if let Some(why) = crossing.broken {
        tail_failed(owner, handle, why);
    }
    Some(crossing)
}

/// The terminal tail refused an ending: count it, fence the flow, and say so.
pub fn tail_failed<O: Owner>(owner: &mut O, handle: O::Handle, why: &'static str) {
    owner.counters().tail_failed += 1;
    let Some(held) = owner.held(handle) else {
        return;
    };
    let report = tail_failure(why, held.client, held.destination);
    held.socket.abort();
    held.cancel.cancel();
    // Same as [fence]: cancelling stops this flow being one its owner expires.
    owner.rearmed(handle);
    owner.deliver(report);
}

/// Which flow this segment names, by both of its endpoints and in the client's own direction.
fn named<I: Ingress>(owner: &I, segment: &Segment) -> Option<I::Handle> {
    flow::named_by(owner.endpoints(), segment.source, segment.destination)
}

/// The phase one flow's socket is in, or `None` if the flow has gone.
fn held_state<O: Owner>(owner: &mut O, handle: O::Handle) -> Option<State> {
    Some(owner.held(handle)?.socket.state())
}

/// Which flow a reset *might* be about, and the phase its socket is in before the stack sees the segment.
fn candidate<I: Ingress>(owner: &mut I, segment: &Segment) -> Option<(I::Handle, State)> {
    let handle = named(owner, segment)?;
    if !reachable(held_state(owner, handle)?) {
        return None;
    }
    owner.settle();
    let before = held_state(owner, handle)?;
    reachable(before).then_some((handle, before))
}

/// Whether a socket in this phase could transition because of a reset at all.
fn reachable(state: State) -> bool {
    match state {
        State::Listen | State::Closed => false,
        State::SynSent
        | State::SynReceived
        | State::Established
        | State::FinWait1
        | State::FinWait2
        | State::CloseWait
        | State::Closing
        | State::LastAck
        | State::TimeWait => true,
    }
}

/// Whether the stack accepted the reset: the only two transitions one it accepted can produce.
pub fn accepted_reset(before: State, after: State) -> bool {
    match before {
        // A passive open the reset sent back to listening.
        State::SynReceived => after == State::Listen || after == State::Closed,
        _ => after == State::Closed,
    }
}

/// The report a refused terminal tail raises, with the flow it is about.
fn tail_failure(
    why: &'static str,
    client: SocketAddr,
    destination: SocketAddr,
) -> DaemonErrorReport {
    daemon_error_report_with_details(
        "shizuku.tcp_terminal_tail",
        "a client's half-close could not be moved out of the stack, so the flow was reset rather than \
         closed over a truncated stream",
        "InvalidData",
        [
            ("reason", why.to_owned()),
            ("client", client.to_string()),
            ("destination", destination.to_string()),
        ],
    )
}

#[cfg(test)]
mod tests {

    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
    use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
    use smoltcp::socket::tcp::{Socket, SocketBuffer};
    use smoltcp::time::Instant as SmolInstant;
    use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio_util::sync::CancellationToken;

    use crate::shared::bridge::{self, Bridge, TailCapacity, Worker};
    use crate::shared::lifetime::Ending;
    use crate::shared::tcp_wire::peek;

    use super::*;

    const MTU: usize = 1500;
    const CLIENT: Ipv4Address = Ipv4Address::new(192, 0, 2, 2);
    const SERVER: Ipv4Address = Ipv4Address::new(198, 51, 100, 7);
    const PORT: u16 = 443;
    const PIPE: usize = 64 * 1024;

    #[derive(Default)]
    struct Wire {
        inbound: Option<Vec<u8>>,
        outbound: VecDeque<Vec<u8>>,
    }

    impl Device for Wire {
        type RxToken<'a> = Rx;
        type TxToken<'a> = Tx<'a>;

        fn receive(&mut self, _now: SmolInstant) -> Option<(Rx, Tx<'_>)> {
            let packet = self.inbound.take()?;
            Some((
                Rx { packet },
                Tx {
                    outbound: &mut self.outbound,
                },
            ))
        }

        fn transmit(&mut self, _now: SmolInstant) -> Option<Tx<'_>> {
            Some(Tx {
                outbound: &mut self.outbound,
            })
        }

        fn capabilities(&self) -> DeviceCapabilities {
            let mut capabilities = DeviceCapabilities::default();
            capabilities.medium = Medium::Ip;
            capabilities.max_transmission_unit = MTU;
            capabilities
        }
    }

    struct Rx {
        packet: Vec<u8>,
    }

    impl RxToken for Rx {
        fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
            f(&self.packet)
        }
    }

    struct Tx<'a> {
        outbound: &'a mut VecDeque<Vec<u8>>,
    }

    impl TxToken for Tx<'_> {
        fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
            let mut packet = vec![0u8; len];
            let result = f(&mut packet);
            self.outbound.push_back(packet);
            result
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct AtScan {
        reset: u64,
        tail_failed: u64,
        state: Option<State>,
        cancelled: bool,
        halted: bool,
        deadline: Option<Instant>,
    }

    struct Flow {
        handle: SocketHandle,
        client: SocketAddr,
        destination: SocketAddr,
        bridge: Bridge,
        worker: Option<Worker>,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    }

    struct TestOwner {
        interface: Interface,
        wire: Wire,
        sockets: SocketSet<'static>,
        flows: Vec<Flow>,
        counters: Counters,
        reports: Vec<DaemonErrorReport>,
        settles: usize,
        openings: usize,
        scans: usize,
        at_scan: Vec<AtScan>,
        at: SmolInstant,
        now: Instant,
        tail: Option<usize>,
        main: usize,
        buffer: usize,
        /// Whether output from settling this packet can be handed off.
        settling: bool,
    }

    impl Owner for TestOwner {
        type Handle = SocketHandle;

        fn held(&mut self, handle: SocketHandle) -> Option<Held<'_>> {
            let TestOwner { sockets, flows, .. } = self;
            let flow = flows.iter_mut().find(|flow| flow.handle == handle)?;
            Some(Held {
                socket: sockets.get_mut::<Socket>(handle),
                bridge: &mut flow.bridge,
                cancel: &flow.cancel,
                deadline: &mut flow.deadline,
                client: flow.client,
                destination: flow.destination,
            })
        }

        fn counters(&mut self) -> &mut Counters {
            &mut self.counters
        }

        fn deliver(&mut self, report: DaemonErrorReport) {
            self.reports.push(report);
        }

        fn reclaim_closed(&mut self) {
            self.scans += 1;
            self.at_scan.push(AtScan {
                reset: self.counters.reset,
                tail_failed: self.counters.tail_failed,
                state: self
                    .flows
                    .first()
                    .map(|flow| self.sockets.get::<Socket>(flow.handle).state()),
                cancelled: self
                    .flows
                    .first()
                    .is_some_and(|flow| flow.cancel.is_cancelled()),
                halted: self.flows.first().is_some_and(|flow| flow.bridge.halted()),
                deadline: self.flows.first().and_then(|flow| flow.deadline),
            });
            let TestOwner { sockets, flows, .. } = self;
            for flow in flows.iter() {
                flow.bridge
                    .teardown(sockets.get::<Socket>(flow.handle).state(), &flow.cancel);
            }
        }

        /// Tests read deadlines directly rather than maintaining an index.
        fn rearmed(&mut self, _handle: SocketHandle) {}
    }

    impl Ingress for TestOwner {
        fn settle(&mut self) {
            self.settles += 1;
            for _ in 0..64 {
                if matches!(
                    self.interface
                        .poll(self.at, &mut self.wire, &mut self.sockets),
                    PollResult::None
                ) {
                    break;
                }
            }
        }

        fn push(&mut self, packet: &[u8]) -> bool {
            if !self.settling || self.wire.inbound.is_some() {
                return false;
            }
            self.wire.inbound = Some(packet.to_vec());
            true
        }

        fn endpoints(&self) -> impl Iterator<Item = (SocketHandle, SocketAddr, SocketAddr)> + '_ {
            self.flows
                .iter()
                .map(|flow| (flow.handle, flow.client, flow.destination))
        }

        fn now(&self) -> Instant {
            self.now
        }

        fn open(&mut self, segment: &Segment) -> Option<SocketHandle> {
            self.openings += 1;
            let mut socket = Socket::new(
                SocketBuffer::new(vec![0u8; self.buffer]),
                SocketBuffer::new(vec![0u8; self.buffer]),
            );
            socket
                .listen((IpAddress::Ipv4(SERVER), PORT))
                .expect("a fresh socket may listen");
            let tail = match self.tail {
                Some(bytes) => TailCapacity::undersized(bytes),
                None => TailCapacity::of(&socket),
            };
            let (owner, worker) = bridge::bridge(self.main, tail);
            let handle = self.sockets.add(socket);
            self.flows.push(Flow {
                handle,
                client: segment.source,
                destination: segment.destination,
                bridge: owner,
                worker: Some(worker),
                cancel: CancellationToken::new(),
                deadline: None,
            });
            Some(handle)
        }
    }

    struct Client {
        interface: Interface,
        wire: Wire,
        sockets: SocketSet<'static>,
        socket: SocketHandle,
    }

    struct Wired {
        owner: TestOwner,
        client: Client,
        millis: i64,
    }

    fn stack(address: Ipv4Address, prefix: u8) -> (Interface, Wire, SocketSet<'static>) {
        let mut wire = Wire::default();
        let mut interface = Interface::new(
            Config::new(HardwareAddress::Ip),
            &mut wire,
            SmolInstant::from_millis(0),
        );
        interface.set_any_ip(true);
        interface.update_ip_addrs(|addresses| {
            addresses
                .push(IpCidr::new(IpAddress::Ipv4(address), prefix))
                .expect("one address fits");
        });
        (interface, wire, SocketSet::new(Vec::new()))
    }

    impl Wired {
        fn new(buffer: usize, tail: Option<usize>) -> Self {
            Self::sized(buffer, tail, PIPE)
        }

        fn sized(buffer: usize, tail: Option<usize>, main: usize) -> Self {
            let (interface, wire, sockets) = stack(SERVER, 24);
            let owner = TestOwner {
                settling: true,
                interface,
                wire,
                sockets,
                flows: Vec::new(),
                counters: Counters::default(),
                reports: Vec::new(),
                settles: 0,
                openings: 0,
                scans: 0,
                at_scan: Vec::new(),
                at: SmolInstant::from_millis(0),
                now: Instant::now(),
                tail,
                main,
                buffer,
            };
            let (interface, wire, mut sockets) = stack(CLIENT, 24);
            let socket = sockets.add(Socket::new(
                SocketBuffer::new(vec![0u8; buffer]),
                SocketBuffer::new(vec![0u8; buffer]),
            ));
            Self {
                owner,
                client: Client {
                    interface,
                    wire,
                    sockets,
                    socket,
                },
                millis: 0,
            }
        }

        fn now(&self) -> SmolInstant {
            SmolInstant::from_millis(self.millis)
        }

        fn advance(&mut self, millis: i64) {
            self.millis += millis;
            self.owner.at = self.now();
        }

        fn client(&mut self) -> &mut Socket<'static> {
            self.client.sockets.get_mut::<Socket>(self.client.socket)
        }

        fn client_state(&self) -> State {
            self.client
                .sockets
                .get::<Socket>(self.client.socket)
                .state()
        }

        fn owner_state(&self) -> State {
            self.owner
                .flows
                .first()
                .map(|flow| self.owner.sockets.get::<Socket>(flow.handle).state())
                .expect("one flow")
        }

        fn connect(&mut self) {
            let Client {
                interface,
                sockets,
                socket,
                ..
            } = &mut self.client;
            sockets
                .get_mut::<Socket>(*socket)
                .connect(interface.context(), (IpAddress::Ipv4(SERVER), PORT), 49152)
                .expect("a fresh socket may connect");
        }

        fn client_polls(&mut self) {
            let now = self.now();
            let Client {
                interface,
                wire,
                sockets,
                ..
            } = &mut self.client;
            for _ in 0..64 {
                if matches!(interface.poll(now, wire, sockets), PollResult::None) {
                    break;
                }
            }
        }

        fn client_speaks(&mut self) -> usize {
            self.owner.at = self.now();
            self.client_polls();
            let mut moved = 0;
            while let Some(packet) = self.client.wire.outbound.pop_front() {
                let Ok(segment) = peek(&packet) else { continue };
                accept(&mut self.owner, &packet, &segment);
                moved += 1;
            }
            moved
        }

        fn owner_answers(&mut self) -> usize {
            let mut moved = 0;
            while let Some(packet) = self.owner.wire.outbound.pop_front() {
                self.client.wire.inbound = Some(packet);
                self.client_polls();
                moved += 1;
            }
            moved
        }

        fn traffic(&mut self) {
            self.owner.at = self.now();
            let handle = self.owner.flows[0].handle;
            let socket = self.owner.sockets.get_mut::<Socket>(handle);
            let mut cx = Context::from_waker(Waker::noop());
            self.owner.flows[0].bridge.cross(socket, &mut cx);
            Ingress::settle(&mut self.owner);
        }

        fn run(&mut self) {
            for _ in 0..64 {
                let moved = self.client_speaks() + self.owner_answers();
                self.traffic();
                if moved == 0 {
                    break;
                }
            }
        }

        fn established(&mut self) {
            self.connect();
            self.run();
            assert_eq!(self.client_state(), State::Established);
            assert_eq!(self.owner_state(), State::Established);
        }

        fn deadline(&self) -> Option<Instant> {
            self.owner.flows[0].deadline
        }

        fn cancelled(&self) -> bool {
            self.owner.flows[0].cancel.is_cancelled()
        }

        fn counters(&self) -> Counters {
            self.owner.counters
        }

        fn expire(&mut self) {
            if self.owner.flows[0]
                .deadline
                .is_some_and(|due| due <= Instant::now())
            {
                self.owner.flows[0].cancel.cancel();
            }
        }

        fn worker_reads(&mut self, into: &mut [u8]) -> Option<usize> {
            let worker = self.owner.flows[0]
                .worker
                .as_mut()
                .expect("the worker's half");
            let mut buffer = ReadBuf::new(into);
            match Pin::new(worker).poll_read(&mut Context::from_waker(Waker::noop()), &mut buffer) {
                Poll::Ready(Ok(())) => Some(buffer.filled().len()),
                Poll::Ready(Err(e)) => panic!("the worker's read failed: {e}"),
                Poll::Pending => None,
            }
        }

        fn worker_finishes_writing(&mut self) {
            let worker = self.owner.flows[0]
                .worker
                .as_mut()
                .expect("the worker's half");
            match Pin::new(worker).poll_shutdown(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(Ok(())) => {}
                other => panic!("an in-memory write half shuts down at once: {other:?}"),
            }
        }

        fn drain_worker(&mut self) -> Vec<u8> {
            let mut received = Vec::new();
            let mut scratch = vec![0u8; 256];
            for _ in 0..512 {
                match self.worker_reads(&mut scratch) {
                    Some(0) => return received,
                    Some(read) => received.extend_from_slice(&scratch[..read]),
                    None => panic!("a finished ending never leaves its reader waiting"),
                }
            }
            panic!("the worker's stream never ended");
        }
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|byte| byte as u8).collect()
    }

    fn abort_packet(wired: &mut Wired) -> Vec<u8> {
        wired.client().abort();
        wired.client_polls();
        while let Some(packet) = wired.client.wire.outbound.pop_front() {
            if peek(&packet).is_ok_and(|segment| segment.rst) {
                return packet;
            }
        }
        panic!("the client's stack never emitted its reset");
    }

    fn opening_packet(wired: &mut Wired) -> Vec<u8> {
        wired.connect();
        wired.client_polls();
        wired.client.wire.outbound.pop_front().expect("a SYN")
    }

    fn retarget(packet: &mut [u8], port: u16) {
        use smoltcp::wire::{Ipv4Packet, TcpPacket};
        let (source, destination) = {
            let ip = Ipv4Packet::new_checked(&*packet).expect("well formed");
            (ip.src_addr(), ip.dst_addr())
        };
        let header = Ipv4Packet::new_checked(&*packet)
            .expect("well formed")
            .header_len() as usize;
        let mut tcp = TcpPacket::new_checked(&mut packet[header..]).expect("well formed");
        tcp.set_src_port(port);
        tcp.fill_checksum(&source.into(), &destination.into());
    }

    fn shift_sequence(packet: &mut [u8]) {
        use smoltcp::wire::{Ipv4Packet, TcpPacket};
        let (source, destination) = {
            let ip = Ipv4Packet::new_checked(&*packet).expect("well formed");
            (ip.src_addr(), ip.dst_addr())
        };
        let header = Ipv4Packet::new_checked(&*packet)
            .expect("well formed")
            .header_len() as usize;
        let mut tcp = TcpPacket::new_checked(&mut packet[header..]).expect("well formed");
        let moved = tcp.seq_number() + (1 << 20);
        tcp.set_seq_number(moved);
        tcp.fill_checksum(&source.into(), &destination.into());
    }

    fn set_reset(packet: &mut [u8]) {
        use smoltcp::wire::{Ipv4Packet, TcpPacket};
        let (source, destination) = {
            let ip = Ipv4Packet::new_checked(&*packet).expect("well formed");
            (ip.src_addr(), ip.dst_addr())
        };
        let header = Ipv4Packet::new_checked(&*packet)
            .expect("well formed")
            .header_len() as usize;
        let mut tcp = TcpPacket::new_checked(&mut packet[header..]).expect("well formed");
        tcp.set_rst(true);
        tcp.fill_checksum(&source.into(), &destination.into());
    }

    fn half_closed_by_this_owner() -> Wired {
        let mut wired = Wired::new(4096, None);
        wired.established();
        wired.worker_finishes_writing();
        wired.run();
        assert!(
            matches!(wired.owner_state(), State::FinWait1 | State::FinWait2),
            "this owner has finished, the client has not: {:?}",
            wired.owner_state()
        );
        wired
    }

    fn into_time_wait(wired: &mut Wired, sent: &[u8]) {
        wired.client().send_slice(sent).expect("room in the client");
        wired.client().close();
        for _ in 0..16 {
            if wired.owner_state() == State::TimeWait {
                break;
            }
            wired.client_speaks();
            wired.owner_answers();
        }
        assert_eq!(
            wired.owner_state(),
            State::TimeWait,
            "the client's ending reached a phase with no floor of its own"
        );
    }

    #[test]
    fn a_client_ending_is_taken_on_the_ingress_that_carried_it() {
        let sent = pattern(200);
        let mut wired = Wired::new(1024, None);
        wired.established();
        wired
            .client()
            .send_slice(&sent)
            .expect("room in the client");
        wired.client().close();
        wired.client_speaks();

        let handle = wired.owner.flows[0].handle;
        assert_eq!(
            wired.owner.sockets.get::<Socket>(handle).recv_queue(),
            0,
            "the ending left the stack on the packet that ended it"
        );
        assert!(
            wired.owner.flows[0].bridge.halted(),
            "as a clean half-close"
        );
        assert!(!wired.cancelled());
        assert_eq!(wired.counters().to_upstream, sent.len() as u64);
        assert_eq!(wired.drain_worker(), sent);
    }

    #[test]
    fn a_fin_arms_its_own_floor_before_the_seal_changes_what_the_phase_means() {
        let sent = pattern(64);
        let mut wired = half_closed_by_this_owner();

        let almost = Instant::now() + Duration::from_millis(1);
        wired.owner.flows[0].deadline = Some(almost);
        into_time_wait(&mut wired, &sent);

        assert!(wired.owner.flows[0].bridge.halted());
        let due = wired
            .deadline()
            .expect("a flush with no deadline is unbounded");
        assert!(
            due > almost + Duration::from_secs(60),
            "the fresh FIN-time floor, not the one that was about to expire"
        );
        assert_eq!(
            wired.owner.flows[0].bridge.ending(State::TimeWait),
            Ending::Flushing,
            "and it is preserved from here, which is why it had to be fresh"
        );

        wired.expire();
        assert!(
            !wired.cancelled(),
            "a worker flushing acknowledged bytes is not expired the moment it starts"
        );
        assert_eq!(wired.drain_worker(), sent);
    }

    #[test]
    fn an_accepted_reset_is_not_rearmed() {
        let mut wired = Wired::new(1024, None);
        wired.established();
        wired.owner.flows[0].deadline = None;
        wired.client().abort();
        wired.client_speaks();

        assert_eq!(wired.owner_state(), State::Closed);
        assert_eq!(wired.counters().reset, 1);
        assert!(wired.cancelled());
        assert_eq!(wired.deadline(), None);
    }

    #[test]
    fn an_accepted_reset_fences_the_socket_before_its_worker_is_cancelled() {
        let mut wired = Wired::new(1024, None);
        let opening = opening_packet(&mut wired);
        let segment = peek(&opening).expect("parses");
        accept(&mut wired.owner, &opening, &segment);
        wired.owner_answers();
        assert_eq!(wired.owner_state(), State::SynReceived);

        let reset = abort_packet(&mut wired);
        let reset_segment = peek(&reset).expect("parses");
        accept(&mut wired.owner, &reset, &reset_segment);
        assert_eq!(
            wired.owner_state(),
            State::Closed,
            "the accepted reset left a listener, and it was fenced at once"
        );
        assert_eq!(wired.counters().reset, 1);
        assert!(wired.cancelled());

        let flows = wired.owner.flows.len();
        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(
            wired.owner_state(),
            State::Closed,
            "the fenced socket cannot accept a successor"
        );
        assert_eq!(
            wired.owner.flows.len(),
            flows,
            "and no second flow was hung off the doomed one"
        );

        let gone = wired.owner.flows.remove(0).handle;
        wired.owner.sockets.remove(gone);
        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(
            wired.owner.flows.len(),
            1,
            "a fresh flow, not a resurrected one"
        );
        assert_ne!(wired.owner_state(), State::Closed);
    }

    #[test]
    fn a_reset_the_stack_refuses_leaves_a_clean_flush_alone() {
        for (name, corrupt) in [
            (
                "a bad TCP checksum",
                (|packet: &mut Vec<u8>| packet[36] ^= 0xff) as fn(&mut Vec<u8>),
            ),
            ("a bad IPv4 header checksum", |packet: &mut Vec<u8>| {
                packet[10] ^= 0xff
            }),
            ("a sequence number outside the window", |packet| {
                shift_sequence(packet)
            }),
        ] {
            let sent = pattern(32);
            let mut wired = Wired::new(1024, None);
            wired.established();
            wired.client().send_slice(&sent).expect("room");
            wired.client().close();
            wired.client_speaks();
            assert!(
                wired.owner.flows[0].bridge.halted(),
                "{name}: a clean flush"
            );

            let mut reset = abort_packet(&mut wired);
            corrupt(&mut reset);
            assert!(peek(&reset).expect("the header still parses").rst, "{name}");
            let segment = peek(&reset).expect("parses");
            accept(&mut wired.owner, &reset, &segment);

            assert_ne!(wired.owner_state(), State::Closed, "{name}: refused");
            assert_eq!(wired.counters().reset, 0, "{name}");
            assert!(!wired.cancelled(), "{name}: the flush is untouched");
            assert!(wired.owner.flows[0].bridge.halted(), "{name}");
        }
    }

    #[test]
    fn a_due_close_timer_is_not_mistaken_for_a_reset_this_stack_accepted() {
        let mut wired = half_closed_by_this_owner();
        into_time_wait(&mut wired, &pattern(16));
        assert!(wired.owner.flows[0].bridge.halted());

        wired.advance(11_000);
        let mut reset = abort_packet(&mut wired);
        shift_sequence(&mut reset);
        let segment = peek(&reset).expect("parses");
        accept(&mut wired.owner, &reset, &segment);

        assert_eq!(wired.owner_state(), State::Closed);
        assert_eq!(
            wired.counters().reset,
            0,
            "but that is the timer's doing, not the segment's"
        );
        assert!(!wired.cancelled());
    }

    #[test]
    fn a_packet_the_device_refuses_is_counted_and_changes_nothing_else() {
        let sent = pattern(16);
        let mut wired = Wired::new(1024, None);
        wired.established();
        wired.client().send_slice(&sent).expect("room");
        wired.client().close();
        wired.client_polls();
        let packet = wired.client.wire.outbound.pop_front().expect("a segment");
        let segment = peek(&packet).expect("parses");
        assert!(!segment.rst);
        wired.owner.wire.inbound = Some(vec![0u8; 40]);

        accept(&mut wired.owner, &packet, &segment);
        assert_eq!(wired.counters().unconsumed, 1);
        assert!(
            !wired.owner.flows[0].bridge.halted(),
            "a segment the stack never saw ends nothing"
        );
        assert!(!wired.cancelled());
    }

    #[test]
    fn a_tail_that_cannot_take_the_ending_is_fenced_cancelled_counted_and_reported() {
        let mut wired = Wired::new(1024, Some(8));
        wired.established();
        wired.client().send_slice(&pattern(200)).expect("room");
        wired.client().close();
        wired.client_speaks();

        assert_eq!(wired.counters().tail_failed, 1);
        assert!(
            !wired.owner.flows[0].bridge.halted(),
            "a short extraction is never a clean half-close"
        );
        assert_eq!(wired.owner_state(), State::Closed);
        assert!(wired.cancelled());
        let report = wired.owner.reports.first().expect("a report was raised");
        assert_eq!(report.context, "shizuku.tcp_terminal_tail");
        assert_eq!(report.kind, "InvalidData");
        for key in ["reason", "client", "destination"] {
            assert!(
                report.details.iter().any(|detail| detail.key == key),
                "the report names the {key}"
            );
        }
        assert!(
            wired.drain_worker().len() < 200,
            "a refusal rather than a whole ending"
        );
    }

    #[test]
    fn an_ordinary_packet_polls_the_stack_once() {
        let mut wired = Wired::new(4096, None);
        wired.established();
        wired.owner.settles = 0;
        wired.client().send_slice(b"ordinary").expect("room");
        assert_eq!(wired.client_speaks(), 1);
        assert_eq!(
            wired.owner.settles, 1,
            "one poll: the one after the push, and no other"
        );

        wired.owner.settles = 0;
        wired.client().send_slice(b"last").expect("room");
        wired.client().close();
        let segments = wired.client_speaks();
        assert!(wired.owner.flows[0].bridge.halted());
        assert_eq!(
            wired.owner.settles,
            segments + 1,
            "one poll each, and one more for the extraction"
        );
    }

    #[test]
    fn a_raw_reset_candidate_settles_first_and_is_answered_with_nothing() {
        let mut wired = Wired::new(4096, None);
        wired.established();
        wired.owner.settles = 0;
        wired.owner.wire.outbound.clear();

        let reset = abort_packet(&mut wired);
        let segment = peek(&reset).expect("parses");
        accept(&mut wired.owner, &reset, &segment);
        assert_eq!(
            wired.owner.settles, 2,
            "the candidate pre-settle, and the poll after the push"
        );
        assert_eq!(wired.owner_state(), State::Closed);
        assert_eq!(wired.counters().reset, 1);
        assert!(
            wired.owner.wire.outbound.is_empty(),
            "and nothing went back to a client that has gone"
        );
    }

    #[test]
    fn a_reset_nothing_here_could_be_about_pays_only_the_ordinary_poll() {
        let mut wired = Wired::new(4096, None);
        wired.established();

        let reset = abort_packet(&mut wired);
        let mut stranger = reset.clone();
        retarget(&mut stranger, 40001);
        wired.owner.settles = 0;
        let strange = peek(&stranger).expect("parses");
        assert!(strange.rst);
        accept(&mut wired.owner, &stranger, &strange);
        assert_eq!(
            wired.owner.settles, 1,
            "one poll: no candidate, so nothing to attribute"
        );
        assert_eq!(wired.counters().reset, 0);
        assert!(!wired.cancelled());

        let segment = peek(&reset).expect("parses");
        wired.owner.settles = 0;
        accept(&mut wired.owner, &reset, &segment);
        assert_eq!(
            wired.owner.settles, 2,
            "a reachable candidate pays the pre-settle that attributes the transition"
        );
        assert_eq!(wired.owner_state(), State::Closed);
        wired.owner.settles = 0;
        accept(&mut wired.owner, &reset, &segment);
        assert_eq!(
            wired.owner.settles, 1,
            "a `Closed` socket cannot be reset again, so no attribution poll"
        );
        assert_eq!(
            wired.counters().reset,
            1,
            "and it is not counted a second time"
        );
    }

    #[test]
    fn a_reset_the_device_refuses_leaves_its_flow_exactly_as_it_was() {
        let mut wired = Wired::new(1024, None);
        wired.established();
        let reset = abort_packet(&mut wired);
        let segment = peek(&reset).expect("parses");
        assert!(segment.rst);
        // The reset's pre-settlement poll consumed the last output slot. Refusal must leave the flow and
        // device unchanged because the stack never saw the reset.
        wired.owner.settling = false;
        let openings = wired.owner.openings;

        accept(&mut wired.owner, &reset, &segment);

        assert_eq!(wired.counters().unconsumed, 1);
        assert_eq!(wired.counters().reset, 0, "a reset the stack never saw");
        assert_eq!(wired.owner_state(), State::Established);
        assert!(!wired.cancelled());
        assert_eq!(
            wired.owner.openings, openings,
            "and nothing was opened for it"
        );
        assert!(
            wired.owner.wire.inbound.is_none(),
            "the segment was refused rather than left where nothing would consume it"
        );
    }

    #[test]
    fn a_device_that_refuses_a_new_opening_unwinds_it_there_and_then() {
        let mut wired = Wired::new(4096, None);
        let opening = opening_packet(&mut wired);
        let segment = peek(&opening).expect("parses");
        assert!(segment.syn && !segment.rst);
        wired.owner.wire.inbound = Some(vec![0u8; 40]);

        accept(&mut wired.owner, &opening, &segment);

        assert_eq!(wired.owner.openings, 1);
        assert_eq!(wired.owner.flows.len(), 1);
        assert_eq!(wired.counters().unconsumed, 1);
        assert_eq!(
            wired.owner_state(),
            State::Closed,
            "the socket is fenced rather than left listening out its floor"
        );
        assert!(wired.cancelled());
        assert_eq!(wired.deadline(), None);
        assert!(
            !wired.owner.flows[0].bridge.halted(),
            "and no ending was extracted"
        );
        assert_eq!(wired.counters().to_upstream, 0);
        assert_eq!(wired.counters().tail_failed, 0);
    }

    #[test]
    fn an_opening_that_also_claims_a_reset_builds_nothing() {
        let mut wired = Wired::new(4096, None);
        let mut opening = opening_packet(&mut wired);
        set_reset(&mut opening);
        let segment = peek(&opening).expect("parses");
        assert!(segment.syn && segment.rst);

        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(wired.owner.openings, 0);
        assert!(wired.owner.flows.is_empty());
    }

    #[test]
    fn an_opening_the_stack_refuses_is_unwound_rather_than_left_to_its_floor() {
        for (name, corrupt) in [
            (
                "a bad IPv4 header checksum",
                (|packet: &mut Vec<u8>| packet[10] ^= 0xff) as fn(&mut Vec<u8>),
            ),
            ("a bad TCP checksum", |packet: &mut Vec<u8>| {
                packet[36] ^= 0xff
            }),
        ] {
            let mut wired = Wired::new(4096, None);
            let mut opening = opening_packet(&mut wired);
            corrupt(&mut opening);
            let segment = peek(&opening).expect("the header still parses");
            assert!(segment.syn && !segment.rst, "{name}");

            accept(&mut wired.owner, &opening, &segment);
            assert_eq!(wired.owner.openings, 1, "{name}: it looked like an opening");
            assert_eq!(
                wired.owner.flows.len(),
                1,
                "{name}: and one was built before the stack could say"
            );
            assert_eq!(
                wired.owner_state(),
                State::Closed,
                "{name}: the socket the stack never took is fenced"
            );
            assert!(
                wired.cancelled(),
                "{name}: and its worker is cancelled, so its terminal reclaims the rest"
            );
        }
    }

    #[test]
    fn a_duplicate_opening_for_a_live_listener_is_left_alone() {
        let mut wired = Wired::new(4096, None);
        let opening = opening_packet(&mut wired);
        let segment = peek(&opening).expect("parses");
        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(wired.owner.openings, 1);
        assert_eq!(wired.owner_state(), State::SynReceived);

        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(wired.owner.openings, 1);
        assert_eq!(wired.owner.flows.len(), 1);
        assert_ne!(wired.owner_state(), State::Closed);
        assert!(!wired.cancelled());
    }

    #[test]
    fn a_traffic_pass_answers_a_refused_tail_exactly_as_an_ingress_does() {
        let mut wired = Wired::sized(1024, Some(8), 8);
        wired.established();
        wired.client().send_slice(&pattern(200)).expect("room");
        wired.client().close();
        wired.client_polls();
        while let Some(packet) = wired.client.wire.outbound.pop_front() {
            wired.owner.wire.inbound = Some(packet);
            Ingress::settle(&mut wired.owner);
        }
        assert_eq!(wired.counters().tail_failed, 0);

        let handle = wired.owner.flows[0].handle;
        let mut cx = Context::from_waker(Waker::noop());
        let crossing = crossed(&mut wired.owner, handle, &mut cx).expect("the flow is live");

        assert_eq!(
            crossing.broken,
            Some("the reserved tail could not take the client's ending")
        );
        assert_eq!(
            wired.counters().tail_failed,
            1,
            "counted, once, by the pass"
        );
        assert_eq!(wired.owner_state(), State::Closed);
        assert!(wired.cancelled());
        let report = wired.owner.reports.first().expect("a report was raised");
        assert_eq!(report.context, "shizuku.tcp_terminal_tail");
        assert!(!wired.owner.flows[0].bridge.halted());
    }

    #[test]
    fn the_closed_socket_scan_runs_once_and_only_after_the_sequence_has_decided() {
        let mut wired = Wired::new(4096, None);
        wired.established();
        wired.owner.scans = 0;
        wired.owner.at_scan.clear();

        wired.client().send_slice(b"ordinary").expect("room");
        assert_eq!(wired.client_speaks(), 1);
        assert_eq!(wired.owner.scans, 1);
        assert!(!wired.cancelled());

        wired.owner.scans = 0;
        wired.owner.at_scan.clear();
        let reset = abort_packet(&mut wired);
        let segment = peek(&reset).expect("parses");
        accept(&mut wired.owner, &reset, &segment);

        assert_eq!(wired.owner.scans, 1);
        let entry = wired.owner.at_scan.first().copied().expect("the scan ran");
        assert_eq!(
            entry.reset, 1,
            "the reset was classified and counted before the scan was entered"
        );
        assert_eq!(
            entry.state,
            Some(State::Closed),
            "and the socket was already fenced"
        );
        assert!(
            entry.cancelled,
            "and its worker already cancelled - by the fence, in order, not by this scan"
        );
        assert_eq!(wired.counters().reset, 1);
    }

    #[test]
    fn a_clean_ending_is_armed_and_sealed_before_the_scan_could_cut_it_short() {
        let sent = pattern(200);
        let mut wired = Wired::new(1024, None);
        wired.established();
        wired.owner.at_scan.clear();

        let stale = Instant::now() + Duration::from_millis(1);
        wired.owner.flows[0].deadline = Some(stale);
        wired
            .client()
            .send_slice(&sent)
            .expect("room in the client");
        wired.client().close();
        wired.client_speaks();

        let entry = wired
            .owner
            .at_scan
            .last()
            .copied()
            .expect("the scan ran for the packet carrying the FIN");
        let due = entry
            .deadline
            .expect("a flush the scan could reach with no deadline is an unbounded flow");
        assert!(
            due > stale + Duration::from_secs(60),
            "the idle floor was refreshed to this FIN's own before the scan was entered, not left at the \
             one that was about to expire"
        );
        assert!(
            entry.halted,
            "and the ending was already extracted, so the scan has nothing to cut short"
        );
        assert!(!wired.cancelled());
        assert_eq!(wired.drain_worker(), sent);
    }

    #[test]
    fn a_reserved_tail_is_the_sockets_own_receive_capacity() {
        for buffer in [1024usize, 4096, 64 * 1024] {
            let socket = Socket::new(
                SocketBuffer::new(vec![0u8; buffer]),
                SocketBuffer::new(vec![0u8; buffer / 2]),
            );
            assert_eq!(
                TailCapacity::of(&socket).bytes(),
                socket.recv_capacity(),
                "the tail is the receive buffer, not the send buffer and not a figure beside them"
            );
            assert_eq!(TailCapacity::of(&socket).bytes(), buffer);
        }
    }
}
