//! Bounded Tokio byte streams bridge each terminated TCP connection to its client-side socket.
use std::future::{poll_fn, Future};
use std::pin::{pin, Pin};
use std::task::{Context, Poll, Waker};

use smoltcp::socket::tcp::{Socket, State};
use tokio::io::{
    join, simplex, AsyncRead, AsyncReadExt, AsyncWrite, Chain, Join, ReadBuf, ReadHalf,
    SimplexStream, WriteHalf,
};
use tokio::task::unconstrained;
use tokio_util::sync::CancellationToken;

use crate::shared::lifetime::{opened, peer_finished, Ending};

/// The worker's whole side of one flow: an ordinary `AsyncRead + AsyncWrite`, built out of Tokio's own
/// combinators and nothing else.
pub type Worker =
    Join<Chain<ReadHalf<SimplexStream>, ReadHalf<SimplexStream>>, WriteHalf<SimplexStream>>;

/// How large the reserved terminal tail is, which is not a number a caller chooses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailCapacity(usize);

impl TailCapacity {
    /// Production's only constructor: the capacity of the very buffer the ending comes out of.
    pub fn of(socket: &Socket) -> Self {
        Self(socket.recv_capacity())
    }

    /// A capacity that is not a socket's, so that the one branch production cannot reach is still held down.
    #[cfg(test)]
    pub(super) fn undersized(bytes: usize) -> Self {
        Self(bytes)
    }

    pub fn bytes(self) -> usize {
        self.0
    }
}

/// Builds one flow's byte bridge: the owner's half, and the worker's.
pub fn bridge(main: usize, tail: TailCapacity) -> (Bridge, Worker) {
    // Owner to worker, worker to owner, and the reserved ending: three bounds with three lifetimes.
    let (down_read, down_write) = simplex(main);
    let (up_read, up_write) = simplex(main);
    let (tail_read, tail_write) = simplex(tail.bytes());
    (
        Bridge {
            down: down_write,
            up: up_read,
            tail: tail_write,
            finished: false,
            upstream: Upstream::Open,
            established: false,
        },
        join(down_read.chain(tail_read), up_write),
    )
}

/// What one flow's turn across the bridge did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Crossing {
    /// Bytes that reached the client's send buffer.
    pub to_client: usize,
    /// Bytes taken out of the client's receive buffer.
    pub to_upstream: usize,
    /// Whether bytes or the ordered EOF reached the client's send side.
    pub delivered: bool,
    /// Whether anything at all changed - bytes either way, a half-close, or a direction ending. `false` means
    /// every wake this flow could still use has been registered, so the owner may wait.
    pub moved: bool,
    /// The worker disappeared while the client still had bytes for it, so the owner stops draining.
    pub stranded: bool,
    /// The reserved terminal tail could not take the client's ending; the owner must fence and cancel it.
    pub broken: Option<&'static str>,
}

/// What one attempt to extract a client's ending came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sealed {
    /// No clean ending is pending on this socket, so there was nothing to extract.
    NotDue,
    /// Extracted whole. The flow is cleanly halted and its worker owes the upstream a flush.
    Whole { moved: usize },
    /// The worker's reader reported itself gone. Not a failure: it races that worker's own task terminal,
    /// and an invariant report here would cry wolf on every one. A `simplex` cannot currently produce it -
    /// see [Crossing::stranded].
    WorkerGone { moved: usize },
    /// The tail could not take the ending. Both pipes are closed and the flow is not halted; the owner has
    /// to end it abortively and say so.
    Broken { moved: usize, why: &'static str },
}

impl Sealed {
    /// How many bytes crossed, whatever the outcome.
    pub fn moved(self) -> usize {
        match self {
            Sealed::NotDue => 0,
            Sealed::Whole { moved }
            | Sealed::WorkerGone { moved }
            | Sealed::Broken { moved, .. } => moved,
        }
    }
}

/// What one direction of one crossing did.
enum Moved {
    /// This many bytes moved. Never zero: a zero-byte crossing is [Moved::Idle], because reporting progress
    /// for it would spin an owner's loop.
    Bytes(usize),
    /// The far end of this direction is over: the worker shut its write half down, or went away.
    Ended,
    /// Nothing moved, and whatever could have made it move has this task registered for it.
    Idle,
}

/// What the Closed-socket check did to one flow's worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    /// The stack has not finished with this socket, so there was nothing to decide.
    Live,
    /// The stack has finished with it and this flow owes nobody a flush. The worker was cancelled, which is
    /// abortive and is meant to be.
    Cancelled,
    /// The stack has finished with it, but the client half-closed cleanly and the worker may still be
    /// writing bytes this daemon acknowledged. Left running: its own completion is what ends the flow, and
    /// what bounds it is the deadline its half-close already armed, any retirement, and session shutdown.
    Flushing,
}

/// What the owner still has toward this flow's worker, and how it came to be that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upstream {
    /// The owner still has a write half toward the worker, and the client has not finished sending.
    Open,
    /// The client's half-close has been propagated, and *after* every byte it sent was across. What is left
    /// is the worker writing those bytes upstream, which is ordinary work rather than a teardown to cut
    /// short.
    Halted,
    /// There is nowhere left to put the client's bytes: the upstream half went - because it failed, or
    /// because it completed and left the flow closing client-side - or the reserved tail could not take the
    /// ending.
    Gone,
}

/// The owner's half of one flow's bridge, and what it has already learned about the flow's two directions.
pub struct Bridge {
    /// Owner to worker: the client's payload on its way upstream.
    down: WriteHalf<SimplexStream>,
    /// Worker to owner: the upstream's payload on its way to the client.
    up: ReadHalf<SimplexStream>,
    /// The reserved one-way tail, empty until the client's ending is extracted into it.
    tail: WriteHalf<SimplexStream>,
    /// Set once the worker's half has reported the end of its stream, which is strictly after every byte it
    /// wrote. Read to close the client's send side exactly once.
    finished: bool,
    upstream: Upstream,
    /// Set once the client's handshake has completed.
    established: bool,
}

impl Bridge {
    /// Whether the worker's half has reported the end of its stream.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Whether this owner still has somewhere to put the client's bytes.
    pub fn sending(&self) -> bool {
        matches!(self.upstream, Upstream::Open)
    }

    /// Whether the client's half-close was propagated cleanly: every byte it sent is across, and the end of
    /// its stream followed them.
    pub fn halted(&self) -> bool {
        matches!(self.upstream, Upstream::Halted)
    }

    /// Stops draining the client's receive buffer for a flow whose upstream half has gone.
    pub fn stop_sending(&mut self) {
        self.upstream = Upstream::Gone;
    }

    /// Where this flow stands in the clean client-ending lifecycle, read from this bridge and the phase its
    /// client-side socket is in together.
    pub fn ending(&self, state: State) -> Ending {
        match self.upstream {
            // Nowhere left to put the client's bytes, so there is no flush to protect and nothing to wait
            // for. A flow closing client-side and one whose worker went are both this.
            Upstream::Gone => Ending::Ordinary,
            Upstream::Halted => Ending::Flushing,
            Upstream::Open if self.established && peer_finished(state) => Ending::Pending,
            Upstream::Open => Ending::Ordinary,
        }
    }

    /// The Closed-socket check: what a client-side socket in this phase does to the worker still attached to
    /// this bridge, and the cancellation that follows from it.
    pub fn teardown(&self, state: State, cancel: &CancellationToken) -> Teardown {
        if state != State::Closed {
            return Teardown::Live;
        }
        if self.halted() {
            return Teardown::Flushing;
        }
        cancel.cancel();
        Teardown::Cancelled
    }

    /// One flow's turn: both directions, the client's ending, and this owner's own.
    pub fn cross(&mut self, socket: &mut Socket, cx: &mut Context<'_>) -> Crossing {
        let state = socket.state();
        self.established |= opened(state);
        let mut crossing = Crossing::default();

        // Upstream to client first, because a full stack send buffer is what throttles the remote. Nothing is
        // read while that buffer cannot take a byte: an empty destination would be answered with a
        // registration-free zero, and what refills the buffer is an acknowledgement the owner is woken for.
        if !self.finished && socket.can_send() {
            match take(&mut self.up, socket, cx) {
                Moved::Bytes(sent) => {
                    crossing.to_client = sent;
                    crossing.delivered = true;
                    crossing.moved = true;
                }
                // The worker finished sending, so the client is told the same way rather than reset -
                // strictly after everything the bridge held, because the bridge delivers its bytes before it
                // reports its end.
                Moved::Ended => self.finished = true,
                Moved::Idle => {}
            }
        }
        // This owner's own FIN - and never while the client's own ending is still in the stack.
        if self.finished && socket.may_send() && !(peer_finished(state) && socket.can_recv()) {
            socket.close();
            crossing.delivered = true;
            crossing.moved = true;
        }

        // Client to upstream, and only ever as much as the bridge will take right now: leaving the rest in
        // the receive buffer is what closes the client's window instead of buffering here.
        if self.sending() && socket.can_recv() {
            match give(&mut self.down, socket, cx) {
                Moved::Bytes(taken) => {
                    crossing.to_upstream = taken;
                    crossing.moved = true;
                }
                Moved::Ended => {
                    self.upstream = Upstream::Gone;
                    crossing.stranded = true;
                    crossing.moved = true;
                }
                Moved::Idle => {}
            }
        }
        // Ingress normally seals the flow before the traffic pass reaches it; this is the defensive path.
        match self.seal(socket) {
            Sealed::NotDue => {}
            Sealed::Whole { moved } => {
                crossing.to_upstream += moved;
                crossing.moved = true;
            }
            Sealed::WorkerGone { moved } => {
                crossing.to_upstream += moved;
                crossing.moved = true;
                crossing.stranded = true;
            }
            Sealed::Broken { moved, why } => {
                crossing.to_upstream += moved;
                crossing.moved = true;
                crossing.broken = Some(why);
            }
        }

        crossing
    }

    /// Extracts the client's ending if one is due, and answers what happened.
    pub fn seal(&mut self, socket: &mut Socket) -> Sealed {
        let state = socket.state();
        // The handshake may have completed on this very packet, so this is read here too rather than only in
        // a crossing that has not happened yet.
        self.established |= opened(state);
        if !(self.established && opened(state) && self.sending() && peer_finished(state)) {
            return Sealed::NotDue;
        }
        self.extract(socket)
    }

    /// Extracts the client's ending: everything it sent, then the end of its stream, in **one** step.
    fn extract(&mut self, socket: &mut Socket) -> Sealed {
        let mut moved = 0;
        let outcome = {
            let Bridge { down, tail, .. } = &mut *self;
            let step = poll_fn(|cx| {
                // Closing the downward pipe first is what orders the two halves of the stream.
                if !shut(down, cx) {
                    return Poll::Ready(Err("the main pipe refused to close"));
                }
                // Every contiguous run of the receive ring, not just the largest one `recv` hands out.
                while socket.can_recv() {
                    match give(tail, socket, cx) {
                        Moved::Bytes(taken) => moved += taken,
                        // The reader has gone. Its task's own terminal is on its way, so this is a race and
                        // not a fault.
                        Moved::Ended => return Poll::Ready(Ok(false)),
                        // With the cooperative budget out of the way this is a tail smaller than the receive
                        // buffer, which [TailCapacity] makes unconstructible in production.
                        Moved::Idle => {
                            return Poll::Ready(Err(
                                "the reserved tail could not take the client's ending",
                            ))
                        }
                    }
                }
                if shut(tail, cx) {
                    Poll::Ready(Ok(true))
                } else {
                    Poll::Ready(Err("the terminal tail refused to close"))
                }
            });
            // One poll, and it always answers `Ready`: nothing above can return `Pending`. A noop waker,
            // because there is nothing here worth waking the owner for - see [Bridge::seal].
            match pin!(unconstrained(step)).poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(outcome) => outcome,
                Poll::Pending => Err("the terminal extraction yielded"),
            }
        };
        // Both, on every path, before anything else can look at this flow.
        self.close_both();
        match outcome {
            Ok(true) => {
                self.upstream = Upstream::Halted;
                Sealed::Whole { moved }
            }
            Ok(false) => {
                self.upstream = Upstream::Gone;
                Sealed::WorkerGone { moved }
            }
            Err(why) => {
                self.upstream = Upstream::Gone;
                Sealed::Broken { moved, why }
            }
        }
    }

    /// Closes both write halves toward the worker. Idempotent, and the one thing every ending has in common.
    fn close_both(&mut self) {
        let mut cx = Context::from_waker(Waker::noop());
        let down = shut(&mut self.down, &mut cx);
        let tail = shut(&mut self.tail, &mut cx);
        debug_assert!(
            down && tail,
            "an in-memory pipe closes its write side synchronously"
        );
    }
}

/// Closes both of this owner's write halves when its side of the bridge goes.
impl Drop for Bridge {
    fn drop(&mut self) {
        self.close_both();
    }
}

/// Closes one write half. Answers whether it is closed, which for an in-memory pipe is always.
fn shut(half: &mut (impl AsyncWrite + Unpin), cx: &mut Context<'_>) -> bool {
    matches!(Pin::new(half).poll_shutdown(cx), Poll::Ready(Ok(())))
}

/// Reads the bridge straight into the client's send buffer, taking exactly what fits and nothing more.
fn take(bridge: &mut (impl AsyncRead + Unpin), socket: &mut Socket, cx: &mut Context<'_>) -> Moved {
    let Ok(moved) = socket.send(|destination| {
        let mut buffer = ReadBuf::new(destination);
        let moved = match Pin::new(bridge).poll_read(cx, &mut buffer) {
            Poll::Ready(Ok(())) => match buffer.filled().len() {
                0 => Moved::Ended,
                filled => Moved::Bytes(filled),
            },
            // A `simplex` reader never answers this, and an end is what any reader that did would mean here:
            // the bytes this direction still owed are gone with whatever produced them.
            Poll::Ready(Err(_)) => Moved::Ended,
            Poll::Pending => Moved::Idle,
        };
        (buffer.filled().len(), moved)
    }) else {
        // The caller asked `can_send`, so the state cannot have changed underneath it.
        return Moved::Idle;
    };
    moved
}

/// Writes the client's receive buffer straight into the bridge, consuming exactly what the bridge took.
fn give(
    bridge: &mut (impl AsyncWrite + Unpin),
    socket: &mut Socket,
    cx: &mut Context<'_>,
) -> Moved {
    let Ok(moved) = socket.recv(|source| match Pin::new(bridge).poll_write(cx, source) {
        Poll::Ready(Ok(0)) => (0, Moved::Idle),
        Poll::Ready(Ok(taken)) => (taken, Moved::Bytes(taken)),
        // The reader's half of this pipe is gone, which `SimplexStream` reports as a broken pipe. Anything
        // else would be a writer this daemon does not build; both mean the same thing here.
        Poll::Ready(Err(_)) => (0, Moved::Ended),
        Poll::Pending => (0, Moved::Idle),
    }) else {
        // The caller asked `can_recv`, so the state cannot have changed underneath it.
        return Moved::Idle;
    };
    moved
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketSet};
    use smoltcp::phy::{Loopback, Medium};
    use smoltcp::socket::tcp::{SocketBuffer, State};
    use smoltcp::time::Instant as SmolInstant;
    use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};
    use std::time::{Duration, Instant};

    use tokio::io::{copy_bidirectional_with_sizes, duplex, AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::shared::flow_budget::BRIDGE_BUFFER;
    use crate::shared::lifetime::{peer_finished, rearmed};

    #[derive(Default)]
    struct Counting(AtomicUsize);

    impl Wake for Counting {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct Wired {
        interface: Interface,
        device: Loopback,
        sockets: SocketSet<'static>,
        engine: SocketHandle,
        client: SocketHandle,
        bridge: Bridge,
        worker: Option<Worker>,
        received: Vec<u8>,
        cancel: CancellationToken,
        woken: Arc<Counting>,
        waker: Waker,
        millis: i64,
    }

    impl Wired {
        fn new(stack: usize, bridge: usize) -> Self {
            Self::with_tail(stack, bridge, stack)
        }

        fn with_tail(stack: usize, bridge: usize, tail: usize) -> Self {
            let mut device = Loopback::new(Medium::Ip);
            let mut interface = Interface::new(
                Config::new(HardwareAddress::Ip),
                &mut device,
                SmolInstant::from_millis(0),
            );
            interface.update_ip_addrs(|addresses| {
                addresses
                    .push(IpCidr::new(IpAddress::v4(192, 0, 2, 1), 24))
                    .expect("one address fits");
            });
            let mut sockets = SocketSet::new(Vec::new());
            let engine = sockets.add(socket(stack));
            let client = sockets.add(socket(stack));
            sockets
                .get_mut::<Socket>(engine)
                .listen(80)
                .expect("a fresh socket may listen");
            sockets
                .get_mut::<Socket>(client)
                .connect(
                    interface.context(),
                    (IpAddress::v4(192, 0, 2, 1), 80),
                    49152,
                )
                .expect("a fresh socket may connect");
            let (owner, worker) = super::bridge(bridge, TailCapacity::undersized(tail));
            let woken = Arc::new(Counting::default());
            let waker = Waker::from(Arc::clone(&woken));
            let mut wired = Self {
                interface,
                device,
                sockets,
                engine,
                client,
                bridge: owner,
                worker: Some(worker),
                received: Vec::new(),
                cancel: CancellationToken::new(),
                woken,
                waker,
                millis: 0,
            };
            wired.run();
            assert_eq!(wired.state(wired.engine), State::Established);
            assert_eq!(wired.state(wired.client), State::Established);
            wired
        }

        fn now(&self) -> SmolInstant {
            SmolInstant::from_millis(self.millis)
        }

        fn advance(&mut self, millis: i64) {
            self.millis += millis;
        }

        fn socket(&mut self, handle: SocketHandle) -> &mut Socket<'static> {
            self.sockets.get_mut::<Socket>(handle)
        }

        fn state(&self, handle: SocketHandle) -> State {
            self.sockets.get::<Socket>(handle).state()
        }

        fn cross(&mut self) -> Crossing {
            let Self {
                sockets,
                engine,
                bridge,
                waker,
                ..
            } = self;
            let socket = sockets.get_mut::<Socket>(*engine);
            bridge.cross(socket, &mut Context::from_waker(waker))
        }

        fn poll_stack(&mut self) -> bool {
            let progressed = !matches!(
                self.interface
                    .poll(self.now(), &mut self.device, &mut self.sockets),
                PollResult::None
            );
            let state = self.sockets.get::<Socket>(self.engine).state();
            self.bridge.teardown(state, &self.cancel);
            progressed
        }

        fn packets_until(&mut self, done: impl Fn(&Self) -> bool) -> bool {
            for _ in 0..512 {
                if done(self) {
                    return true;
                }
                let progressed = self.poll_stack();
                self.client_reads();
                if progressed {
                    continue;
                }
                match self.interface.poll_delay(self.now(), &self.sockets) {
                    Some(delay) => self.millis += (delay.total_millis() as i64).clamp(1, 50),
                    None => break,
                }
            }
            done(self)
        }

        fn turn(&mut self) -> (bool, Crossing) {
            let progressed = self.poll_stack();
            let crossing = self.cross();
            self.poll_stack();
            (progressed, crossing)
        }

        fn run(&mut self) -> (usize, usize) {
            let (mut to_client, mut to_upstream) = (0, 0);
            for _ in 0..512 {
                let (progressed, crossing) = self.turn();
                to_client += crossing.to_client;
                to_upstream += crossing.to_upstream;
                let read = self.client_reads();
                if progressed || crossing.moved || read {
                    continue;
                }
                match self.interface.poll_delay(self.now(), &self.sockets) {
                    Some(delay) => self.millis += (delay.total_millis() as i64).clamp(1, 50),
                    None => break,
                }
            }
            (to_client, to_upstream)
        }

        fn worker(&mut self) -> &mut Worker {
            self.worker.as_mut().expect("the worker's half is here")
        }

        fn hand_to_worker(&mut self) -> Worker {
            self.worker.take().expect("handed over once")
        }

        fn worker_completes(&mut self) {
            self.worker_ends_writing();
            drop(self.hand_to_worker());
        }

        fn worker_ends_writing(&mut self) {
            match Pin::new(self.worker()).poll_shutdown(&mut noop()) {
                Poll::Ready(Ok(())) => {}
                other => panic!("an in-memory write half shuts down at once: {other:?}"),
            }
        }

        fn worker_writes(&mut self, bytes: &[u8]) -> Option<usize> {
            match Pin::new(self.worker()).poll_write(&mut noop(), bytes) {
                Poll::Ready(Ok(taken)) => Some(taken),
                Poll::Ready(Err(e)) => panic!("the worker's write failed: {e}"),
                Poll::Pending => None,
            }
        }

        fn worker_reads(&mut self, into: &mut [u8]) -> Option<usize> {
            let mut buffer = ReadBuf::new(into);
            match Pin::new(self.worker()).poll_read(&mut noop(), &mut buffer) {
                Poll::Ready(Ok(())) => Some(buffer.filled().len()),
                Poll::Ready(Err(e)) => panic!("the worker's read failed: {e}"),
                Poll::Pending => None,
            }
        }

        async fn cross_here(&mut self) -> Crossing {
            std::future::poll_fn(|cx| {
                let Self {
                    sockets,
                    engine,
                    bridge,
                    ..
                } = self;
                let socket = sockets.get_mut::<Socket>(*engine);
                Poll::Ready(bridge.cross(socket, cx))
            })
            .await
        }

        async fn step(&mut self) {
            let progressed = self.poll_stack();
            let crossing = self.cross_here().await;
            self.poll_stack();
            let read = self.client_reads();
            if !progressed && !crossing.moved && !read {
                if let Some(delay) = self.interface.poll_delay(self.now(), &self.sockets) {
                    self.millis += (delay.total_millis() as i64).clamp(1, 50);
                }
            }
            tokio::task::yield_now().await;
        }

        fn client_reads(&mut self) -> bool {
            let Self {
                sockets,
                client,
                received,
                ..
            } = self;
            let socket = sockets.get_mut::<Socket>(*client);
            let mut read = false;
            while socket.can_recv() {
                socket
                    .recv(|data| {
                        received.extend_from_slice(data);
                        read |= !data.is_empty();
                        (data.len(), ())
                    })
                    .expect("a socket with data may be read");
            }
            read
        }

        fn delivered(&self) -> &[u8] {
            &self.received
        }

        fn wakes(&self) -> usize {
            self.woken.0.load(Ordering::Relaxed)
        }
    }

    fn socket(buffer: usize) -> Socket<'static> {
        Socket::new(
            SocketBuffer::new(vec![0u8; buffer]),
            SocketBuffer::new(vec![0u8; buffer]),
        )
    }

    fn noop() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|byte| byte as u8).collect()
    }

    #[test]
    fn data_arriving_in_the_bridge_is_what_wakes_the_owner() {
        let mut wired = Wired::new(4096, 4096);
        assert!(!wired.cross().moved);
        let before = wired.wakes();
        assert_eq!(wired.worker_writes(b"x"), Some(1));
        assert_eq!(wired.wakes(), before + 1);
        assert_eq!(wired.cross().to_client, 1);
        wired.run();
        assert_eq!(wired.delivered(), b"x");
    }

    #[test]
    fn capacity_returning_in_the_bridge_is_what_wakes_the_owner() {
        let mut wired = Wired::new(4096, 64);
        let sent = pattern(4096);
        let client = wired.client;
        let queued = wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        assert!(queued > 64);
        wired.run();
        let engine = wired.engine;
        assert!(wired.socket(engine).recv_queue() > 0);
        assert!(!wired.cross().moved);
        let before = wired.wakes();
        let mut taken = [0u8; 1];
        assert_eq!(wired.worker_reads(&mut taken), Some(1));
        assert_eq!(wired.wakes(), before + 1);
        assert_eq!(wired.cross().to_upstream, 1);
    }

    #[test]
    fn the_bridge_bounds_the_read_ahead_and_nothing_else_does() {
        let mut wired = Wired::new(64, BRIDGE_BUFFER);
        let mut written = 0;
        while let Some(taken) = wired.worker_writes(&pattern(4096)) {
            written += taken;
        }
        assert_eq!(written, BRIDGE_BUFFER);
        assert_eq!(wired.worker_writes(b"x"), None);
    }

    #[test]
    fn a_download_arrives_whole_and_in_order_however_small_the_stack_buffers_are() {
        let mut wired = Wired::new(1024, 16 * 1024);
        let sent = pattern(64 * 1024);
        let mut offered = 0;
        while offered < sent.len() {
            match wired.worker_writes(&sent[offered..]) {
                Some(taken) => offered += taken,
                None => {
                    wired.run();
                }
            }
        }
        wired.run();
        assert_eq!(wired.delivered(), sent);
    }

    #[test]
    fn an_upload_reaches_the_worker_whole_and_in_order() {
        let mut wired = Wired::new(1024, 4 * 1024);
        let sent = pattern(64 * 1024);
        let mut offered = 0;
        let mut received = Vec::new();
        let mut scratch = vec![0u8; 4096];
        while offered < sent.len() || received.len() < sent.len() {
            let client = wired.client;
            offered += wired
                .socket(client)
                .send_slice(&sent[offered..])
                .expect("an established socket may send");
            wired.run();
            while let Some(read) = wired.worker_reads(&mut scratch) {
                if read == 0 {
                    break;
                }
                received.extend_from_slice(&scratch[..read]);
            }
        }
        assert_eq!(received, sent);
    }

    #[test]
    fn a_partial_crossing_keeps_the_rest_where_it_was() {
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"abc"), Some(3));
        let first = wired.cross();
        assert_eq!(
            first.to_client, 1,
            "exactly what the send buffer had room for"
        );
        wired.run();
        assert_eq!(wired.delivered(), b"abc");
    }

    #[test]
    fn the_worker_finishing_closes_the_client_after_every_byte_it_wrote() {
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"answer"), Some(6));
        wired.worker_completes();
        wired.run();
        assert_eq!(wired.delivered(), b"answer");
        assert!(wired.bridge.finished());
        assert!(!wired.sockets.get::<Socket>(wired.client).may_recv());
        assert!(matches!(
            wired.state(wired.client),
            State::CloseWait | State::TimeWait | State::Closed
        ));
    }

    #[test]
    fn a_client_that_half_closes_still_gets_its_response() {
        let mut wired = Wired::new(1024, 1024);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(b"request")
            .expect("an established socket may send");
        wired.socket(client).close();
        wired.run();
        let mut scratch = vec![0u8; 64];
        assert_eq!(wired.worker_reads(&mut scratch), Some(7));
        assert_eq!(&scratch[..7], b"request");
        assert_eq!(wired.worker_reads(&mut scratch), Some(0));
        assert!(!wired.bridge.sending());
        assert_eq!(wired.worker_writes(b"response"), Some(8));
        wired.run();
        assert_eq!(wired.delivered(), b"response");
    }

    #[test]
    fn an_abortive_ending_discards_what_the_bridge_was_holding() {
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"undelivered"), Some(11));
        let (owner, _worker) = super::bridge(1, TailCapacity::undersized(1));
        drop(std::mem::replace(&mut wired.bridge, owner));
        wired.run();
        assert_eq!(wired.delivered(), b"");
        assert_eq!(
            wired.worker_reads(&mut [0u8; 8]),
            Some(0),
            "the worker's whole read side ended, so its copy can return"
        );
    }

    #[test]
    fn a_flow_closing_client_side_stops_the_owner_draining_for_it() {
        let mut wired = Wired::new(4096, 64);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&pattern(4096))
            .expect("an established socket may send");
        wired.run();
        assert!(wired.bridge.sending());
        wired.bridge.stop_sending();
        assert!(!wired.bridge.sending());
        let engine = wired.engine;
        let queued = wired.socket(engine).recv_queue();
        let after = wired.cross();
        assert_eq!(
            after.to_upstream, 0,
            "nothing more is taken out of the stack"
        );
        assert_eq!(
            wired.socket(engine).recv_queue(),
            queued,
            "and the client's window stays where it was"
        );
    }

    #[test]
    fn a_crossing_that_moves_nothing_registers_rather_than_reporting_progress() {
        let mut wired = Wired::new(4096, 64);
        for _ in 0..8 {
            assert_eq!(wired.cross(), Crossing::default());
        }
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"ab"), Some(2));
        assert!(wired.cross().moved);
        let quiet = wired.wakes();
        assert!(!wired.cross().moved);
        assert_eq!(wired.wakes(), quiet);
    }

    #[test]
    fn the_worker_sees_main_bytes_then_tail_bytes_then_one_end_of_stream() {
        let mut wired = Wired::new(4096, 8);
        let sent = pattern(64);
        let client = wired.client;
        assert_eq!(
            wired
                .socket(client)
                .send_slice(&sent)
                .expect("an established socket may send"),
            sent.len()
        );
        wired.socket(client).close();
        wired.run();
        assert!(wired.bridge.halted());
        let engine = wired.engine;
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "nothing of the client's is left inside smoltcp"
        );

        let mut received = Vec::new();
        let mut scratch = vec![0u8; 8];
        loop {
            match wired.worker_reads(&mut scratch) {
                Some(0) => break,
                Some(read) => received.extend_from_slice(&scratch[..read]),
                None => panic!("a sealed ending never blocks its reader"),
            }
        }
        assert_eq!(received, sent);
        assert_eq!(
            wired.worker_reads(&mut scratch),
            Some(0),
            "and the end of the stream stays the end"
        );
    }

    #[test]
    fn a_full_main_bridge_and_a_client_ending_together_lose_nothing() {
        let mut wired = Wired::new(4096, 8);
        let sent = pattern(64);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        wired.run();
        wired.worker_completes();
        wired.run();
        assert_eq!(
            wired.state(wired.engine),
            State::Closed,
            "the teardown ran to completion"
        );
        assert!(
            !wired.cancel.is_cancelled(),
            "and a clean ending was never cancelled"
        );
    }

    #[test]
    fn the_local_fin_waits_for_the_crossing_that_extracts_the_clients_ending() {
        let mut wired = Wired::new(4096, 8);
        let sent = pattern(64);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        wired.worker_ends_writing();
        assert!(wired.packets_until(|w| peer_finished(w.state(w.engine))));
        let engine = wired.engine;
        assert_eq!(wired.state(engine), State::CloseWait);
        assert!(
            wired.socket(engine).recv_queue() > 0,
            "and its bytes are here"
        );

        let crossed = wired.cross();
        assert!(crossed.moved);
        assert_eq!(
            wired.state(wired.engine),
            State::CloseWait,
            "the local FIN waited for the extraction"
        );
        assert!(wired.bridge.halted());
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "and left nothing of the client's in the stack"
        );

        wired.run();
        assert_eq!(wired.state(wired.engine), State::Closed);
        assert!(
            !wired.cancel.is_cancelled(),
            "a clean ending is never cancelled"
        );
    }

    #[tokio::test]
    async fn a_depleted_cooperative_budget_does_not_split_the_extraction() {
        let sent = pattern(64);
        let mut wired = Wired::new(4096, 8);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        assert!(wired.packets_until(|w| peer_finished(w.state(w.engine))));
        let engine = wired.engine;
        assert!(wired.socket(engine).recv_queue() > 0);

        let (_drain, mut spender) = simplex(4096);
        for _ in 0..160 {
            let _ =
                std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut spender).poll_write(cx, b"x")))
                    .await;
        }

        let crossing = std::future::poll_fn(|cx| {
            let Wired {
                sockets,
                engine,
                bridge,
                ..
            } = &mut wired;
            Poll::Ready(bridge.cross(sockets.get_mut::<Socket>(*engine), cx))
        })
        .await;
        assert!(
            crossing.broken.is_none(),
            "a depleted budget is not a fault"
        );
        assert!(
            wired.bridge.halted(),
            "and the ending finished in that one crossing"
        );
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "with nothing left in the stack for a poll to clear"
        );

        tokio::task::yield_now().await;
        let mut received = Vec::new();
        let mut scratch = vec![0u8; 16];
        loop {
            match wired.worker_reads(&mut scratch) {
                Some(0) => break,
                Some(read) => received.extend_from_slice(&scratch[..read]),
                None => panic!("a finished ending never blocks its reader"),
            }
        }
        assert_eq!(received, sent);
        assert_eq!(wired.worker_reads(&mut scratch), Some(0));
    }

    #[test]
    fn a_wrapped_receive_ring_drains_both_of_its_contiguous_runs() {
        let mut wired = Wired::new(64, 8);
        let client = wired.client;
        let first = pattern(48);
        assert_eq!(
            wired
                .socket(client)
                .send_slice(&first)
                .expect("an established socket may send"),
            48
        );
        let engine = wired.engine;
        assert!(wired.packets_until(|w| w.sockets.get::<Socket>(w.engine).recv_queue() == 48));

        let mut received = Vec::new();
        let mut scratch = vec![0u8; 8];
        while received.len() < 40 {
            wired.cross();
            let read = wired
                .worker_reads(&mut scratch)
                .expect("the main stream has bytes");
            assert!(read > 0);
            received.extend_from_slice(&scratch[..read]);
        }
        assert_eq!(received.len(), 40);
        assert_eq!(wired.socket(engine).recv_queue(), 8);
        assert!(wired.packets_until(|w| w.sockets.get::<Socket>(w.client).send_queue() == 0));

        let second: Vec<u8> = (0..32u16).map(|byte| (128 + byte) as u8).collect();
        assert_eq!(
            wired
                .socket(client)
                .send_slice(&second)
                .expect("an established socket may send"),
            32
        );
        wired.socket(client).close();
        assert!(wired.packets_until(|w| peer_finished(w.state(w.engine))));
        assert_eq!(
            wired.socket(engine).recv_queue(),
            40,
            "forty bytes, across both of the ring's runs"
        );

        let crossing = wired.cross();
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "both contiguous runs drained, not just the first"
        );
        assert_eq!(crossing.to_upstream, 40);
        assert!(wired.bridge.halted());

        loop {
            match wired.worker_reads(&mut scratch) {
                Some(0) => break,
                Some(read) => received.extend_from_slice(&scratch[..read]),
                None => panic!("a finished ending never blocks its reader"),
            }
        }
        let mut expected = first.clone();
        expected.extend_from_slice(&second);
        assert_eq!(received, expected);
    }

    #[test]
    fn a_client_that_resets_records_no_clean_half_close_and_takes_its_worker_with_it() {
        let mut wired = Wired::new(4096, 8);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&pattern(64))
            .expect("an established socket may send");
        wired.run();
        wired.socket(client).abort();
        wired.run();
        assert_eq!(wired.state(wired.engine), State::Closed);
        assert!(
            !wired.bridge.halted(),
            "a reset is owed no flush and no wait"
        );
        assert!(
            wired.cancel.is_cancelled(),
            "a socket the stack has finished with takes its worker with it"
        );
    }

    #[test]
    fn the_extraction_holds_its_order_against_a_worker_that_is_not_reading() {
        let sent = pattern(200);
        let mut wired = Wired::new(1024, 8);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        wired.run();

        let engine = wired.engine;
        assert!(wired.bridge.halted());
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "the extraction does not wait for the worker"
        );

        wired.advance(30_000);
        wired.run();

        let mut received = Vec::new();
        let mut scratch = vec![0u8; 8];
        loop {
            match wired.worker_reads(&mut scratch) {
                Some(0) => break,
                Some(read) => received.extend_from_slice(&scratch[..read]),
                None => panic!("nothing is left to wait for"),
            }
        }
        assert_eq!(received, sent);
        assert_eq!(
            wired.worker_reads(&mut scratch),
            Some(0),
            "one end of stream, after both pipes"
        );
    }

    #[test]
    fn a_tail_too_small_for_the_ending_is_refused_rather_than_truncated() {
        let sent = pattern(512);
        let mut wired = Wired::with_tail(1024, 8, 16);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        wired.run();

        assert!(
            !wired.bridge.halted(),
            "a short extraction is not a clean half-close"
        );
        assert_eq!(
            wired.bridge.ending(State::CloseWait),
            Ending::Ordinary,
            "so nothing is owed a flush"
        );
        assert_eq!(
            wired.bridge.ending(State::Closed),
            Ending::Ordinary,
            "and the Closed-socket check will cancel"
        );
        let cancel = CancellationToken::new();
        assert_eq!(
            wired.bridge.teardown(State::Closed, &cancel),
            Teardown::Cancelled
        );
        assert!(cancel.is_cancelled());

        let mut scratch = vec![0u8; 64];
        let mut received = Vec::new();
        for _ in 0..512 {
            match wired.worker_reads(&mut scratch) {
                Some(0) => break,
                Some(read) => received.extend_from_slice(&scratch[..read]),
                None => panic!("a refused ending never leaves its reader waiting"),
            }
        }
        assert_eq!(
            wired.worker_reads(&mut scratch),
            Some(0),
            "the end of the stream, and it stays ended"
        );
        assert!(
            received.len() < sent.len(),
            "and it is a refusal rather than a whole ending"
        );
    }

    #[test]
    fn the_owners_half_going_ends_both_of_the_workers_pipes() {
        let (bridge, mut worker) = super::bridge(8, TailCapacity::undersized(8));
        drop(bridge);
        let mut scratch = [0u8; 8];
        let mut buffer = ReadBuf::new(&mut scratch);
        assert!(matches!(
            Pin::new(&mut worker).poll_read(&mut noop(), &mut buffer),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(buffer.filled().len(), 0);
    }

    #[test]
    fn a_reader_that_merely_went_away_is_not_an_invariant_failure() {
        assert_eq!(Sealed::WorkerGone { moved: 7 }.moved(), 7);
        assert_eq!(
            Sealed::Broken {
                moved: 7,
                why: "why"
            }
            .moved(),
            7
        );
        assert_eq!(Sealed::NotDue.moved(), 0);
        assert_eq!(Sealed::Whole { moved: 7 }.moved(), 7);
    }

    #[test]
    fn a_cleanly_halted_flow_is_left_to_finish_flushing_when_its_socket_closes() {
        let mut wired = Wired::new(1024, 1024);
        let client = wired.client;
        wired.socket(client).close();
        wired.run();
        assert!(wired.bridge.halted());
        wired.worker_completes();
        wired.run();
        assert_eq!(wired.state(wired.engine), State::Closed);
        assert!(
            !wired.cancel.is_cancelled(),
            "a clean flush is not something a Closed socket may cut short"
        );
        let armed = Instant::now() + Duration::from_secs(600);
        assert_eq!(
            rearmed(
                Some(armed),
                State::Closed,
                wired.bridge.ending(State::Closed),
                Instant::now()
            ),
            Some(armed),
            "a flushing flow in a terminal phase keeps exactly the bound it had"
        );
    }

    #[test]
    fn the_closed_socket_check_decides_by_phase_and_by_the_halt() {
        let (bridge, _worker) = super::bridge(8, TailCapacity::undersized(8));
        let cancel = CancellationToken::new();
        for state in [State::Established, State::CloseWait, State::TimeWait] {
            assert_eq!(bridge.teardown(state, &cancel), Teardown::Live);
            assert!(!cancel.is_cancelled(), "{state:?}");
        }
        assert_eq!(bridge.teardown(State::Closed, &cancel), Teardown::Cancelled);
        assert!(cancel.is_cancelled());

        let mut wired = Wired::new(1024, 1024);
        let client = wired.client;
        wired.socket(client).close();
        wired.run();
        assert!(wired.bridge.halted());
        let flushing = CancellationToken::new();
        for _ in 0..4 {
            assert_eq!(
                wired.bridge.teardown(State::Closed, &flushing),
                Teardown::Flushing
            );
        }
        assert!(!flushing.is_cancelled());
    }

    #[tokio::test]
    async fn a_clean_close_drains_every_byte_the_client_sent_before_the_flow_ends() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut wired = Wired::new(4096, 4096);
            let (upstream, mut remote) = duplex(1);
            let worker = wired.hand_to_worker();
            let copy = tokio::spawn(async move {
                let (mut upstream, mut worker) = (upstream, worker);
                copy_bidirectional_with_sizes(&mut upstream, &mut worker, 64, 64).await
            });

            let request = pattern(64);
            let client = wired.client;
            wired
                .socket(client)
                .send_slice(&request)
                .expect("an established socket may send");
            wired.socket(client).close();
            while !wired.bridge.halted() {
                wired.step().await;
            }

            remote
                .write_all(b"response")
                .await
                .expect("the upstream answers");
            remote.shutdown().await.expect("and then ends");
            while wired.state(wired.engine) != State::Closed {
                wired.step().await;
            }

            assert!(!copy.is_finished());
            assert!(wired.bridge.halted());
            assert!(
                !wired.cancel.is_cancelled(),
                "the flow's worker survived the whole teardown"
            );
            let armed = Instant::now() + Duration::from_secs(600);
            assert_eq!(
                rearmed(
                    Some(armed),
                    wired.state(wired.engine),
                    wired.bridge.ending(wired.state(wired.engine)),
                    Instant::now()
                ),
                Some(armed),
                "and the bound it carries is preserved rather than replaced with `now`"
            );
            assert_eq!(wired.delivered(), b"response");

            let mut upstream_received = Vec::new();
            let mut scratch = vec![0u8; 64];
            while upstream_received.len() < request.len() {
                let read = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    remote.read(&mut scratch),
                )
                .await
                .expect("the upstream keeps receiving")
                .expect("a readable upstream");
                assert_ne!(read, 0);
                upstream_received.extend_from_slice(&scratch[..read]);
            }
            assert_eq!(
                upstream_received, request,
                "every byte the client sent reached the upstream, in order"
            );
            let (to_upstream, to_client) = copy
                .await
                .expect("the copy task completes")
                .expect("and completes cleanly");
            assert_eq!(to_upstream as usize, b"response".len());
            assert_eq!(to_client as usize, request.len());
        })
        .await
        .expect("the whole exchange is bounded");
    }

    #[tokio::test]
    async fn a_full_bridge_survives_the_close_timer_when_the_upstream_ended_first() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut wired = Wired::new(4096, 8);
            let (upstream, mut remote) = duplex(1);
            let worker = wired.hand_to_worker();
            let copy = tokio::spawn(async move {
                let (mut upstream, mut worker) = (upstream, worker);
                copy_bidirectional_with_sizes(&mut upstream, &mut worker, 64, 64).await
            });

            remote.shutdown().await.expect("the upstream ends");
            while !matches!(wired.state(wired.engine), State::FinWait1 | State::FinWait2) {
                wired.step().await;
            }

            let sent = pattern(64);
            let client = wired.client;
            wired
                .socket(client)
                .send_slice(&sent)
                .expect("an established socket may send");
            wired.socket(client).close();
            assert!(wired.packets_until(|w| peer_finished(w.state(w.engine))));
            assert_eq!(
                wired.state(wired.engine),
                State::TimeWait,
                "smoltcp reaches TIME-WAIT in one step here"
            );
            let engine = wired.engine;
            assert!(
                wired.socket(engine).recv_queue() > 0,
                "with the client's bytes still inside it"
            );

            let crossed = wired.cross();
            assert!(crossed.moved);
            assert!(wired.bridge.halted());
            assert_eq!(
                wired.socket(engine).recv_queue(),
                0,
                "before the close timer could ever clear it"
            );

            wired.advance(11_000);
            while wired.state(wired.engine) != State::Closed {
                wired.step().await;
            }
            assert!(!copy.is_finished());
            assert!(
                !wired.cancel.is_cancelled(),
                "and a clean ending is not cancelled by the socket the stack finished with"
            );

            let mut upstream_received = Vec::new();
            let mut scratch = vec![0u8; 64];
            while upstream_received.len() < sent.len() {
                let read = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    remote.read(&mut scratch),
                )
                .await
                .expect("the upstream keeps receiving")
                .expect("a readable upstream");
                assert_ne!(read, 0);
                upstream_received.extend_from_slice(&scratch[..read]);
            }
            assert_eq!(
                upstream_received, sent,
                "every byte the client sent survived the close timer, in order"
            );
            copy.await
                .expect("the copy task completes")
                .expect("and completes cleanly");
        })
        .await
        .expect("the whole exchange is bounded");
    }

    #[test]
    fn a_flow_delivering_a_long_response_after_its_client_half_closed_is_never_idle() {
        let mut wired = Wired::new(1024, 1024);
        let client = wired.client;
        wired.socket(client).close();
        wired.run();
        assert!(wired.bridge.halted());
        assert_eq!(
            wired.state(wired.engine),
            State::CloseWait,
            "the client is done sending and this owner is not"
        );
        assert_eq!(
            wired.bridge.ending(State::CloseWait),
            Ending::Flushing,
            "propagated, and still delivering"
        );
        let stale = Instant::now() + Duration::from_millis(1);
        let refreshed = rearmed(
            Some(stale),
            State::CloseWait,
            wired.bridge.ending(State::CloseWait),
            Instant::now(),
        )
        .expect("still bounded");
        assert!(
            refreshed > stale + Duration::from_secs(60),
            "delivering to the client refreshes the floor rather than letting it run out"
        );
        assert_eq!(wired.worker_writes(b"a long response"), Some(15));
        wired.run();
        assert_eq!(wired.delivered(), b"a long response");
        assert!(!wired.cancel.is_cancelled());
    }
}
