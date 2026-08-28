//! The order one client segment is handled in, and every decision that order makes.
//!
//! Each step below is a decision about *when*, and each was wrong at some point in a way no other test could
//! see - because the bug is never inside a step, it is in the gap between two. So the sequence lives here,
//! in a platform-neutral module the host builds and tests, and the engine that supplies the tables is an
//! adapter with no decisions of its own to get wrong.
//!
//! What the owner provides is deliberately *primitive*: the stack's two plumbing calls, a way to reach one
//! flow's socket and bridge together, its counters, and a sink for a report. Ordering, classification, state
//! transitions, counter selection and report construction are all here. An owner that could reimplement any
//! of them would be a second copy of the thing under test.
//!
//! # One client's ending is taken on the ingress that carried it
//!
//! Push the packet, poll the stack, and *then*, before returning, extract the ending of the exact flow that
//! segment named. It has to be here. A later owner turn is a later `Interface::poll`, and every arm that can
//! interpose makes one: a configuration change is offered first and can await a whole retirement, a terminal
//! reclaims a flow, a deadline fires. A socket that reached `TIME-WAIT` ten seconds ago clears its entire
//! receive buffer inside any of them, and those are bytes this daemon has already acknowledged.
//!
//! Exactly one flow, and only its terminal extraction: no pass over the table, no steady-state crossing.
//!
//! # The idle floor is armed before the ending changes what the phase means
//!
//! The rearm runs *before* the seal, after the poll that produced the phase, with nothing awaited between.
//! A successful seal leaves the flow flushing, and a flushing flow in a terminal phase deliberately
//! *preserves* the deadline it already has - `TIME-WAIT` has no floor of its own and `Closed` has a zero one,
//! so re-deriving there would either unbound the flush or make it instantly due. Sealing first therefore
//! preserves the *previous* deadline, and a FIN arriving a moment before one expires would be cancelled
//! mid-flush. Rearming first makes the preserved deadline the fresh one this packet earned.
//!
//! # A reset is what the stack accepted, not what a bit said
//!
//! The `RST` bit is a *candidate* and nothing more: the checksum, the tuple, the sequence number and the
//! window are the stack's to judge, and it refuses a reset outright in `LISTEN`. So no reset cause is
//! carried. What is carried is which flow the segment names and what phase that socket is in, and the cause
//! is the transition observed across the poll that processed this exact packet. Both polls run at one pinned
//! instant, so a close timer that was already due cannot be read as a reset.
//!
//! An accepted reset is fenced *synchronously*, socket before worker. `SYN-RECEIVED` -> `LISTEN` leaves a
//! reusable listener, and cancelling a worker is asynchronous - a same-tuple `SYN` arriving before that
//! worker's terminal would attach to a flow reclamation is about to destroy.
//!
//! # The stack is polled once for an ordinary packet
//!
//! The poll after the push is the one every packet needs. A *second* one runs only when this owner has
//! changed something the stack has yet to act on - an extraction that emptied a receive buffer and so
//! reopened a window, or a fence that left a reset to emit. An ordinary segment on an established flow
//! changes neither and gets one poll, which matters because this is the throughput path. The candidate
//! pre-settle is separate and runs only for a segment claiming to be a reset, where it buys the attribution.

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
    /// The device still held a packet an earlier poll should have taken. This owner's bug, not a client's.
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
///
/// Together because every decision needs more than one of them at once: the idle floor is the phase *and*
/// the bridge's view of the ending, the extraction is the bridge *and* the socket, and fencing is the socket
/// *and* the token in that order. Handing them out one at a time is what let an owner sequence them itself.
pub struct Held<'a> {
    pub socket: &'a mut Socket<'static>,
    pub bridge: &'a mut Bridge,
    pub cancel: &'a CancellationToken,
    pub deadline: &'a mut Option<Instant>,
    pub client: SocketAddr,
    pub destination: SocketAddr,
}

/// The flow table, for the decisions that need nothing of the stack's plumbing.
///
/// Separate from [Ingress] because the traffic pass has no packet, no device and no output, and must still
/// settle a terminal-tail failure exactly the way an ingress does.
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
    ///
    /// A primitive, and *when* it runs is not this owner's to choose: it can cancel a worker, so running it
    /// while a packet is being handled would pre-empt the two orderings that packet is entitled to - fence
    /// before cancel for an accepted reset, arm before seal for a FIN. [accept] runs it once, last.
    fn reclaim_closed(&mut self);
}

/// What [accept] needs beyond the table: the stack, and the ability to open a flow.
pub trait Ingress: Owner {
    /// Runs the stack to quiescence **at the instant this call pinned**, emitting whatever it produces.
    ///
    /// The same instant every time, which is what makes a transition observed across a packet attributable
    /// to that packet: `smoltcp` compares every deadline against the timestamp it is handed, so two polls at
    /// one instant cannot disagree about which timer is due.
    fn settle(&mut self);

    /// Offers the packet to the stack. `false` means the device still held an untaken one.
    fn push(&mut self, packet: &[u8]) -> bool;

    /// Every live flow's slot and the endpoints it is keyed by. Which one a segment *names* is decided here,
    /// by [crate::shared::flow::named_by], and not by the owner.
    fn endpoints(&self) -> impl Iterator<Item = (Self::Handle, SocketAddr, SocketAddr)> + '_;

    /// Opens a flow: a socket, a charged grant, a bridge and a worker. `None` means it could not be
    /// admitted. The one step that is genuinely the engine's, because none of what it builds exists here.
    fn open(&mut self, segment: &Segment) -> Option<Self::Handle>;

    /// The wall clock this packet's idle floor is measured from.
    fn now(&self) -> Instant;
}

/// One client segment, from the wire to this owner's tables.
///
/// Nothing awaits, so nothing can interpose. See the module note for why each step is where it is.
pub fn accept<I: Ingress>(owner: &mut I, packet: &[u8], segment: &Segment) {
    // A `SYN` naming no live flow opens one. A duplicate for a flow that exists falls through to the stack,
    // which reuses the half-open state it already has rather than allocating a second.
    //
    // Never for `SYN|RST`. The stack refuses a reset in `LISTEN` and would refuse the opening too, so what
    // that combination buys is a socket, a charged grant, a bridge and a spawned worker per packet - held
    // until a four-minute floor - for a segment that was never going to open anything.
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
///
/// Last, and never inside [Ingress::settle]. The scan cancels workers, and every decision above is entitled
/// to run first: an accepted reset fences its socket *before* its worker is told to stop, and a client's FIN
/// arms its idle floor before its ending is extracted. A settle that scanned on its way past did both of
/// those first and in the wrong order - which is what it did, until the poll and the scan were separated.
///
/// Once for every packet the stack saw, on every path out of [accept] that reached the push - including the
/// ones that refuse the packet or abort the flow. The one exit that does not reach it is a `SYN` whose
/// admission was refused: nothing was built and nothing was pushed, so there is nothing to scan.
fn reclaim<O: Owner>(owner: &mut O) {
    owner.reclaim_closed();
}

/// Arms this flow's idle floor and then extracts its ending, in that order and with nothing between.
///
/// `None` when the flow is gone. A cancelled flow is not rearmed: it is already retiring and waiting only on
/// its worker, so a refreshed deadline would outlive the record it belongs to.
fn arm_and_seal<I: Ingress>(owner: &mut I, handle: I::Handle) -> Option<Sealed> {
    let now = owner.now();
    let held = owner.held(handle)?;
    let state = held.socket.state();
    if !held.cancel.is_cancelled() {
        *held.deadline = rearmed(*held.deadline, state, held.bridge.ending(state), now);
    }
    Some(held.bridge.seal(held.socket))
}

/// Ends one flow abortively: its socket first, then its worker.
///
/// The order is the whole of it, and it is here rather than behind a callback so that exactly one
/// implementation of it exists. Its own terminal is what removes the flow, which is the path every abortive
/// ending takes. Used for an accepted reset, and for an opening the stack then refused - which is not a
/// failure of anything, but must not be a socket, a grant, a descriptor and a worker held until a
/// four-minute floor for a connection that never existed.
fn fence<O: Owner>(owner: &mut O, handle: O::Handle) {
    let Some(held) = owner.held(handle) else {
        return;
    };
    held.socket.abort();
    held.cancel.cancel();
}

/// One flow's turn on the **traffic pass**: the crossing, and what a refused terminal tail there means.
///
/// The pass has no packet and no device, so it is not [accept] - but a crossing can find the same broken
/// invariant an ingress can, and it must answer it identically. That answer is here rather than at the call
/// site so that exactly one of it exists; the caller is left with the counters that are its own.
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
///
/// One implementation for both paths that can find it - the ingress above, and the traffic pass - because a
/// flow does not care which noticed, and two sequences would be two chances to get one wrong.
pub fn tail_failed<O: Owner>(owner: &mut O, handle: O::Handle, why: &'static str) {
    owner.counters().tail_failed += 1;
    let Some(held) = owner.held(handle) else {
        return;
    };
    let report = tail_failure(why, held.client, held.destination);
    held.socket.abort();
    held.cancel.cancel();
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
///
/// `None` when nothing this owner holds could transition because of one - and answering that costs nothing,
/// which is the point. The settling poll below is the price of the attribution and is paid only by a segment
/// that has something to attribute: an *unknown* reset, or an *unknown* `SYN|RST`, names no live flow at all
/// and takes the ordinary post-push poll and no other. Traffic nobody asked for is exactly the traffic that
/// must not cost double. A `SYN|RST` whose tuple does match a reachable flow is a different segment and pays
/// the pre-settle like any other candidate; what it does not do is *open* one - see [accept].
///
/// The cheap look happens twice, before and after that poll, and both are needed. Before, because a phase
/// the poll cannot move is a phase worth skipping the poll for: nothing pending can take a socket out of
/// `Listen` - only a `SYN` this owner has not pushed yet does that - and `Closed` is terminal. After, because
/// the poll is what settles a timer that was already due, and a `TIME-WAIT` socket it closes must not then be
/// read as a socket *this segment* reset.
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
///
/// `Listen` ignores one outright and `Closed` has nothing left to reset, so neither can - and treating an
/// already-`Closed` flow as freshly reset would cancel a clean flush that a due close timer, not this
/// segment, had just ended. Exhaustive rather than a wildcard: a phase `smoltcp` adds has to be classified
/// here.
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
///
/// Observational on purpose - nothing here re-implements sequence, window or checksum validation, and could
/// not do it as well as the stack that already did. What makes the observation exact is that `smoltcp` never
/// applies a reset segment's *acknowledgement* (`smoltcp-0.13.1 src/socket/tcp.rs:1774`), so a segment
/// carrying `RST` can move a socket only through the reset arms themselves (`:1820-1841`); the caller has
/// already settled every timer due at this instant; and nothing else runs in between.
pub fn accepted_reset(before: State, after: State) -> bool {
    match before {
        // A passive open the reset sent back to listening.
        State::SynReceived => after == State::Listen || after == State::Closed,
        _ => after == State::Closed,
    }
}

/// The report a refused terminal tail raises, with the flow it is about.
///
/// Built here rather than at the delivery site so that removing the emission is a change *this* module's
/// tests can see, and so that both paths that can raise one raise the same one. A client told its request
/// was received in full when part of it was dropped is the one outcome this daemon must never produce
/// quietly.
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
    //! Two real `smoltcp` stacks with one real [crate::shared::bridge::Bridge] between them, and the packets
    //! carried from one to the other by hand.
    //!
    //! Nothing about the *sequence* under test is stood in for. The owner below supplies primitives and
    //! nothing else - a poll, a device slot, an iterator over its table, one flow's pieces, its counters, a
    //! sink for a report - so every ordering, classification, counter selection and report this exercises is
    //! [accept]'s own production code. The client is a second terminating stack, so every handshake, window
    //! and phase is the protocol's own; the extraction is [crate::shared::bridge::Bridge::seal]; the idle
    //! floor is [crate::shared::lifetime]'s; the worker's half of the bridge is held here, so what it can
    //! read is exactly what a real worker would read.
    //!
    //! What is a stand-in is the *flow table* - a `Vec` rather than the engine's registry - and admission,
    //! neither of which is what these tests are about and both of which are covered where they live.
    //!
    //! What this deliberately is **not** is the `tun_reader` select loop. It drives the exact synchronous
    //! owner boundary, which is the boundary every ordering defect below lived on.

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
    /// Deep enough that the main pipe is never what refuses an ending, so what the tail does is visible.
    const PIPE: usize = 64 * 1024;

    /// One packet in each direction, exactly as the TUN shim holds them.
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

    /// What one flow looked like at the moment the Closed-socket scan was entered.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct AtScan {
        reset: u64,
        tail_failed: u64,
        state: Option<State>,
        cancelled: bool,
        halted: bool,
        /// The deadline itself, not merely whether one exists: `Wired::established` has already armed one, so
        /// "some deadline is set" is true before the FIN arrives and proves nothing about the rearm.
        deadline: Option<Instant>,
    }

    /// One flow, with everything an owner really holds for it.
    struct Flow {
        handle: SocketHandle,
        client: SocketAddr,
        destination: SocketAddr,
        bridge: Bridge,
        worker: Option<Worker>,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    }

    /// The packet owner, over a real stack. Primitives only - see the module note.
    struct TestOwner {
        interface: Interface,
        wire: Wire,
        sockets: SocketSet<'static>,
        flows: Vec<Flow>,
        counters: Counters,
        reports: Vec<DaemonErrorReport>,
        /// How many times the stack has been polled, which is the whole of the throughput claim.
        settles: usize,
        /// Openings this owner was asked for, so a test can prove one was refused before anything was built.
        openings: usize,
        /// How many times the Closed-socket scan has run.
        scans: usize,
        /// What was already true each time it was entered. The scan can cancel a worker, so *who* cancelled
        /// one is the whole question, and only a record taken before the scan acts can answer it.
        at_scan: Vec<AtScan>,
        at: SmolInstant,
        now: Instant,
        /// A tail smaller than the receive buffer, for the one case production cannot construct.
        tail: Option<usize>,
        /// Each steady-state direction. Small when a test needs the *tail* to be what carries an ending
        /// rather than the main pipe absorbing it first.
        main: usize,
        buffer: usize,
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
            // What was already true when the scan was *entered*, before it applies a single
            // `Bridge::teardown`. Counting scans and reading the state afterwards cannot tell a scan that
            // ran last from one that ran first and did the cancelling itself - both leave one scan and the
            // same end state. This is the only record that can.
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
            if self.wire.inbound.is_some() {
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

    /// The client's own stack, on the far side of the wire.
    struct Client {
        interface: Interface,
        wire: Wire,
        sockets: SocketSet<'static>,
        socket: SocketHandle,
    }

    /// Both halves, and the packets between them.
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

        /// Moves both clocks on, for a test that has to reach one of `smoltcp`'s fixed timers.
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

        /// Runs the client's own stack, so whatever it wants to send is on its wire.
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

        /// Every segment the client's stack wants to send, handed to [accept] exactly as ingress would.
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

        /// Every segment the owner's stack produced, back to the client's.
        fn owner_answers(&mut self) -> usize {
            let mut moved = 0;
            while let Some(packet) = self.owner.wire.outbound.pop_front() {
                self.client.wire.inbound = Some(packet);
                self.client_polls();
                moved += 1;
            }
            moved
        }

        /// The traffic pass: one crossing for the flow, then the stack. What [accept] is *not*.
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

        /// The owner's expiry, exactly as the engine makes it: a deadline in the past is abortive.
        fn expire(&mut self) {
            if self.owner.flows[0]
                .deadline
                .is_some_and(|due| due <= Instant::now())
            {
                self.owner.flows[0].cancel.cancel();
            }
        }

        /// What the worker's half can read right now. `Some(0)` is the end of the stream.
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

        /// The upstream reaching the end of its stream, which is what a worker's copy does to the bridge.
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

        /// Everything the worker can read, to the end of its stream.
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

    /// The one segment the client's stack emits when it aborts: a real reset, with a real sequence number and
    /// a real checksum, taken off the wire rather than assembled here.
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

    /// The client's opening `SYN`, captured before the owner ever sees it.
    fn opening_packet(wired: &mut Wired) -> Vec<u8> {
        wired.connect();
        wired.client_polls();
        wired.client.wire.outbound.pop_front().expect("a SYN")
    }

    /// Points a segment at a different client port, so it names a flow this owner does not hold.
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

    /// Moves a segment's sequence number far outside any window, and re-sums it so the *only* thing wrong
    /// with it is where it claims to be.
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

    /// Sets the reset bit on a segment and makes its checksum good again, so the only thing unusual about it
    /// is the flag.
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

    /// A flow whose own side has finished, so the next thing the client sends takes its socket into a
    /// *terminal* phase - which is where the idle floor stops being re-derived and starts being preserved.
    fn half_closed_by_this_owner() -> Wired {
        let mut wired = Wired::new(4096, None);
        wired.established();
        // The upstream reaches the end of its stream, so the crossing sends this owner's FIN.
        wired.worker_finishes_writing();
        wired.run();
        assert!(
            matches!(wired.owner_state(), State::FinWait1 | State::FinWait2),
            "this owner has finished, the client has not: {:?}",
            wired.owner_state()
        );
        wired
    }

    /// Drives the packet path until the owner's socket reaches `TIME-WAIT`, with no traffic pass at all.
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
        // The property every ordering below rests on: when `accept` returns, the ending is out of the stack.
        // Whatever polls it next - a configuration change awaiting a retirement, a terminal, a deadline -
        // finds nothing left to discard.
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
        assert!(!wired.cancelled(), "and the worker is left to flush it");
        assert_eq!(wired.counters().to_upstream, sent.len() as u64);
        // Main bytes, then tail bytes, then exactly one end of stream.
        assert_eq!(wired.drain_worker(), sent);
    }

    #[test]
    fn a_fin_arms_its_own_floor_before_the_seal_changes_what_the_phase_means() {
        // The order of the two steps, and why it is not interchangeable.
        //
        // A successful seal leaves the flow *flushing*, and a flushing flow in a terminal phase deliberately
        // preserves the deadline it already has: `TIME-WAIT` has no floor of its own and `Closed` has a zero
        // one, so re-deriving there would either unbound the flush or make it instantly due. Seal first and
        // the deadline preserved is the *previous* one - so a FIN arriving a moment before it expires is
        // cancelled while its worker is still flushing bytes this daemon acknowledged. Rearm first and the
        // preserved deadline is the fresh one this packet earned.
        let sent = pattern(64);
        let mut wired = half_closed_by_this_owner();

        // A prior deadline about to expire, as a long-idle flow really has.
        let almost = Instant::now() + Duration::from_millis(1);
        wired.owner.flows[0].deadline = Some(almost);
        into_time_wait(&mut wired, &sent);

        // Sealed, and bounded by the floor this FIN earned rather than by the one it arrived just before.
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

        // So the very next expiry does not cut the flush short.
        wired.expire();
        assert!(
            !wired.cancelled(),
            "a worker flushing acknowledged bytes is not expired the moment it starts"
        );
        assert_eq!(wired.drain_worker(), sent);
    }

    #[test]
    fn an_accepted_reset_is_not_rearmed() {
        // A flow whose client is gone has no idle floor worth arming, and arming one would keep a cancelled
        // worker's record alive past the moment its terminal is due.
        let mut wired = Wired::new(1024, None);
        wired.established();
        wired.owner.flows[0].deadline = None;
        wired.client().abort();
        wired.client_speaks();

        assert_eq!(wired.owner_state(), State::Closed);
        assert_eq!(wired.counters().reset, 1);
        assert!(wired.cancelled());
        assert_eq!(wired.deadline(), None, "nothing armed a floor for it");
    }

    #[test]
    fn an_accepted_reset_fences_the_socket_before_its_worker_is_cancelled() {
        // `SYN-RECEIVED` -> `LISTEN` is a reset the stack accepted that leaves a *reusable* listener, and
        // cancellation is asynchronous - so the successor SYN below would attach to a predecessor that
        // reclamation is about to destroy.
        let mut wired = Wired::new(1024, None);
        let opening = opening_packet(&mut wired);
        let segment = peek(&opening).expect("parses");
        accept(&mut wired.owner, &opening, &segment);
        // This owner's SYN|ACK back, but not the client's final ACK.
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

        // The race the fence exists for, with nothing having yielded in between: the same client opens again
        // before the cancelled worker's terminal has been joined.
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

        // Once the predecessor's terminal has been reclaimed, the client's retransmitted SYN opens cleanly.
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
        // Three ways a segment can carry the bit and not be a reset, and the stack is what says so in each.
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
        // "The socket is `Closed` after the packet" is evidence about the packet only if nothing else could
        // have closed it in the same poll - and a `TIME-WAIT` socket whose ten-second timer came due is
        // exactly something else. The settling poll before the candidate is what tells them apart.
        let mut wired = half_closed_by_this_owner();
        into_time_wait(&mut wired, &pattern(16));
        assert!(wired.owner.flows[0].bridge.halted());

        // Due, so the very next poll closes the socket whatever arrives.
        wired.advance(11_000);
        let mut reset = abort_packet(&mut wired);
        shift_sequence(&mut reset);
        let segment = peek(&reset).expect("parses");
        accept(&mut wired.owner, &reset, &segment);

        assert_eq!(wired.owner_state(), State::Closed, "the timer closed it");
        assert_eq!(
            wired.counters().reset,
            0,
            "but that is the timer's doing, not the segment's"
        );
        assert!(!wired.cancelled(), "so the clean flush is left to finish");
    }

    #[test]
    fn a_packet_the_device_refuses_is_counted_and_changes_nothing_else() {
        // A push fails only when the device still holds a packet an earlier poll should have taken, which is
        // a bug in this owner rather than something a client did - so it is counted and the segment is
        // dropped. Nothing is undone, because nothing was done.
        let sent = pattern(16);
        let mut wired = Wired::new(1024, None);
        wired.established();
        wired.client().send_slice(&sent).expect("room");
        wired.client().close();
        wired.client_polls();
        let packet = wired.client.wire.outbound.pop_front().expect("a segment");
        let segment = peek(&packet).expect("parses");
        assert!(!segment.rst);
        // Jammed, which is the one thing that makes a push fail.
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
        // The impossible case. Production asks the socket for the capacity, so it cannot construct one; what
        // has to be held down is the owner's answer if it ever could.
        let mut wired = Wired::new(1024, Some(8));
        wired.established();
        wired.client().send_slice(&pattern(200)).expect("room");
        wired.client().close();
        wired.client_speaks();

        assert_eq!(wired.counters().tail_failed, 1, "counted on its own");
        assert!(
            !wired.owner.flows[0].bridge.halted(),
            "a short extraction is never a clean half-close"
        );
        assert_eq!(wired.owner_state(), State::Closed, "the socket is fenced");
        assert!(wired.cancelled(), "and its worker cancelled");
        // The structured non-fatal, which is the only thing that makes this visible outside the process.
        let report = wired.owner.reports.first().expect("a report was raised");
        assert_eq!(report.context, "shizuku.tcp_terminal_tail");
        assert_eq!(report.kind, "InvalidData");
        for key in ["reason", "client", "destination"] {
            assert!(
                report.details.iter().any(|detail| detail.key == key),
                "the report names the {key}"
            );
        }
        // And both pipes ended, so the worker is not parked on one of them for ever.
        assert!(
            wired.drain_worker().len() < 200,
            "a refusal rather than a whole ending"
        );
    }

    #[test]
    fn an_ordinary_packet_polls_the_stack_once() {
        // The throughput claim, counted rather than asserted. A second same-instant `Interface::poll` on
        // every segment of every established flow is pure cost: this owner has changed nothing the stack has
        // yet to act on, and the poll after the push already ran.
        let mut wired = Wired::new(4096, None);
        wired.established();
        wired.owner.settles = 0;
        wired.client().send_slice(b"ordinary").expect("room");
        assert_eq!(wired.client_speaks(), 1, "one segment");
        assert_eq!(
            wired.owner.settles, 1,
            "one poll: the one after the push, and no other"
        );

        // A terminal extraction *has* changed something - it emptied a receive buffer and reopened a window -
        // so it earns the second one.
        wired.owner.settles = 0;
        wired.client().send_slice(b"last").expect("room");
        wired.client().close();
        let segments = wired.client_speaks();
        assert!(wired.owner.flows[0].bridge.halted(), "the ending was taken");
        assert_eq!(
            wired.owner.settles,
            segments + 1,
            "one poll each, and one more for the extraction"
        );
    }

    #[test]
    fn a_raw_reset_candidate_settles_first_and_is_answered_with_nothing() {
        // The pre-settle is the price of attributing a transition to this packet, so it runs only for a
        // segment claiming to be a reset. And an accepted reset ends with no poll at all: the stack has
        // already cleared the socket's tuple, so this daemon answers a client's reset with silence rather
        // than a reset of its own.
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
        // The pre-settle buys one thing - attributing a transition to this packet rather than to a timer that
        // was already due - so a segment with nothing to attribute must not pay for it. Unknown resets and
        // `SYN|RST` are exactly the traffic an attacker sends by the thousand, and doubling the stack poll
        // for them is the wrong way round.
        let mut wired = Wired::new(4096, None);
        wired.established();

        // One real reset off the wire, and a copy of it pointed at a flow this owner does not hold.
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
        assert!(!wired.cancelled(), "and the live flow is untouched");

        // The real one, which does name a reachable flow and so does buy the attribution poll.
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
    fn a_device_that_refuses_a_new_opening_unwinds_it_there_and_then() {
        // The other half of the unwind, and the one an established flow's test cannot reach: the segment that
        // *created* a flow is the segment the device then refused. Nothing will ever arrive on that socket -
        // the stack never saw the `SYN` - so it holds a descriptor, a grant and a worker for a connection
        // that does not exist.
        let mut wired = Wired::new(4096, None);
        let opening = opening_packet(&mut wired);
        let segment = peek(&opening).expect("parses");
        assert!(segment.syn && !segment.rst);
        // Jammed, which is the one thing that makes a push fail.
        wired.owner.wire.inbound = Some(vec![0u8; 40]);

        accept(&mut wired.owner, &opening, &segment);

        assert_eq!(wired.owner.openings, 1, "exactly one opening was built");
        assert_eq!(wired.owner.flows.len(), 1);
        assert_eq!(wired.counters().unconsumed, 1, "and the refusal is counted");
        assert_eq!(
            wired.owner_state(),
            State::Closed,
            "the socket is fenced rather than left listening out its floor"
        );
        assert!(wired.cancelled(), "and its worker cancelled after it");
        // Nothing of the flow lifecycle ran for a packet the stack never saw.
        assert_eq!(wired.deadline(), None, "no idle floor was armed");
        assert!(
            !wired.owner.flows[0].bridge.halted(),
            "and no ending was extracted"
        );
        assert_eq!(wired.counters().to_upstream, 0);
        assert_eq!(wired.counters().tail_failed, 0);
    }

    #[test]
    fn an_opening_that_also_claims_a_reset_builds_nothing() {
        // `SYN|RST` is not an opening. The stack refuses a reset in `LISTEN` and would refuse the opening
        // too, so what it would buy is a socket, a charged grant, a bridge and a spawned worker per packet -
        // held until the transitory floor - for a segment that was never going to connect.
        let mut wired = Wired::new(4096, None);
        let mut opening = opening_packet(&mut wired);
        set_reset(&mut opening);
        let segment = peek(&opening).expect("parses");
        assert!(segment.syn && segment.rst);

        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(wired.owner.openings, 0, "nothing was even asked for");
        assert!(wired.owner.flows.is_empty(), "so nothing was built");
    }

    #[test]
    fn an_opening_the_stack_refuses_is_unwound_rather_than_left_to_its_floor() {
        // A `SYN` this owner cannot validate and the stack then throws away. The flow behind it holds a
        // socket, a grant, a descriptor and a worker; leaving it for the four-minute transitory floor is a
        // per-packet cost an attacker chooses.
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
        // The unwind is only ever about the flow *this* packet created. A retransmitted `SYN` for a flow that
        // already exists opens nothing and must disturb nothing.
        let mut wired = Wired::new(4096, None);
        let opening = opening_packet(&mut wired);
        let segment = peek(&opening).expect("parses");
        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(wired.owner.openings, 1);
        assert_eq!(wired.owner_state(), State::SynReceived);

        accept(&mut wired.owner, &opening, &segment);
        assert_eq!(wired.owner.openings, 1, "the duplicate opened nothing");
        assert_eq!(wired.owner.flows.len(), 1);
        assert_ne!(wired.owner_state(), State::Closed, "and fenced nothing");
        assert!(!wired.cancelled());
    }

    #[test]
    fn a_traffic_pass_answers_a_refused_tail_exactly_as_an_ingress_does() {
        // The other path that can find the invariant broken. It has no packet and no device, so it is not
        // `accept` - and it must still fence, cancel, count and report identically, because a flow does not
        // care which path noticed. Two sequences would be two chances to get one wrong; there is one.
        // A main pipe too small to absorb the payload, so what is left when the crossing reaches its
        // extraction step is the ending itself - which is what the tail must take and cannot.
        let mut wired = Wired::sized(1024, Some(8), 8);
        wired.established();
        // Payload and FIN into the stack with no ingress seal, so the crossing is what finds the ending.
        wired.client().send_slice(&pattern(200)).expect("room");
        wired.client().close();
        wired.client_polls();
        while let Some(packet) = wired.client.wire.outbound.pop_front() {
            wired.owner.wire.inbound = Some(packet);
            Ingress::settle(&mut wired.owner);
        }
        assert_eq!(wired.counters().tail_failed, 0, "no ingress ran");

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
        assert_eq!(wired.owner_state(), State::Closed, "and the socket fenced");
        assert!(wired.cancelled(), "and its worker cancelled");
        let report = wired.owner.reports.first().expect("a report was raised");
        assert_eq!(report.context, "shizuku.tcp_terminal_tail");
        assert!(!wired.owner.flows[0].bridge.halted(), "never a clean close");
    }

    #[test]
    fn the_closed_socket_scan_runs_once_and_only_after_the_sequence_has_decided() {
        // The scan cancels workers, so *when* it runs is a decision and not plumbing - and counting it is not
        // enough to prove that decision. A scan moved to just after the post-push settle still runs exactly
        // once and still leaves the same end state; what it changes is who did the cancelling. So every
        // assertion below is about what was already true when the scan was *entered*.
        let mut wired = Wired::new(4096, None);
        wired.established();
        wired.owner.scans = 0;
        wired.owner.at_scan.clear();

        // An ordinary segment: one scan, at the end, with nothing for it to do.
        wired.client().send_slice(b"ordinary").expect("room");
        assert_eq!(wired.client_speaks(), 1);
        assert_eq!(wired.owner.scans, 1, "once per packet");
        assert!(!wired.cancelled());

        // An accepted reset. By the time the scan is entered the sequence has already classified it, counted
        // it, fenced the socket and cancelled the worker - in that order. A scan that ran earlier would find
        // a `Closed` socket whose bridge is not halted and would cancel the worker *itself*, which is the
        // fence-before-cancel invariant lost.
        wired.owner.scans = 0;
        wired.owner.at_scan.clear();
        let reset = abort_packet(&mut wired);
        let segment = peek(&reset).expect("parses");
        accept(&mut wired.owner, &reset, &segment);

        assert_eq!(wired.owner.scans, 1, "still once, still last");
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
        // The other half of the same ordering, on the path where getting it wrong loses data rather than
        // merely reordering it. A scan entered before the seal finds a flow whose bridge is not halted; if
        // that flow's socket has reached `Closed`, `Bridge::teardown` cancels a worker that is still flushing
        // bytes this daemon acknowledged. The arm and the seal have to be behind it.
        let sent = pattern(200);
        let mut wired = Wired::new(1024, None);
        wired.established();
        wired.owner.at_scan.clear();

        // A sentinel deadline this FIN's own floor cannot be confused with: about to expire, and nothing the
        // packet does could leave it in place except failing to rearm at all. `established` has already armed
        // a real one, so "some deadline is set" would have been true before the FIN ever arrived.
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
        assert!(!wired.cancelled(), "and nothing cancelled the flush");
        assert_eq!(wired.drain_worker(), sent);
    }

    #[test]
    fn a_reserved_tail_is_the_sockets_own_receive_capacity() {
        // The single sizing authority, at the one place it is read. A constructor that answered anything else
        // would make every claim about an unsplittable extraction false.
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
