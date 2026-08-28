//! One flow's half of a bounded byte bridge, and the crossing between it and the client-side TCP socket.
//!
//! The bridge is Tokio's own in-memory pipes: an ordinary bounded stream whose other half a flow's worker
//! holds as a plain `AsyncRead + AsyncWrite`. Nothing of this daemon's travels on it. What is here is the
//! *decision* - which direction may move, how much, and what a direction ending means - and the owner that
//! holds a `smoltcp` interface, a socket set and a task per flow wires it up.
//!
//! # The four quadrants, and which wake each of them has
//!
//! Every stall a terminated flow can be in is one of four, and none needs a wake of ours. Two are the
//! bridge's: `poll_read` registers the owner with it and the worker writing one byte wakes it, `poll_write`
//! registers and the worker reading one byte wakes it. The other two are a full client send buffer and an
//! empty client receive buffer, and nothing is registered for either - deliberately, because both change
//! only when a packet or a stack timer is processed, and the owner is the only thing that processes those
//! and re-enters this crossing straight afterwards. That is why the pinned `smoltcp`'s `async` feature is
//! off: its wakers exist for a task that does *not* run inside the poll advancing the interface, so here
//! they would fire into a task already scheduled to look.
//!
//! # Nothing is held between the two buffers
//!
//! [Bridge::cross] reads the bridge straight into `smoltcp`'s send buffer and writes `smoltcp`'s receive
//! buffer straight into the bridge, at the contiguous slices each offers. A short move is simply fewer bytes,
//! never a remainder somebody has to remember: what the bridge would not take stays in the receive buffer,
//! which closes the client's window, and what the send buffer would not take stays in the bridge, which stops
//! the worker reading its upstream. Neither direction can drop a byte of an acknowledged stream.

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
///
/// Reading is `down.chain(tail)` and writing is the upward pipe, joined back into one object - so a worker
/// sees a single bidirectional stream and never learns that its ending arrives on a second pipe. Neither
/// [Worker] nor [Bridge] implements a byte queue, a capacity signal or a waker slot: `simplex`, `chain` and
/// `join` are maintained library types and every wake below is theirs.
///
/// **Three `simplex` pipes rather than a `duplex` behind a `split`.** Each `simplex` is itself a `split` and
/// still owns one `Arc<Mutex<SimplexStream>>`; what naming the three directions outright removes is the
/// *extra outer* split a `duplex` half needed to reach `chain` - one more allocation, and a second mutex
/// every worker read and write took before reaching the pipe's own. It also gives the terminal tail an order
/// against the main stream, which a `duplex` cannot express, and three explicit buffers for the charge to
/// name rather than two plus a wrapper nobody counted.
///
/// Both TCP kinds get this same object from [bridge]: an ordinary relayed flow hands it to
/// `copy_bidirectional_with_sizes`, and a DNS-over-TCP transport reads and writes it directly.
pub type Worker =
    Join<Chain<ReadHalf<SimplexStream>, ReadHalf<SimplexStream>>, WriteHalf<SimplexStream>>;

/// How large the reserved terminal tail is, which is not a number a caller chooses.
///
/// The client-side receive buffer's own capacity, taken from the socket rather than from a field beside it:
/// the tail holds everything that buffer can hold, in one go, and a sizing field is something two call sites
/// can disagree about. There is one production constructor and it needs the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailCapacity(usize);

impl TailCapacity {
    /// Production's only constructor: the capacity of the very buffer the ending comes out of.
    pub fn of(socket: &Socket) -> Self {
        Self(socket.recv_capacity())
    }

    /// A capacity that is not a socket's, so that the one branch production cannot reach is still held down.
    ///
    /// `#[cfg(test)]`, and this module's tests and [crate::shared::ingress]'s are its only callers: a tail
    /// too small for an ending is what [Bridge::extract] must refuse rather than truncate, and no production
    /// caller can build one because [TailCapacity::of] is the only other way to make one. A constructor
    /// visible outside a test build would make the invariant this type exists to state simply untrue.
    #[cfg(test)]
    pub(super) fn undersized(bytes: usize) -> Self {
        Self(bytes)
    }

    pub fn bytes(self) -> usize {
        self.0
    }
}

/// Builds one flow's byte bridge: the owner's half, and the worker's.
///
/// `main` bounds each steady-state direction. The tail stays **empty** until the client's ending is extracted
/// into it, which is what makes that one uninterruptible step - see [Bridge::extract].
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
    /// Whether anything reached the client's *send side* - bytes, or the ordered end of stream that follows
    /// them. This is what an owner refreshes an idle floor on, and it is deliberately not the other
    /// direction: a client that stops acknowledging fills its send buffer, which stops this and lets the flow
    /// expire.
    pub delivered: bool,
    /// Whether anything at all changed - bytes either way, a half-close, or a direction ending. `false` means
    /// every wake this flow could still use has been registered, so the owner may wait.
    pub moved: bool,
    /// The worker's half reported itself gone while the client still had bytes for it, so the owner stops
    /// draining. Ordinary rather than alarming: it races that worker's own terminal, which is what ends the
    /// flow. Not currently reachable through a `simplex` - dropping a `ReadHalf` sets no flag, so a pipe
    /// reports `BrokenPipe` only after a `close_read` this daemon never performs, and what tells the owner a
    /// worker has gone is [Bridge::stop_sending]. Kept because the distinction is the point: if a pipe ever
    /// does answer this, it must stay an ordinary ending and never [Crossing::broken].
    pub stranded: bool,
    /// The reserved terminal tail could not take the client's ending, which is an invariant failure and not
    /// a state. `Some` is a construction error - see [Bridge::extract] - and the owner must fence the socket,
    /// cancel the worker and report it, which [crate::shared::ingress::tail_failed] is the one place that
    /// happens.
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
///
/// Told apart because they are three different things to the flow: bytes are traffic, the end of a direction
/// is a half-close to propagate, and nothing at all is a registration already made.
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
///
/// Three states rather than a `bool`, because the one thing that reads this has to tell a *clean* client
/// half-close from the two endings that are not one: cancelling a worker still flushing the client's own
/// acknowledged bytes upstream drops exactly those bytes. There is deliberately no state between
/// [Upstream::Open] and [Upstream::Halted] - see [Bridge::extract].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Upstream {
    /// The owner still has a write half toward the worker, and the client has not finished sending.
    Open,
    /// The client's half-close has been propagated, and *after* every byte it sent was across. What is left
    /// is the worker writing those bytes upstream, which is ordinary work rather than a teardown to cut
    /// short.
    Halted,
    /// There is nowhere left to put the client's bytes: the worker went, its flow was detached, or the
    /// reserved tail could not take the ending.
    Gone,
}

/// The owner's half of one flow's bridge, and what it has already learned about the flow's two directions.
///
/// Every field below a latch rather than bookkeeping. Each turns a repeatable observation into a one-time
/// action, which is what keeps a crossing that finds nothing new from reporting progress and spinning the
/// owner that called it.
pub struct Bridge {
    /// Owner to worker: the client's payload on its way upstream.
    down: WriteHalf<SimplexStream>,
    /// Worker to owner: the upstream's payload on its way to the client.
    up: ReadHalf<SimplexStream>,
    /// The reserved one-way tail, empty until the client's ending is extracted into it.
    ///
    /// Held back rather than used, because that is what makes the extraction independent of the worker: it is
    /// the client-side receive buffer's own capacity, so whatever is in that buffer when the client's FIN
    /// arrives fits in one go. Writing steady-state traffic here would give that away.
    tail: WriteHalf<SimplexStream>,
    /// Set once the worker's half has reported the end of its stream, which is strictly after every byte it
    /// wrote. Read to close the client's send side exactly once.
    finished: bool,
    upstream: Upstream,
    /// Set once the client's handshake has completed.
    ///
    /// Load-bearing, not bookkeeping. Every "is this side done" question below is asked with `may_recv`,
    /// which is false for a socket that is merely *listening* - so without this the first crossing after
    /// opening a flow reads a brand-new connection as a half-closed one.
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
    ///
    /// The one question a client-side socket reaching `Closed` has to ask before it cancels the worker.
    /// `true` means the worker is still doing ordinary work - writing bytes this daemon already acknowledged
    /// to the client - so its own completion is what ends the flow, bounded by a retirement, an idle expiry,
    /// a sweep and session shutdown, all of which stay abortive. `false` covers every other way a socket
    /// reaches `Closed`: a reset the stack accepted, a flow that never opened, a worker already gone.
    pub fn halted(&self) -> bool {
        matches!(self.upstream, Upstream::Halted)
    }

    /// Stops draining the client's receive buffer for a flow whose worker has gone.
    ///
    /// The detach path's, and it is not a close: it only stops the owner asking a bridge whose reader has
    /// gone. Deliberately not [Upstream::Halted] - nothing is flushing upstream any more, so nothing is owed
    /// a wait.
    pub fn stop_sending(&mut self) {
        self.upstream = Upstream::Gone;
    }

    /// Where this flow stands in the clean client-ending lifecycle, read from this bridge and the phase its
    /// client-side socket is in together.
    ///
    /// Derived rather than remembered, and that is the whole reason it is a function. The two facts move at
    /// different moments even within one call: the client's FIN reaches the *stack* when the packet is
    /// pushed, and this bridge only when the ending is extracted a few steps later - so at the moment the
    /// idle floor is armed the phase says the client has finished and this bridge still says `Open`. A flag
    /// beside them would be a third thing that could disagree with either.
    ///
    /// A reset does not appear here at all: an accepted reset is settled by the owner that saw `smoltcp`
    /// accept it, which cancels the flow there and then. Carrying it as a latch here meant classifying a
    /// packet this daemon had not validated - see [crate::shared::ingress]. `established` is required for
    /// [Ending::Pending] because a flow that never opened has no clean ending to be part-way through.
    pub fn ending(&self, state: State) -> Ending {
        match self.upstream {
            // Nowhere left to put the client's bytes, so there is no flush to protect and nothing to wait
            // for. A detached flow and one whose worker went are both this.
            Upstream::Gone => Ending::Ordinary,
            Upstream::Halted => Ending::Flushing,
            Upstream::Open if self.established && peer_finished(state) => Ending::Pending,
            Upstream::Open => Ending::Ordinary,
        }
    }

    /// The Closed-socket check: what a client-side socket in this phase does to the worker still attached to
    /// this bridge, and the cancellation that follows from it.
    ///
    /// The whole decision, side effect included, so that an owner walking its table cannot express half of
    /// it. What lives here rather than at that call site is the exception, because the exception is the part
    /// that loses data when it goes missing: a flow whose client half-closed *cleanly* may still have a
    /// worker writing bytes this daemon acknowledged, and cancelling is abortive. Only a fully
    /// [Upstream::Halted] flow is that exception.
    ///
    /// Takes the phase rather than a "closed" flag so that which phase is terminal is this module's answer
    /// too, and so that a caller cannot get the question wrong on the way in.
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
    ///
    /// The order is the correctness property, and two of the four steps were once in the wrong place:
    /// upstream to client first, because a full stack send buffer is what throttles the remote; then client
    /// to upstream, as much as the main stream will take; then the client's ending, if the ingress that
    /// carried its FIN has not already taken it; and this owner's own FIN held back until that has run, so a
    /// `CLOSE-WAIT` socket can never reach `LAST-ACK` with the client's bytes still unread inside it. The
    /// phase is read once, from the socket, so no caller can hold a second opinion about it.
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
        //
        // `may_send` is true in exactly `ESTABLISHED` and `CLOSE-WAIT`. In the first the client has not
        // finished, so this goes out at once, which keeps the two half-closes independent for the protocols
        // that need an upstream EOF before their own. In the second the client *has* finished and this is
        // the guard: emitting with bytes still unread took a socket to `LAST-ACK` and then `Closed` holding
        // payload nothing could read again. `can_recv` is asked of the *stack*, so the FIN cannot overtake a
        // byte the client sent whatever else went wrong. Idempotent by the phase: closing leaves a state
        // whose `may_send` is false.
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
        // Defensive, and normally a no-op. Production takes a client's ending on the ingress that carried
        // its FIN: the device holds one packet, that packet names exactly one flow, and
        // [crate::shared::ingress::accept] settles and seals that flow before returning - so by the time a
        // traffic pass reaches it, [Bridge::seal] answers [Sealed::NotDue]. Nothing else can produce a peer
        // FIN: a timer and a retirement move this owner's own side, never the client's. This arm is the
        // mechanism kept honest against a caller that reaches the bridge some other way, not a lifecycle
        // production relies on.
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
    ///
    /// The entry point the **packet owner** calls on the exact flow a segment named, before that call
    /// returns - see [crate::shared::ingress] for why a later owner turn is too late. [Bridge::cross] asks
    /// the same question defensively and normally gets [Sealed::NotDue], because the ingress that carried
    /// the FIN has already sealed that flow.
    ///
    /// Nothing here registers a waker: the extraction cannot answer `Pending` for a reason worth waiting on,
    /// so a registration would only wake the owner for a flow it has nothing left to do for.
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
    ///
    /// # Why it cannot be spread over several
    ///
    /// The owner polls the stack between owner turns - the traffic path does it after any pass that moved
    /// something, and so do the packet, timer, terminal and retirement paths. A `TIME-WAIT` socket ten
    /// seconds old clears its whole receive buffer inside any of them (`smoltcp-0.13.1 src/socket/tcp.rs`
    /// `:888-897`, reached from `:2440-2444`; `set_timeout` does not govern that timer). A half-done
    /// extraction is therefore not a state to resume - it is acknowledged bytes waiting to be discarded.
    ///
    /// # Why one is enough
    ///
    /// Every bound below is the library's, not a guess of this daemon's. The tail is **empty**, because
    /// nothing else writes to it, and its capacity is the receive buffer's own - see [TailCapacity] - so the
    /// room is there. `WriteHalf<SimplexStream>::poll_write` takes a *blocking* `std::sync::Mutex` rather
    /// than a poll-based lock (`tokio 1.53.1`, `io/split.rs`), so a worker reading the tail on another
    /// thread cannot make it answer `Pending`; underneath, `SimplexStream` answers `Pending` in exactly one
    /// case, `max_buf_size - buffer.len() == 0` (`io/util/mem.rs`); and `poll_shutdown` is `close_write` and
    /// `Ready(Ok(()))` unconditionally. A `smoltcp` receive ring is two contiguous runs at most.
    ///
    /// The one remaining yield is Tokio's cooperative budget - `poll_proceed` answers `Pending` once the
    /// *task* has spent its 128 operations, whatever the pipe's state - and the owner scans every live flow
    /// in one task poll, so a flow reached late in a busy pass hit it routinely.
    /// [tokio::task::unconstrained] removes that and nothing else, for one poll of one shutdown, at most two
    /// writes and one shutdown: a fixed, self-terminating step rather than a loop that could starve the
    /// runtime.
    ///
    /// So a `Pending` from the tail can only mean it was built smaller than the receive buffer. That is a
    /// construction error, answered as [Sealed::Broken] so the owner can fence the socket, cancel the worker
    /// and report it, rather than a clean close over a truncated stream.
    ///
    /// # Both pipes close, whatever happened
    ///
    /// On every path out - whole, reader gone, no room, a shutdown that refused - both are closed before
    /// returning, because a worker reading `down.chain(tail)` needs the end of *each* to reach the end of
    /// its stream. A prefix written into a pipe nobody closed is a worker parked for ever on a descriptor
    /// and an admission slot. The closes are idempotent, which is what makes repeating them free. The
    /// downward pipe goes **first**, so no later byte can overtake one already in the tail.
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
///
/// A `simplex` half does **not** signal its peer on drop - only `tokio::io::duplex` does, and naming the
/// three directions outright gave that up along with the extra outer split. Nothing production does depends
/// on this: the clean paths shut both halves explicitly above, and every reclaim cancels and joins the
/// worker before dropping this. It is the belt to that brace, and each call is one library operation that
/// cannot fail.
impl Drop for Bridge {
    fn drop(&mut self) {
        self.close_both();
    }
}

/// Closes one write half. Answers whether it is closed, which for an in-memory pipe is always.
///
/// No cooperative check: `SimplexStream::poll_shutdown` is `close_write` and `Ready(Ok(()))`.
fn shut(half: &mut (impl AsyncWrite + Unpin), cx: &mut Context<'_>) -> bool {
    matches!(Pin::new(half).poll_shutdown(cx), Poll::Ready(Ok(())))
}

/// Reads the bridge straight into the client's send buffer, taking exactly what fits and nothing more.
///
/// The caller has already established that there is room, which is what makes a zero-byte answer
/// unambiguously the end of the stream rather than an empty destination.
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
///
/// A full bridge consumes nothing at all - which is the backpressure: the receive buffer stays occupied and
/// the window the stack advertises closes on its own.
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

    /// A waker that only counts, so a test can say whether *the bridge* woke the owner rather than whether
    /// something else in the process happened to.
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

    /// One real terminated flow: two `smoltcp` sockets connected to each other over a real loopback, and the
    /// production bridge across the one the engine owns.
    ///
    /// The client half is a `smoltcp` socket rather than a model of one, so every window, buffer bound and
    /// state transition below is the stack's own. The worker half is the far end of the same
    /// `simplex` pipes production hands its tasks.
    struct Wired {
        interface: Interface,
        device: Loopback,
        sockets: SocketSet<'static>,
        /// The socket the bridge crosses with - the client-facing half of a terminated flow.
        engine: SocketHandle,
        /// The peer, standing where a tethered client's TCP stack stands.
        client: SocketHandle,
        bridge: Bridge,
        /// The worker's half. `None` once a test has handed it to a real task - or once it has dropped it,
        /// which is what a worker's own completion does.
        worker: Option<Worker>,
        /// Everything the client's application has read, drained on every turn of the owner's loop. Draining
        /// is what a client does and what keeps its window open, so a small receive buffer stays a partial
        /// *crossing* rather than a stalled connection.
        received: Vec<u8>,
        /// This flow's worker token, exactly as the engine holds one. Real rather than a flag, because what
        /// the Closed-socket check does with it is the thing under test.
        cancel: CancellationToken,
        woken: Arc<Counting>,
        waker: Waker,
        millis: i64,
    }

    impl Wired {
        /// `stack` is each socket buffer and `bridge` each direction of the main byte bridge. The reserved
        /// tail is built at `stack`, which is production's own rule: it is sized to the client-side receive
        /// buffer's capacity so an extraction can never be short.
        ///
        /// Returns an established connection: the handshake below is the stack's own, run to completion over
        /// the loopback.
        fn new(stack: usize, bridge: usize) -> Self {
            Self::with_tail(stack, bridge, stack)
        }

        /// The same, with the tail sized independently.
        ///
        /// Production cannot do this at all - [TailCapacity::of] asks the socket - and one test below does it
        /// on purpose, because a tail too small for an ending is the branch [Bridge::extract] must refuse
        /// rather than truncate.
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

        /// Moves the stack's own clock on, for a test that has to reach one of smoltcp's fixed timers - the
        /// ten-second close timer in particular, which no setting of ours governs.
        fn advance(&mut self, millis: i64) {
            self.millis += millis;
        }

        fn socket(&mut self, handle: SocketHandle) -> &mut Socket<'static> {
            self.sockets.get_mut::<Socket>(handle)
        }

        fn state(&self, handle: SocketHandle) -> State {
            self.sockets.get::<Socket>(handle).state()
        }

        /// One crossing, exactly as the engine makes it: the phase decides whether the flow has opened, and
        /// the owner's own waker is what gets registered.
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

        /// Advances the stack and runs the Closed-socket check, exactly where `Engine::poll` runs it.
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

        /// Runs the **stack alone** until `done` holds: poll, the Closed-socket check, and never a crossing.
        ///
        /// A way to put the connection into a particular phase before asking what one crossing does to it -
        /// nothing more. It models no owner turn: production takes a client's ending on the ingress that
        /// carried its FIN, before that call returns, which is [crate::shared::ingress]'s to sequence and to
        /// prove.
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

        /// One **traffic pass**: run the stack, give this flow its crossing, run the stack for what that
        /// moved.
        ///
        /// `Engine::traffic`'s shape and only that. Idle floors are not modelled here - arming one is part of
        /// the packet boundary's own order, which [crate::shared::ingress] owns and proves.
        fn turn(&mut self) -> (bool, Crossing) {
            let progressed = self.poll_stack();
            let crossing = self.cross();
            self.poll_stack();
            (progressed, crossing)
        }

        /// The owner's loop: advance the stack, give the flow its turn, and repeat until neither has anything
        /// left to do at any instant the stack is waiting for. Answers what crossed in total.
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
                // Nothing is due at this instant. Jump to whatever the stack is waiting for - a delayed
                // acknowledgement, a retransmission - and stop once it is waiting for nothing.
                match self.interface.poll_delay(self.now(), &self.sockets) {
                    // Clamped below so a zero delay cannot spin, and above so one socket's ten-second close
                    // timer does not swallow the whole budget.
                    Some(delay) => self.millis += (delay.total_millis() as i64).clamp(1, 50),
                    None => break,
                }
            }
            (to_client, to_upstream)
        }

        /// The worker's half, for a test driving it by hand rather than through a task.
        fn worker(&mut self) -> &mut Worker {
            self.worker.as_mut().expect("the worker's half is here")
        }

        /// Hands the worker's half to a real task, which is what production does. Everything after this
        /// drives the owner's side only.
        fn hand_to_worker(&mut self) -> Worker {
            self.worker.take().expect("handed over once")
        }

        /// The worker's task running to completion: it shuts its write half down and then goes.
        ///
        /// Both steps, because a `simplex` half does not signal its peer on drop the way `duplex` did - and
        /// production does both too, since `copy_bidirectional` shuts a direction down when its reader
        /// reaches the end of the stream and only then returns.
        fn worker_completes(&mut self) {
            self.worker_ends_writing();
            drop(self.hand_to_worker());
        }

        /// The upstream ending: the worker shuts down its write half and goes on reading, which is what
        /// `copy_bidirectional` does when its upstream reaches the end of its stream.
        fn worker_ends_writing(&mut self) {
            match Pin::new(self.worker()).poll_shutdown(&mut noop()) {
                Poll::Ready(Ok(())) => {}
                other => panic!("an in-memory write half shuts down at once: {other:?}"),
            }
        }

        /// What the worker half can write right now, and how much of it the bridge took. `None` is a bridge
        /// with no room, which is the backpressure.
        fn worker_writes(&mut self, bytes: &[u8]) -> Option<usize> {
            match Pin::new(self.worker()).poll_write(&mut noop(), bytes) {
                Poll::Ready(Ok(taken)) => Some(taken),
                Poll::Ready(Err(e)) => panic!("the worker's write failed: {e}"),
                Poll::Pending => None,
            }
        }

        /// What the worker half can read right now. `Some(0)` is the end of the stream.
        fn worker_reads(&mut self, into: &mut [u8]) -> Option<usize> {
            let mut buffer = ReadBuf::new(into);
            match Pin::new(self.worker()).poll_read(&mut noop(), &mut buffer) {
                Poll::Ready(Ok(())) => Some(buffer.filled().len()),
                Poll::Ready(Err(e)) => panic!("the worker's read failed: {e}"),
                Poll::Pending => None,
            }
        }

        /// One crossing made from inside a real task, so the waker registered is that task's own - which is
        /// what production does.
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

        /// The same traffic pass under a real runtime, so a worker running as a real task gets to run in
        /// between.
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

        /// The client's application reading everything its stack has for it, which is also what reopens the
        /// window a small receive buffer closes.
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

        /// Everything the client's application has read so far.
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

    /// A context for the *worker's* side of a poll, which no test asserts on: what the assertions are about
    /// is which of the owner's wakes the bridge registered.
    fn noop() -> Context<'static> {
        Context::from_waker(Waker::noop())
    }

    fn pattern(length: usize) -> Vec<u8> {
        (0..length).map(|byte| byte as u8).collect()
    }

    #[test]
    fn data_arriving_in_the_bridge_is_what_wakes_the_owner() {
        let mut wired = Wired::new(4096, 4096);
        // Nothing to move, and the crossing is what registers the owner with the bridge it is waiting on.
        assert!(!wired.cross().moved);
        let before = wired.wakes();
        // The worker writing one byte is the whole of the event: no packet, no timer, no marker.
        assert_eq!(wired.worker_writes(b"x"), Some(1));
        assert_eq!(wired.wakes(), before + 1);
        assert_eq!(wired.cross().to_client, 1);
        wired.run();
        assert_eq!(wired.delivered(), b"x");
    }

    #[test]
    fn capacity_returning_in_the_bridge_is_what_wakes_the_owner() {
        // A bridge far smaller than the client's stack buffers, so the client can fill it and still have
        // bytes waiting - which is the state a full bridge has to be woken out of.
        let mut wired = Wired::new(4096, 64);
        let sent = pattern(4096);
        let client = wired.client;
        let queued = wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        assert!(queued > 64, "the client has more than the bridge can hold");
        wired.run();
        // The bridge is full and the receive buffer still holds the rest: the owner consumed nothing more,
        // which is what closes the client's window rather than dropping a byte.
        let engine = wired.engine;
        assert!(wired.socket(engine).recv_queue() > 0);
        assert!(!wired.cross().moved);
        let before = wired.wakes();
        // The worker taking one byte out is the whole of the event.
        let mut taken = [0u8; 1];
        assert_eq!(wired.worker_reads(&mut taken), Some(1));
        assert_eq!(wired.wakes(), before + 1);
        assert_eq!(wired.cross().to_upstream, 1);
    }

    #[test]
    fn the_bridge_bounds_the_read_ahead_and_nothing_else_does() {
        let mut wired = Wired::new(64, BRIDGE_BUFFER);
        // The worker fills its whole charged read-ahead with the owner never crossing once: no
        // acknowledgment, no rendezvous per segment, and nothing of the client's involved.
        let mut written = 0;
        while let Some(taken) = wired.worker_writes(&pattern(4096)) {
            written += taken;
        }
        assert_eq!(written, BRIDGE_BUFFER, "exactly the capacity, and no more");
        // And the next byte waits, which is what stops the upstream half reading and closes the remote's
        // window.
        assert_eq!(wired.worker_writes(b"x"), None);
    }

    #[test]
    fn a_download_arrives_whole_and_in_order_however_small_the_stack_buffers_are() {
        // Stack buffers far smaller than one write, so every byte crosses in pieces the send buffer has room
        // for and the rest stays in the bridge until it does.
        let mut wired = Wired::new(1024, 16 * 1024);
        let sent = pattern(64 * 1024);
        let mut offered = 0;
        while offered < sent.len() {
            match wired.worker_writes(&sent[offered..]) {
                Some(taken) => offered += taken,
                // The bridge is full: the owner's turn is what makes room, and there is no signal of ours in
                // between.
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
        // One byte of stack send buffer, so a crossing can only ever take one byte at a time and everything
        // else has to stay in the bridge rather than in a remainder somebody holds.
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"abc"), Some(3));
        let first = wired.cross();
        assert_eq!(
            first.to_client, 1,
            "exactly what the send buffer had room for"
        );
        // The rest is still the bridge's, and arrives as the client acknowledges.
        wired.run();
        assert_eq!(wired.delivered(), b"abc");
    }

    #[test]
    fn the_worker_finishing_closes_the_client_after_every_byte_it_wrote() {
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"answer"), Some(6));
        // The worker's task completes, which is the clean completion an engine detaches on: what it wrote
        // stays readable and the end of the stream follows it.
        wired.worker_completes();
        wired.run();
        assert_eq!(wired.delivered(), b"answer");
        assert!(wired.bridge.finished());
        // And only then is the client's send side closed, so the FIN follows the payload rather than
        // replacing it.
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
        // The client finishes asking and closes its own send side, exactly as a request-then-response
        // protocol does.
        wired.socket(client).close();
        wired.run();
        let mut scratch = vec![0u8; 64];
        assert_eq!(wired.worker_reads(&mut scratch), Some(7));
        assert_eq!(&scratch[..7], b"request");
        // The half-close reaches the worker as the end of its own stream, strictly after the request.
        assert_eq!(wired.worker_reads(&mut scratch), Some(0));
        assert!(!wired.bridge.sending());
        // And the response still crosses, because only one direction ended.
        assert_eq!(wired.worker_writes(b"response"), Some(8));
        wired.run();
        assert_eq!(wired.delivered(), b"response");
    }

    #[test]
    fn an_abortive_ending_discards_what_the_bridge_was_holding() {
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"undelivered"), Some(11));
        // A retirement, an idle expiry or a failed upstream: the flow is reclaimed and its bridge with it.
        let (owner, _worker) = super::bridge(1, TailCapacity::undersized(1));
        drop(std::mem::replace(&mut wired.bridge, owner));
        // Nothing of it reached the client, which is what a reset means - and the worker is left able to
        // finish rather than waiting on pipes nobody will ever close. Both of this owner's write halves go
        // with it, which is why the chain reaches its end at all: a `simplex` half does not signal its peer
        // on drop, so the reclaim has to say so itself.
        wired.run();
        assert_eq!(wired.delivered(), b"");
        assert_eq!(
            wired.worker_reads(&mut [0u8; 8]),
            Some(0),
            "the worker's whole read side ended, so its copy can return"
        );
    }

    #[test]
    fn a_detached_flow_stops_the_owner_draining_for_it() {
        let mut wired = Wired::new(4096, 64);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&pattern(4096))
            .expect("an established socket may send");
        wired.run();
        // The worker's terminal reached the owner and the flow was detached: nothing reads the downward pipe
        // any more, so the owner is told to stop draining the receive buffer for it. That is the production
        // path - the engine calls this from `Engine::close` - rather than something inferred from a pipe.
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
        // An idle flow: nothing in the bridge, nothing in the receive buffer. Repeated crossings report no
        // progress at all, which is what lets the owner wait instead of spinning.
        for _ in 0..8 {
            assert_eq!(wired.cross(), Crossing::default());
        }
        // And the same while the client's send buffer is what is full: the bridge holds bytes the stack
        // cannot take, and what will free it is the client's own acknowledgement rather than a wake of ours.
        let mut wired = Wired::new(1, 64);
        assert_eq!(wired.worker_writes(b"ab"), Some(2));
        assert!(wired.cross().moved, "one byte fits");
        let quiet = wired.wakes();
        assert!(!wired.cross().moved, "and the send buffer is full again");
        assert_eq!(wired.wakes(), quiet, "nothing of the bridge's was woken");
    }

    #[test]
    fn the_worker_sees_main_bytes_then_tail_bytes_then_one_end_of_stream() {
        // A main stream far smaller than what the client sends, so the ending has to be split across both
        // pipes: some bytes reach the worker the ordinary way and the rest come out of the reserved tail.
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
        // The ending is out of the stack whatever the main stream could take.
        assert!(wired.bridge.halted());
        let engine = wired.engine;
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "nothing of the client's is left inside smoltcp"
        );

        // And the worker reads one ordered stream: main bytes, then tail bytes, then exactly one end.
        let mut received = Vec::new();
        let mut scratch = vec![0u8; 8];
        loop {
            match wired.worker_reads(&mut scratch) {
                Some(0) => break,
                Some(read) => received.extend_from_slice(&scratch[..read]),
                None => panic!("a sealed ending never blocks its reader"),
            }
        }
        assert_eq!(received, sent, "every byte, in order, across both pipes");
        assert_eq!(
            wired.worker_reads(&mut scratch),
            Some(0),
            "and the end of the stream stays the end"
        );
    }

    #[test]
    fn a_full_main_bridge_and_a_client_ending_together_lose_nothing() {
        // Regression for the first reported loss: client payload and FIN put the socket in `CLOSE-WAIT`
        // with unread bytes, the upstream then ended, and this owner's own FIN went out anyway - taking the
        // socket to `LAST-ACK` and then `Closed` with the client's acknowledged bytes still inside it, where
        // the Closed-socket check cancelled the worker and they were gone.
        let mut wired = Wired::new(4096, 8);
        let sent = pattern(64);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        wired.run();
        // The upstream ends too, which is what used to emit the local FIN too early.
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
        // The guard, one crossing at a time. A main stream far too small for what the client sent, so its
        // payload is still inside smoltcp when its FIN and the upstream's EOF have both been seen.
        let mut wired = Wired::new(4096, 8);
        let sent = pattern(64);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        // The upstream ends too, which is what used to emit the local FIN while those bytes were unread.
        wired.worker_ends_writing();
        // Deliver the client's payload and FIN to the stack without crossing, so the next crossing is the
        // first one that sees both endings.
        assert!(wired.packets_until(|w| peer_finished(w.state(w.engine))));
        let engine = wired.engine;
        assert_eq!(wired.state(engine), State::CloseWait);
        assert!(
            wired.socket(engine).recv_queue() > 0,
            "and its bytes are here"
        );

        // One crossing. It learns the upstream ended, withholds the FIN because the receive buffer still has
        // the client's bytes, and extracts them. Removing that guard sends the FIN in this very crossing and
        // takes the socket to `LAST-ACK` holding payload nothing will ever read.
        let crossed = wired.cross();
        assert!(crossed.moved);
        assert_eq!(
            wired.state(wired.engine),
            State::CloseWait,
            "the local FIN waited for the extraction"
        );
        assert!(wired.bridge.halted(), "which happened in the same crossing");
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "and left nothing of the client's in the stack"
        );

        // The next crossing owes nothing, so the FIN goes out and the teardown runs to completion.
        wired.run();
        assert_eq!(wired.state(wired.engine), State::Closed);
        assert!(
            !wired.cancel.is_cancelled(),
            "a clean ending is never cancelled"
        );
    }

    #[tokio::test]
    async fn a_depleted_cooperative_budget_does_not_split_the_extraction() {
        // Tokio's in-memory pipes are cooperative: `SimplexStream::poll_write` runs `poll_proceed` first, so
        // once this task has spent its 128-operation budget a write answers `Pending` even though the tail
        // has room. An owner scans every live flow in one task poll, so a flow reached late in a busy pass
        // hits exactly that. It must not split the extraction, because the owner polls the stack between
        // turns and a `TIME-WAIT` socket clears its receive buffer there.
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

        // Spend the budget inside *this* task before the crossing, exactly as a busy pass would. Each write
        // into a scratch pipe is one cooperative operation.
        let (_drain, mut spender) = simplex(4096);
        for _ in 0..160 {
            let _ =
                std::future::poll_fn(|cx| Poll::Ready(Pin::new(&mut spender).poll_write(cx, b"x")))
                    .await;
        }

        // One crossing, with the budget gone.
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

        // Main bytes, then tail bytes, then exactly one end of stream. Read after a real yield, because the
        // budget this test deliberately spent is still spent - a *reader* is subject to the same cooperative
        // check, and in production it is a different task with a budget of its own.
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
        assert_eq!(received, sent, "every byte, in order");
        assert_eq!(wired.worker_reads(&mut scratch), Some(0), "and one EOF");
    }

    #[test]
    fn a_wrapped_receive_ring_drains_both_of_its_contiguous_runs() {
        // `smoltcp`'s receive buffer is a ring, and one `recv` hands out only its largest *contiguous* run.
        // Once the head has advanced, a later burst wraps and the client's ending sits in two runs - so an
        // extraction that wrote once would leave the second run behind, for the close timer to clear, while
        // reporting the ending as finished.
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

        // Move the head on by forty, eight at a time, which is all the main stream holds.
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
        assert_eq!(received.len(), 40, "the head advanced by forty");
        assert_eq!(wired.socket(engine).recv_queue(), 8, "eight left behind it");
        // Let the acknowledgements land, so the client's own send buffer is free for the next burst.
        assert!(wired.packets_until(|w| w.sockets.get::<Socket>(w.client).send_queue() == 0));

        // Now a burst that wraps past the end of the ring, and the client's ending behind it.
        let second: Vec<u8> = (0..32u16).map(|byte| (128 + byte) as u8).collect();
        assert_eq!(
            wired
                .socket(client)
                .send_slice(&second)
                .expect("an established socket may send"),
            32
        );
        wired.socket(client).close();
        // The stack alone, so this asks what *one* extraction does with a wrapped ring rather than what any
        // owner does with a packet. Production seals on the ingress that carries the FIN - see
        // [crate::shared::ingress] - and this is the mechanism that ingress calls into.
        assert!(wired.packets_until(|w| peer_finished(w.state(w.engine))));
        assert_eq!(
            wired.socket(engine).recv_queue(),
            40,
            "forty bytes, across both of the ring's runs"
        );

        // Exactly one extraction. The tail is the receive buffer's own capacity, so one is all an ending
        // should ever need, whichever caller asks for it.
        let crossing = wired.cross();
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "both contiguous runs drained, not just the first"
        );
        assert_eq!(crossing.to_upstream, 40, "and all forty crossed at once");
        assert!(wired.bridge.halted(), "so the ending finished in that pass");

        // Everything the client sent, in order, then one end of stream.
        loop {
            match wired.worker_reads(&mut scratch) {
                Some(0) => break,
                Some(read) => received.extend_from_slice(&scratch[..read]),
                None => panic!("a finished ending never blocks its reader"),
            }
        }
        let mut expected = first.clone();
        expected.extend_from_slice(&second);
        assert_eq!(received, expected, "every byte across the wrap, in order");
    }

    #[test]
    fn a_client_that_resets_records_no_clean_half_close_and_takes_its_worker_with_it() {
        // A tiny main stream, so the client's payload is still in the stack when it resets - which is the
        // companion to the clean cases: the same shape, reaching the opposite classification.
        let mut wired = Wired::new(4096, 8);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&pattern(64))
            .expect("an established socket may send");
        wired.run();
        // A reset rather than a FIN, before any clean ending could be classified. Its socket goes straight to
        // `Closed`, which is the phase `opened` answers `false` for - so nothing here is a clean half-close,
        // nothing is extracted into the tail, and what the client sent may be discarded.
        wired.socket(client).abort();
        wired.run();
        assert_eq!(wired.state(wired.engine), State::Closed);
        assert!(
            !wired.bridge.halted(),
            "a reset is owed no flush and no wait"
        );
        // And the Closed-socket check cancels, which is what every abortive ending needs it to do.
        assert!(
            wired.cancel.is_cancelled(),
            "a socket the stack has finished with takes its worker with it"
        );
    }

    #[test]
    fn the_extraction_holds_its_order_against_a_worker_that_is_not_reading() {
        // Main bytes and tail bytes are two pipes, and only the worker's `chain` puts them back in order. A
        // worker that is not reading is what makes the distinction visible: the main stream fills and stays
        // full, so everything after it goes to the tail - and all of it has to survive `smoltcp`'s own
        // ten-second close timer, whose expiry clears the receive buffer.
        let sent = pattern(200);
        let mut wired = Wired::new(1024, 8);
        let client = wired.client;
        wired
            .socket(client)
            .send_slice(&sent)
            .expect("an established socket may send");
        wired.socket(client).close();
        wired.run();

        // Nothing has read a byte, and the ending is already out of the stack.
        let engine = wired.engine;
        assert!(wired.bridge.halted());
        assert_eq!(
            wired.socket(engine).recv_queue(),
            0,
            "the extraction does not wait for the worker"
        );

        // Well past the close timer, still with nothing read.
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
        assert_eq!(received, sent, "every byte the client sent, in one order");
        assert_eq!(
            wired.worker_reads(&mut scratch),
            Some(0),
            "one end of stream, after both pipes"
        );
    }

    #[test]
    fn a_tail_too_small_for_the_ending_is_refused_rather_than_truncated() {
        // The construction invariant, from the only side that can fail it. Production asks the socket for the
        // capacity - see [TailCapacity] - so this cannot happen there; if it ever did, the choice is between
        // an abortive ending and a clean close that silently dropped acknowledged bytes.
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

        // The part that keeps the worker from being parked for ever: *both* pipes reached a finite end, so
        // `down.chain(tail)` terminates. A prefix written into a tail nobody closed would leave that worker
        // holding an upstream descriptor and an admission slot with nothing left to wake it.
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
        // Dropping a `simplex` half signals *nothing* - only `tokio::io::duplex` does that - so the end of
        // each pipe is something this owner has to say explicitly. It says it in three places: after a whole
        // extraction, after a refused one, and here, when its half of the bridge goes at all. A worker reads
        // `down.chain(tail)`, so one pipe left open is a worker parked for ever on a stream that will never
        // end, holding an upstream descriptor and an admission slot.
        let (bridge, mut worker) = super::bridge(8, TailCapacity::undersized(8));
        drop(bridge);
        let mut scratch = [0u8; 8];
        let mut buffer = ReadBuf::new(&mut scratch);
        assert!(matches!(
            Pin::new(&mut worker).poll_read(&mut noop(), &mut buffer),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(buffer.filled().len(), 0, "both pipes, in one end of stream");
    }

    #[test]
    fn a_reader_that_merely_went_away_is_not_an_invariant_failure() {
        // The arm that separates an ordinary disappearance from a broken invariant, asked of the type rather
        // than of a connection - because with three `simplex` pipes the library cannot currently produce it.
        // Dropping `ReadHalf` sets no flag, so a write into a pipe whose reader has gone still succeeds; only
        // a `close_read` this daemon never performs would report `BrokenPipe`. The arm stays because the
        // distinction is the point: a reader disappearing races its own task terminal and must never be
        // counted, fenced or reported as the terminal-tail invariant failing.
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
        // The client finishes asking, which the crossing propagates as a clean half-close.
        wired.socket(client).close();
        wired.run();
        assert!(wired.bridge.halted());
        // Then the upstream ends too, so this owner closes its own side and the teardown runs to `Closed`.
        wired.worker_completes();
        wired.run();
        assert_eq!(wired.state(wired.engine), State::Closed);
        // The whole point: the stack has finished with the socket, and the worker is *not* cancelled. In
        // production it is still writing bytes this daemon acknowledged, and cancelling is abortive.
        assert!(
            !wired.cancel.is_cancelled(),
            "a clean flush is not something a Closed socket may cut short"
        );
        // Nor is the bound this flow carries taken away or made immediately due, either of which is the same
        // cancellation one turn later. The deadline itself is armed on the packet boundary - see
        // [crate::shared::ingress] - so what this asks is the decision that boundary reads.
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
        // The decision on its own, with real tokens and no stack at all, so each arm is pinned rather than
        // inferred from a connection that happened to reach it.
        let (bridge, _worker) = super::bridge(8, TailCapacity::undersized(8));
        let cancel = CancellationToken::new();
        // A phase the stack has not finished with decides nothing, whatever else is true.
        for state in [State::Established, State::CloseWait, State::TimeWait] {
            assert_eq!(bridge.teardown(state, &cancel), Teardown::Live);
            assert!(!cancel.is_cancelled(), "{state:?}");
        }
        // `Closed` on a flow that recorded no clean half-close is abortive.
        assert_eq!(bridge.teardown(State::Closed, &cancel), Teardown::Cancelled);
        assert!(cancel.is_cancelled());

        // And `Closed` on one that did records nothing and cancels nothing, however often it is asked.
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

    /// The race the owner's closed-socket sweep has to survive, driven through the production copy.
    ///
    /// Upstream EOF, a client upload the worker has taken but not yet written on, a client FIN, and then
    /// `smoltcp` reaching `Closed` while those bytes are still in flight. An owner that cancelled on `Closed`
    /// alone would drop them - they are in the worker's scratch, past the bridge and short of the upstream -
    /// and they are bytes this daemon already acknowledged to the client.
    #[tokio::test]
    async fn a_clean_close_drains_every_byte_the_client_sent_before_the_flow_ends() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let mut wired = Wired::new(4096, 4096);
            // One byte of upstream, so the copy takes the request off the bridge and then blocks with it in
            // hand. That is the window: past the bridge, short of the remote, and invisible to the stack.
            let (upstream, mut remote) = duplex(1);
            let worker = wired.hand_to_worker();
            let copy = tokio::spawn(async move {
                let (mut upstream, mut worker) = (upstream, worker);
                copy_bidirectional_with_sizes(&mut upstream, &mut worker, 64, 64).await
            });

            // The client sends its request and finishes asking.
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

            // The upstream answers and ends, which closes the client's side too.
            remote
                .write_all(b"response")
                .await
                .expect("the upstream answers");
            remote.shutdown().await.expect("and then ends");
            while wired.state(wired.engine) != State::Closed {
                wired.step().await;
            }

            // The race, observed: the stack has finished with this socket while the worker is still holding
            // bytes the client sent and this daemon acknowledged. Cancelling here would lose them.
            assert!(!copy.is_finished(), "the worker is still flushing");
            assert!(wired.bridge.halted());
            // And the whole owner lifecycle around it has left the worker alone. Both halves matter and both
            // were wrong once: the Closed-socket check must not cancel, *and* the rearm the final teardown
            // packet triggers must not replace this flow's flush bound with `now` - an immediately due
            // deadline is the same cancellation one turn later.
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

            // The upstream starts reading again. Every byte arrives, and only then does the copy complete -
            // which is the terminal the owner ends the flow on.
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
                assert_ne!(read, 0, "the copy ended before it had flushed");
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

    /// The second reported loss: **the upstream ends first and the main bridge is full**.
    ///
    /// Once this owner's own FIN is out, the client's remaining payload and FIN can put the socket straight
    /// into `TIME-WAIT` with those bytes unread - and smoltcp's close timer is a fixed ten seconds that
    /// `set_timeout` does not govern (`smoltcp-0.13.1 src/socket/tcp.rs:297`), after which `reset()` clears
    /// the receive buffer outright (`:888-897`, reached from `:2440-2444`). Remembering that the ending was
    /// pending cannot preserve those bytes; only getting them out of the stack can.
    #[tokio::test]
    async fn a_full_bridge_survives_the_close_timer_when_the_upstream_ended_first() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            // A main stream of eight bytes against a sixty-four byte ending, and a one-byte upstream so the
            // worker stays backpressured throughout.
            let mut wired = Wired::new(4096, 8);
            let (upstream, mut remote) = duplex(1);
            let worker = wired.hand_to_worker();
            let copy = tokio::spawn(async move {
                let (mut upstream, mut worker) = (upstream, worker);
                copy_bidirectional_with_sizes(&mut upstream, &mut worker, 64, 64).await
            });

            // The upstream ends first, so this owner sends its own FIN while the client is still sending.
            remote.shutdown().await.expect("the upstream ends");
            while !matches!(wired.state(wired.engine), State::FinWait1 | State::FinWait2) {
                wired.step().await;
            }

            // Now the client's payload and FIN, put into the stack directly. Production would have sealed
            // this on the ingress that carried the FIN; what is being isolated here is the *mechanism* that
            // ingress calls - a full main pipe, a wrapped ending and the close timer - with the packet
            // boundary's own ordering proved in [crate::shared::ingress].
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

            // One extraction takes them, whole, with a full main pipe and a worker that is not reading.
            // Nothing here depends on scheduling: what makes the ending safe is that it leaves no half-done
            // state for any later poll to catch, which is why production can take it inside `accept` and why
            // no arm of the ingress loop needs to be biased for it.
            let crossed = wired.cross();
            assert!(crossed.moved);
            assert!(wired.bridge.halted(), "the ending is out of the stack");
            assert_eq!(
                wired.socket(engine).recv_queue(),
                0,
                "before the close timer could ever clear it"
            );

            // Now let the close timer really run. Ten seconds of the stack's own clock, with the upstream
            // still refusing to read, so `reset()` runs on a receive buffer this owner has already emptied.
            wired.advance(11_000);
            while wired.state(wired.engine) != State::Closed {
                wired.step().await;
            }
            assert!(!copy.is_finished(), "the worker is still flushing");
            assert!(
                !wired.cancel.is_cancelled(),
                "and a clean ending is not cancelled by the socket the stack finished with"
            );

            // Only now does the upstream read. Every acknowledged byte is still there, in order.
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
                assert_ne!(read, 0, "the copy ended before it had flushed");
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
        // The second flaw the blanket freeze had. The client finishes asking, the bridge halts, and the
        // upstream then streams a response for longer than the floor that FIN happened to arm. Every
        // delivery is real activity, so the flow must be rearmed by it rather than expiring mid-response.
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
        // A halted `CloseWait` flow is still an ordinary download, so this is what its activity earns.
        assert_eq!(
            wired.bridge.ending(State::CloseWait),
            Ending::Flushing,
            "propagated, and still delivering"
        );
        // So its activity earns a fresh floor rather than the frozen one a halt used to impose. Asked of the
        // decision, because arming it is the packet boundary's - see [crate::shared::ingress].
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
        // And the delivery itself still happens, which is what makes that activity real.
        assert_eq!(wired.worker_writes(b"a long response"), Some(15));
        wired.run();
        assert_eq!(wired.delivered(), b"a long response");
        assert!(!wired.cancel.is_cancelled());
    }
}
