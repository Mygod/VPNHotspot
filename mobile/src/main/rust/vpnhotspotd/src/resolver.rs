//! One resolver transaction on a chosen `Network`, through the platform resolver rather than sockets the
//! daemon owns.
//!
//! The app-UID TestNetwork path's resolver, and only that: [crate::virtual_dns] is the sole caller. Root's
//! DNS proxy keeps its own resolver code in [crate::dns], unchanged, and nothing here is reachable from it.
//!
//! Going through the platform keeps private DNS, caching, and per-network resolver configuration, none of
//! which the daemon could reimplement. What it costs is that the transaction belongs to Android once
//! submitted: cancelling recovers this process's descriptor and not the resolver's work.
//!
//! # Reaching the platform is not the same as being able to watch it
//!
//! `android_res_nsend` is synchronous and irreversible: when it answers with a descriptor, Android is holding
//! one of this UID's resolver slots whatever happens next in this process. The two steps that follow it -
//! making that descriptor nonblocking and registering it with the runtime - are this daemon's own, and either
//! of them failing leaves a slot that is taken and a completion nothing here can ever observe. That is a
//! third outcome, not a variety of failure, so [Submission] names it: see [Submission::Unobservable] and the
//! quarantine its callers put the logical token into.

use std::future::poll_fn;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::task::{Context, Poll};

use libc::c_int;
use tokio::io::unix::AsyncFd;
use tokio::io::Ready;
use vpnhotspotd::shared::dns_wire::MAX_MESSAGE;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::model::Network;

use crate::socket::set_nonblocking;

/// The two steps of a transaction that are this process's own, named so a failure at one of them is reported
/// as itself rather than as one more resolver answer. Not prefixed per mode: this path is shared, and both
/// conversations call the same two functions.
const NONBLOCK: &str = "resolver.nonblock";
const REGISTER: &str = "resolver.register";

/// android/multinetwork.h: `ResNsendFlags::ANDROID_RESOLV_NO_RETRY`.
const ANDROID_RESOLV_NO_RETRY: u32 = 1 << 0;

/// Says out loud that the platform is holding a resolver slot this process can no longer watch.
///
/// One function for both protocols and every owner that can be the last to see such an outcome, because the
/// sentence is about the *platform* rather than about who noticed: a second copy of it is a second wording to
/// keep in step and, since the coalescer keys on the reporting site, a second site for one fact.
///
/// Saying it is not the same as deciding *who* says it, and that decision is per protocol and deliberate. A
/// UDP query owns its own transaction, so its terminal always reports. A DNS-over-TCP transaction can outlive
/// the transport that asked for it, so the rule there is "whoever is last": the transport's own terminal while
/// there is one, and otherwise the owner about to destroy or keep the state - see
/// [crate::tcp::Engine::settle]. Exactly one of those runs for any one outcome, which is why this needs no
/// flag to remember whether it has already been called.
///
/// `#[track_caller]` so the report names the owner that made the call rather than this line, which is what
/// makes a duplicate visible as two sites if one is ever introduced.
#[track_caller]
pub(crate) fn report_unobservable(transaction: u64, failure: &Failure) {
    let Some((context, error)) = failure.reportable() else {
        return;
    };
    crate::report::message_with_details(
        context,
        format!("the platform accepted a DNS query this process can no longer observe: {error}"),
        format!("{:?}", error.kind()),
        [("transaction", transaction)],
    );
}

/// Owns the descriptor `android_res_nsend` returned, so that dropping it before the answer is read
/// cancels the transaction rather than leaking the descriptor.
struct ResolverQuery {
    fd: Option<RawFd>,
}

impl ResolverQuery {
    fn finish(mut self) -> io::Result<Vec<u8>> {
        let mut rcode = 0;
        let mut response = vec![0u8; MAX_MESSAGE];
        // SAFETY: the descriptor is owned here and taken, so nresult closes it exactly once, and the
        // buffer's length is what the reader is told it may write.
        let size = unsafe {
            android_res_nresult(
                self.fd.take().unwrap(),
                &mut rcode,
                response.as_mut_ptr(),
                response.len(),
            )
        };
        if size < 0 {
            Err(io::Error::from_raw_os_error(-size))
        } else {
            response.truncate(size as usize);
            Ok(response)
        }
    }
}

impl AsRawFd for ResolverQuery {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.unwrap()
    }
}

impl Drop for ResolverQuery {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            // SAFETY: fd is owned here and has not been handed to nresult.
            unsafe {
                android_res_cancel(fd);
            }
        }
    }
}

#[link(name = "android")]
unsafe extern "C" {
    fn android_res_nsend(network: u64, msg: *const u8, msglen: usize, flags: u32) -> c_int;
    fn android_res_nresult(fd: c_int, rcode: *mut c_int, answer: *mut u8, anslen: usize) -> c_int;
    fn android_res_cancel(nsend_fd: c_int);
}

/// A transaction the platform has accepted, waiting only to be read.
///
/// Split from the read deliberately, and it is the whole reason this type exists. `android_res_nsend` is
/// synchronous: it hands the query to the platform on the `Network` it was given and returns a descriptor.
/// Awaiting the answer is not. Keeping the two together meant the submission happened wherever the spawned
/// task first got scheduled, so a query classified under one config could reach the resolver *after* its
/// successor had been acknowledged - on a `Network` the session had already stopped claiming. Handing the
/// submission back as a value lets the owner perform it in its own serial order and poll only the wait.
pub(crate) struct Resolving {
    /// Taken at the terminal, which is what hands the descriptor to `android_res_nresult` to read and close.
    /// Absent afterwards, so a second poll cannot read a descriptor this transaction has already given up.
    fd: Option<AsyncFd<ResolverQuery>>,
}

/// What polling one accepted transaction came to.
///
/// Two outcomes, for the same reason [Submission] has three: losing the ability to *watch* a transaction the
/// platform has already accepted is a different ownership from any answer, and an owner handed it as an
/// ordinary failure would refund a logical token for a per-UID slot Android is still holding. This is the
/// same distinction [Submission::Unobservable] draws, reached at the other end of the same transaction.
pub(crate) enum Completed {
    /// The platform's own outcome for this query: its answer, or what it answered instead.
    Answered(Result<Vec<u8>, Failure>),
    /// The readiness registration this transaction was being watched with failed after `android_res_nsend`
    /// had accepted the query. The descriptor is returned - it is dropped with this transaction, which
    /// cancels it - but Android's slot is not, and there is no longer anything here that could observe its
    /// end.
    ///
    /// # How reachable this is, and why owners must not rely on the answer
    ///
    /// Narrowly, from a poll: `Registration::poll_ready` answers `Err` when the runtime's `ScheduledIo` for
    /// this descriptor has been shut down, not for an ordinary `EPOLLERR` - error readiness arrives as bits in
    /// the ready set, which is why the terminal condition above reads them rather than an errno. So on the
    /// pinned runtime the poll-time case implies the I/O driver is going away, and an owner that saw it would
    /// not go on to adopt a config and admit more work.
    ///
    /// That is an argument about a dependency's internals, and no owner here is built on it. The submission
    /// case has no such caveat, and a runtime bump could change the reasoning. Owners therefore treat this
    /// outcome as terminal on its own terms - see the generation-versus-observability ordering in
    /// [crate::tcp::dns].
    Unobservable(Failure),
}

impl Completed {
    /// This transaction's own answer, for a caller that has already dealt with the ownership.
    pub(crate) fn answer(self) -> Result<Vec<u8>, Failure> {
        match self {
            Self::Answered(answer) => answer,
            Self::Unobservable(failure) => Err(failure),
        }
    }

    /// Whether a logical token this transaction stood for may never be reused.
    pub(crate) fn unobservable(&self) -> bool {
        matches!(self, Self::Unobservable(_))
    }
}

impl Resolving {
    /// Polls this transaction to its terminal, on the owner's own task rather than in one of its own.
    ///
    /// `dnsproxyd`'s resnsend handler writes one result and then drops the client socket, so what says the
    /// answer is whole is the *peer close*, not readability: `android_res_nresult` performs synchronous reads
    /// and must not be handed a nonblocking descriptor that has only part of a message. Readability without a
    /// close is therefore cleared and waited on again.
    ///
    /// # Both directions, because neither alone sees every close
    ///
    /// [AsyncFd] offers no arbitrary-interest poll, only one per direction, and each direction's readiness is
    /// masked to its own bits - so `Ready::is_error` is never set in either and a terminal condition written
    /// against it would be dead. What the two directions do cover between them is every way this descriptor
    /// can end: `mio` reports `EPOLLHUP` and `EPOLLIN|EPOLLRDHUP` as read-closed, and a bare `EPOLLERR` -
    /// with no `HUP` beside it - as *write*-closed. Watching only the read direction would therefore have
    /// rested on a claim about when the kernel raises `HUP`, and the cost of that claim being wrong is a
    /// transaction that never reaches a terminal at all: a descriptor record, a logical token and a query
    /// held until the session ends, with no timer by design. So the write direction is polled too, purely as
    /// a close detector, and its readiness is cleared when it says only that the socket is writable.
    pub(crate) fn poll_result(&mut self, cx: &mut Context<'_>) -> Poll<Completed> {
        let Some(fd) = self.fd.as_ref() else {
            // Unreachable: an owner removes a transaction the moment it produces a result, so nothing polls
            // one twice. Answered rather than asserted, because a panic here would take the process with it -
            // and answered as an ordinary failure, because nothing about it says Android kept a slot.
            let stale = io::Error::other("a resolver transaction polled after its own terminal");
            return Poll::Ready(Completed::Answered(Err(Failure::local(REGISTER)(stale))));
        };
        // The readiness wait is the runtime's. A failure in it is this process's own *and* it is the loss of
        // the only thing watching a transaction Android has already accepted, which is why it is not an
        // ordinary failure.
        let mut closed = false;
        loop {
            let mut ready = match fd.poll_read_ready(cx) {
                Poll::Pending => break,
                Poll::Ready(Ok(ready)) => ready,
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Completed::Unobservable(Failure::local(REGISTER)(e)))
                }
            };
            if ready.ready().is_read_closed() {
                closed = true;
                break;
            }
            ready.clear_ready_matching(Ready::READABLE);
        }
        while !closed {
            let mut ready = match fd.poll_write_ready(cx) {
                Poll::Pending => break,
                Poll::Ready(Ok(ready)) => ready,
                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Completed::Unobservable(Failure::local(REGISTER)(e)))
                }
            };
            if ready.ready().is_write_closed() {
                closed = true;
                break;
            }
            ready.clear_ready_matching(Ready::WRITABLE);
        }
        if !closed {
            return Poll::Pending;
        }
        let Some(fd) = self.fd.take() else {
            // Unreachable: it was present at the top of this call and nothing above takes it.
            return Poll::Pending;
        };
        let answer = fd.into_inner().finish().map_err(Failure::platform);
        Poll::Ready(Completed::Answered(answer))
    }

    /// Awaits this transaction's terminal, for an owner that runs it in a task of its own.
    pub(crate) async fn read(mut self) -> Completed {
        poll_fn(|cx| self.poll_result(cx)).await
    }
}

/// What one submission at the syscall boundary came to.
///
/// Three outcomes rather than two, because "Android never got it" and "Android got it and this process
/// cannot watch it" are different *ownerships* rather than two flavours of failure. Only the second leaves a
/// per-UID resolver slot taken with nothing here able to observe its end, and an owner that treated it as an
/// ordinary failure would refund a logical token for work Android is still doing.
pub(crate) enum Submission {
    /// `android_res_nsend` refused it, so nothing of Android's is held. One query's own expected failure.
    NeverReached(Failure),
    /// Accepted, with something this process can poll until the answer is terminal.
    Accepted(Resolving),
    /// Accepted by Android, and then this daemon's own wrapper failed. The descriptor is returned here - the
    /// dropped [ResolverQuery] cancels and closes it - but Android's slot is not, and there is nothing left
    /// to observe its completion with, so the logical token that named it may never be reused.
    Unobservable(Failure),
}

/// Submits one query on `network`, synchronously, and answers what that submission came to.
///
/// The failure is classified because a client drives how many of these there are. What `android_res_nsend`
/// answers is the platform's - a full per-UID limiter, a name that could not be resolved - and every one of
/// those reaches the client as SERVFAIL rather than as a report. Only the two wrapper steps below are this
/// daemon's own; see [vpnhotspotd::shared::failure].
pub(crate) fn submit(network: Network, message: &[u8]) -> Submission {
    // SAFETY: message outlives the call and its length is what the resolver is told to read.
    let fd = unsafe {
        android_res_nsend(
            network,
            message.as_ptr(),
            message.len(),
            ANDROID_RESOLV_NO_RETRY,
        )
    };
    if fd < 0 {
        return Submission::NeverReached(Failure::platform(io::Error::from_raw_os_error(-fd)));
    }
    let fd = ResolverQuery { fd: Some(fd) };
    // Past this point Android is holding a slot for this query whatever this process does next, which is why
    // both of the steps below answer with [Submission::Unobservable] rather than an ordinary failure.
    if let Err(e) = set_nonblocking(fd.as_raw_fd()) {
        return Submission::Unobservable(Failure::local(NONBLOCK)(e));
    }
    match AsyncFd::new(fd) {
        Ok(fd) => Submission::Accepted(Resolving { fd: Some(fd) }),
        Err(e) => Submission::Unobservable(Failure::local(REGISTER)(e)),
    }
}
