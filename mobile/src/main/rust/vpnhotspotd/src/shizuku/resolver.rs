//! One resolver transaction on a chosen `Network`, through the platform resolver rather than sockets the
//! daemon owns.
//!
//! The app-UID TestNetwork path's resolver, and only that: [crate::shizuku::virtual_dns] and the DNS-over-TCP
//! transaction table are its callers. Root's DNS proxy keeps its own resolver code in `root/dns.rs`,
//! unchanged, and nothing here is reachable from it.
//!
//! Going through the platform keeps private DNS, caching, and per-network resolver configuration, none of
//! which the daemon could reimplement. What it costs is that the operation belongs to Android once submitted:
//! `android_res_cancel` is a `close()` of the descriptor this process was handed, so dropping it recovers
//! this process's descriptor and not the resolver's work. Android's own query limiter releases the slot when
//! its `resolv_res_nsend` eventually returns, which is a temporary lifetime of Android's rather than
//! something this process can shorten, prove, or has to model.
//!
//! # Two steps here are the daemon's own
//!
//! `android_res_nsend` is synchronous: when it answers with a descriptor, the query is Android's. The two
//! steps that follow it - making that descriptor nonblocking and registering it with the runtime - are this
//! daemon's, and so is the readiness registration polled afterwards. None of them is one query's outcome, so
//! none of them is answered per query: each comes back as a [Failure::Local], which
//! [Failure::ending] turns into the error its owner ends on. An owner whose wrapper around the platform
//! failed cannot be trusted to wrap the next query either, so it stops accepting them - by ending the
//! app-UID dataplane task, and with it the session. What the platform itself answers, before or after
//! acceptance, stays [Failure::Expected] and is that one query's SERVFAIL.

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

/// The steps of a transaction that are this process's own, named so a failure at one of them is reported as
/// itself rather than as one more resolver answer. Not prefixed per mode: this path is shared, and both
/// conversations call the same two functions.
const NONBLOCK: &str = "resolver.nonblock";
const REGISTER: &str = "resolver.register";

/// android/multinetwork.h: `ResNsendFlags::ANDROID_RESOLV_NO_RETRY`.
const ANDROID_RESOLV_NO_RETRY: u32 = 1 << 0;

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
    pub(crate) fn poll_result(&mut self, cx: &mut Context<'_>) -> Poll<Result<Vec<u8>, Failure>> {
        let Some(fd) = self.fd.as_ref() else {
            // Unreachable: an owner removes a transaction the moment it produces a result, so nothing polls
            // one twice. Answered rather than asserted, because a panic here would take the process with it -
            // and answered as this daemon's own, because a transaction polled past its terminal is a bug here
            // rather than anything a client asked for.
            let stale = io::Error::other("a resolver transaction polled after its own terminal");
            return Poll::Ready(Err(Failure::local(REGISTER)(stale)));
        };
        // The readiness wait is the runtime's, so a failure in it is this process's own rather than one more
        // resolver answer - which is what makes it end the owner watching this transaction instead of
        // becoming that query's SERVFAIL.
        let mut closed = false;
        loop {
            let mut ready = match fd.poll_read_ready(cx) {
                Poll::Pending => break,
                Poll::Ready(Ok(ready)) => ready,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(Failure::local(REGISTER)(e))),
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
                Poll::Ready(Err(e)) => return Poll::Ready(Err(Failure::local(REGISTER)(e))),
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
        Poll::Ready(fd.into_inner().finish().map_err(Failure::platform))
    }

    /// Awaits this transaction's terminal, for an owner that runs it in a task of its own.
    pub(crate) async fn read(mut self) -> Result<Vec<u8>, Failure> {
        poll_fn(|cx| self.poll_result(cx)).await
    }
}

/// Submits one query on `network`, synchronously, and hands back what there is to wait on.
///
/// `Err` is that there is nothing to wait on, and *which* failure it was decides what the caller does with
/// it. `android_res_nsend` refusing - a full per-UID limiter, a name that could not be resolved - is the
/// platform's answer to one query the client chose to send, so it is [Failure::Expected] and reaches that
/// client as SERVFAIL. The two wrapper steps below are this daemon's own, so they are [Failure::Local] and
/// end the owner that asked; the descriptor Android returned is cancelled and closed by the dropped
/// [ResolverQuery] either way. See [vpnhotspotd::shared::failure].
pub(crate) fn submit(network: Network, message: &[u8]) -> Result<Resolving, Failure> {
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
        return Err(Failure::platform(io::Error::from_raw_os_error(-fd)));
    }
    let fd = ResolverQuery { fd: Some(fd) };
    // Past this point the query is Android's, and the two steps below are this process's own wrapper around
    // the descriptor it was handed - so each is classified as local, and each ends the owner that asked
    // rather than answering one query. Returning either way drops the descriptor, which cancels and closes
    // it; Android's own operation ends when its resolver work does, which nothing here waits for.
    if let Err(e) = set_nonblocking(fd.as_raw_fd()) {
        return Err(Failure::local(NONBLOCK)(e));
    }
    AsyncFd::new(fd)
        .map(|fd| Resolving { fd: Some(fd) })
        .map_err(Failure::local(REGISTER))
}
