//! Wraps Android's asynchronous resolver descriptors for Tokio readiness.
use std::future::poll_fn;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::task::{Context, Poll};

use libc::c_int;
use tokio::io::unix::AsyncFd;
use tokio::io::Ready;
use vpnhotspotd::shared::dns_wire::MAX_MESSAGE;
use vpnhotspotd::shared::failure::Failure;

use crate::socket::set_nonblocking;

/// The steps of a transaction that are this process's own, named so a failure at one of them is reported as
/// itself rather than as one more resolver answer. Not prefixed per mode: this path is shared, and both
/// conversations call the same two functions.
const NONBLOCK: &str = "resolver.nonblock";
const REGISTER: &str = "resolver.register";

/// android/multinetwork.h: `ResNsendFlags::ANDROID_RESOLV_NO_RETRY`.
const ANDROID_RESOLV_NO_RETRY: u32 = 1 << 0;

/// android/multinetwork.h: `NETWORK_UNSPECIFIED`; leaves selection unset so dnsproxyd chooses using the
/// caller's peer UID.
const NETWORK_UNSPECIFIED: u64 = 0;

/// Owns an `android_res_nsend` descriptor. Dropping closes this process's handle but does not cancel or join
/// Android's resolver work.
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

/// Holds a submitted transaction's dnsproxyd descriptor. Platform acceptance or refusal, including `EBUSY`,
/// arrives through it.
pub(crate) struct Resolving {
    /// Taken at the terminal, which is what hands the descriptor to `android_res_nresult` to read and close.
    /// Absent afterwards, so a second poll cannot read a descriptor this transaction has already given up.
    fd: Option<AsyncFd<ResolverQuery>>,
}

impl Resolving {
    /// Polls the transaction to completion.
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

/// Submits one query with network selection unset, synchronously, and hands back what there is to wait on.
pub(crate) fn submit(message: &[u8]) -> Result<Resolving, Failure> {
    // SAFETY: message outlives the call and its length is what the resolver is told to read.
    let fd = unsafe {
        android_res_nsend(
            NETWORK_UNSPECIFIED,
            message.as_ptr(),
            message.len(),
            ANDROID_RESOLV_NO_RETRY,
        )
    };
    if fd < 0 {
        return Err(Failure::platform(io::Error::from_raw_os_error(-fd)));
    }
    let fd = ResolverQuery { fd: Some(fd) };
    // The remaining wrapper failures are local. Returning drops our descriptor without cancelling or joining
    // Android's work.
    if let Err(e) = set_nonblocking(fd.as_raw_fd()) {
        return Err(Failure::local(NONBLOCK)(e));
    }
    AsyncFd::new(fd)
        .map(|fd| Resolving { fd: Some(fd) })
        .map_err(Failure::local(REGISTER))
}
