//! Bounded handoff from Internet-facing reply sockets to the dataplane owner.
//!
//! Authorization occurs after handoff, so workers reserve owner capacity before reading or allocating.
//! When full, remote-controlled data remains in the kernel receive queue.

/// Capacity of each subsystem's reply handoff.
///
/// **Resource:** one unread-by-owner reply event, including at most one 65,535-byte protocol payload.
///
/// **Derivation:** one is the minimum [tokio::sync::mpsc::channel] capacity and matches the owner's one event
/// per subsystem per fair pass. UDP and Echo have separate mailboxes; including an event under processing,
/// the session can own at most three remote reply payloads.
///
/// **Failure mode:** an Internet peer can otherwise grow daemon memory faster than the owner consumes it.
///
/// **Exhaustion:** the worker waits before reading; the kernel queues or drops incoming data. Cancellation
/// and owner closure release the wait without a read.
pub const MAILBOX: usize = 1;

/// One subsystem's reply handoff: the sender each of its workers clones, and the receiver the owner keeps.
pub fn mailbox<E>() -> (tokio::sync::mpsc::Sender<E>, tokio::sync::mpsc::Receiver<E>) {
    tokio::sync::mpsc::channel(MAILBOX)
}

/// What one message off a socket's error queue is, as far as the decision below is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// An ICMP error a router sent, which the owner may be able to correlate.
    Remote,
    /// The kernel's own refusal of a send this worker did not make.
    Local,
    /// A message that named neither.
    Neither,
    /// The queue is empty.
    Empty,
}

/// What one readiness turn produced.
#[derive(Debug)]
enum Readiness<T> {
    /// A datagram, already allocated, to hand to the owner.
    Received(T),
    /// The readiness was stale: the read would have blocked, so the socket is waited on again.
    Stale,
    /// The socket has a queued kernel error instead of a datagram.
    Errored,
    /// The daemon's own I/O went wrong, which ends this worker.
    Failed(std::io::Error),
}

/// Classifies one attempted read, from exactly the shape `AsyncFd::try_io` answers with.
fn classify_readiness<T, W>(
    attempt: Result<std::io::Result<T>, W>,
    kernel_error: impl Fn(&std::io::Error) -> bool,
) -> Readiness<T> {
    match attempt {
        Ok(Ok(received)) => Readiness::Received(received),
        Ok(Err(e)) if kernel_error(&e) => Readiness::Errored,
        Ok(Err(e)) => Readiness::Failed(e),
        Err(_would_block) => Readiness::Stale,
    }
}

/// Where one error-readiness turn gets its messages.
pub trait ErrorSource {
    /// Takes the next message, or answers [Drained::Empty] when the queue is dry.
    fn next(&mut self) -> std::io::Result<Drained>;
}

/// What one whole reply-worker turn came to.
#[derive(Debug)]
pub enum Turned {
    /// A datagram went through the owner handoff.
    Sent,
    /// One router error went through it.
    Reported,
    /// Stale readiness, or an empty or unreportable error queue.
    Released,
    /// Cancelled while waiting for readiness, for owner capacity, or before a ready socket was read.
    Cancelled,
    /// The owner is gone, so there is nobody left to hand a reply to.
    Closed,
    /// The daemon's own I/O failed, which ends this worker.
    Failed(std::io::Error),
}

/// Everything one reply worker needs to take a turn, held together so the turn owns the whole order rather
/// than being handed a step someone else already took.
pub struct Turn<'a, X: std::os::fd::AsRawFd, S: ?Sized, E> {
    pub sender: &'a tokio::sync::mpsc::Sender<E>,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub fd: &'a tokio::io::unix::AsyncFd<X>,
    pub interest: tokio::io::Interest,
    /// Borrowed rather than built, so a turn can never leave a worker owning a second ancillary buffer.
    pub errors: &'a mut S,
}

impl<X, S, E> Turn<'_, X, S, E>
where
    X: std::os::fd::AsRawFd,
    S: ErrorSource + ?Sized,
{
    /// One whole turn: wait for readiness, take one owner slot, then read at most one unit and publish it.
    pub async fn run<T>(
        self,
        read: impl FnOnce(&X) -> std::io::Result<T>,
        kernel_error: impl Fn(&std::io::Error) -> bool,
        datagram: impl FnOnce(T) -> E,
        reported: impl FnOnce(&mut S) -> Option<E>,
    ) -> Turned {
        let Turn {
            sender,
            cancel,
            fd,
            interest,
            errors,
        } = self;
        // Do not hold shared owner capacity while the socket is idle.
        let mut guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Turned::Cancelled,
            ready = fd.ready(interest) => match ready {
                Ok(guard) => guard,
                Err(error) => return Turned::Failed(error),
            },
        };
        // Reserve before reading; cancellation or closure leaves the datagram in the kernel queue.
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Turned::Cancelled,
            reserved = sender.reserve() => match reserved {
                Ok(permit) => permit,
                Err(_) => return Turned::Closed,
            },
        };
        let attempt = guard.try_io(|inner| read(inner.get_ref()));
        match classify_readiness(attempt, kernel_error) {
            Readiness::Received(payload) => {
                permit.send(datagram(payload));
                Turned::Sent
            }
            Readiness::Stale => Turned::Released,
            Readiness::Failed(e) => Turned::Failed(e),
            Readiness::Errored => match errors.next() {
                Err(e) => Turned::Failed(e),
                // One error per turn; remaining errors keep the socket readable.
                Ok(Drained::Remote) => match reported(errors) {
                    Some(event) => {
                        permit.send(event);
                        Turned::Reported
                    }
                    None => Turned::Released,
                },
                // The send path handles local refusals; the other cases have nothing to publish.
                Ok(Drained::Local | Drained::Neither | Drained::Empty) => Turned::Released,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::task::Poll;

    use super::*;

    struct Scripted {
        backlog: std::collections::VecDeque<Drained>,
        asked: usize,
        built: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Scripted {
        fn new(backlog: Vec<Drained>, built: &std::rc::Rc<std::cell::Cell<usize>>) -> Self {
            built.set(built.get() + 1);
            Self {
                backlog: backlog.into(),
                asked: 0,
                built: std::rc::Rc::clone(built),
            }
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            self.built.set(self.built.get() - 1);
        }
    }

    impl ErrorSource for Scripted {
        fn next(&mut self) -> std::io::Result<Drained> {
            self.asked += 1;
            Ok(self.backlog.pop_front().unwrap_or(Drained::Empty))
        }
    }

    struct Pipe {
        read: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
        write: std::os::fd::OwnedFd,
    }

    impl Pipe {
        fn new() -> Self {
            use std::os::fd::{FromRawFd, OwnedFd};
            let mut ends = [0 as libc::c_int; 2];
            let made =
                unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
            assert_eq!(made, 0, "{}", std::io::Error::last_os_error());
            let (read, write) =
                unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
            Self {
                read: tokio::io::unix::AsyncFd::new(read).expect("a nonblocking pipe registers"),
                write,
            }
        }

        fn feed(&self, byte: u8) {
            use std::os::fd::AsRawFd;
            let wrote = unsafe { libc::write(self.write.as_raw_fd(), [byte].as_ptr().cast(), 1) };
            assert_eq!(wrote, 1, "{}", std::io::Error::last_os_error());
        }
    }

    fn read_one(
        allocations: &std::cell::Cell<usize>,
    ) -> impl FnOnce(&std::os::fd::OwnedFd) -> std::io::Result<Vec<u8>> + '_ {
        move |inner| {
            use std::os::fd::AsRawFd;
            let mut byte = [0u8; 1];
            let read =
                unsafe { libc::read(inner.as_raw_fd(), byte.as_mut_ptr().cast(), byte.len()) };
            if read < 0 {
                return Err(std::io::Error::last_os_error());
            }
            allocations.set(allocations.get() + 1);
            Ok(byte[..read as usize].to_vec())
        }
    }

    /// Polls one turn exactly once, which is how a worker parked on owner capacity is observed without
    /// letting it make progress it should not be able to make.
    async fn polled<F: Future<Output = Turned>>(turn: std::pin::Pin<&mut F>) -> Poll<Turned> {
        let mut turn = Some(turn);
        std::future::poll_fn(move |cx| Poll::Ready(turn.take().expect("polled once").poll(cx)))
            .await
    }

    #[tokio::test]
    async fn the_production_turn_orders_readiness_and_allocation() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = mailbox::<Vec<u8>>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let kernel = |e: &std::io::Error| e.kind() == std::io::ErrorKind::ConnectionRefused;
        let allocations = std::cell::Cell::new(0usize);
        let mut source = Scripted::new(Vec::new(), &built);

        pipe.feed(7);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(read_one(&allocations), kernel, |payload| payload, |_| None)
        .await;
        assert!(matches!(turned, Turned::Sent), "{turned:?}");
        assert_eq!(allocations.get(), 1);
        assert_eq!(receiver.recv().await, Some(vec![7]));

        pipe.feed(9);
        let drained = {
            use std::os::fd::AsRawFd;
            let mut byte = [0u8; 1];
            unsafe { libc::read(pipe.read.get_ref().as_raw_fd(), byte.as_mut_ptr().cast(), 1) }
        };
        assert_eq!(
            drained, 1,
            "consumed behind the turn's back, as another reader would"
        );
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(read_one(&allocations), kernel, |payload| payload, |_| None)
        .await;
        assert!(
            matches!(turned, Turned::Released),
            "a real WouldBlock is stale, not a failure: {turned:?}"
        );
        assert_eq!(allocations.get(), 1);
        assert_eq!(source.asked, 0);
        assert_eq!(built.get(), 1);
    }

    #[tokio::test]
    async fn a_full_mailbox_stops_the_next_read_until_the_owner_takes_one() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = mailbox::<Vec<u8>>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let kernel = |_: &std::io::Error| false;
        let allocations = std::cell::Cell::new(0usize);
        let mut source = Scripted::new(Vec::new(), &built);

        // One reply fills the whole handoff, because its capacity is one.
        pipe.feed(11);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(read_one(&allocations), kernel, |payload| payload, |_| None)
        .await;
        assert!(matches!(turned, Turned::Sent), "{turned:?}");
        assert_eq!(allocations.get(), 1);

        // A second datagram is waiting on the socket and the owner has not taken the first.
        pipe.feed(13);
        // Scoped so the parked turn releases its borrows before the counters below are read.
        {
            let mut second = pin!(Turn {
                sender: &sender,
                cancel: &cancel,
                fd: &pipe.read,
                interest: tokio::io::Interest::READABLE,
                errors: &mut source,
            }
            .run(read_one(&allocations), kernel, |payload| payload, |_| None));
            assert!(
                polled(second.as_mut()).await.is_pending(),
                "a full handoff parks the worker"
            );
            assert_eq!(
                allocations.get(),
                1,
                "and it parks before the read, so nothing was allocated for the second datagram"
            );

            // The owner takes the first, which is what frees the capacity for the second.
            assert_eq!(receiver.recv().await, Some(vec![11]));
            let turned = second.await;
            assert!(matches!(turned, Turned::Sent), "{turned:?}");
        }
        assert_eq!(allocations.get(), 2);
        assert_eq!(source.asked, 0);
        assert_eq!(receiver.recv().await, Some(vec![13]));
        assert_eq!(built.get(), 1);
    }

    #[tokio::test]
    async fn owner_closure_releases_a_worker_waiting_for_capacity() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = mailbox::<Vec<u8>>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let allocations = std::cell::Cell::new(0usize);
        let mut source = Scripted::new(Vec::new(), &built);

        pipe.feed(17);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(
            read_one(&allocations),
            |_| false,
            |payload| payload,
            |_| None,
        )
        .await;
        assert!(matches!(turned, Turned::Sent), "{turned:?}");

        pipe.feed(19);
        // Scoped so the parked turn - and the borrows it holds - are gone before the counters are read.
        {
            let mut waiting = pin!(Turn {
                sender: &sender,
                cancel: &cancel,
                fd: &pipe.read,
                interest: tokio::io::Interest::READABLE,
                errors: &mut source,
            }
            .run(
                read_one(&allocations),
                |_| false,
                |payload| payload,
                |_| None
            ));
            assert!(polled(waiting.as_mut()).await.is_pending());
            receiver.close();
            drop(receiver);
            let turned = waiting.await;
            assert!(matches!(turned, Turned::Closed), "{turned:?}");
        }
        assert_eq!(allocations.get(), 1, "the released worker read nothing");
        assert_eq!(source.asked, 0);
    }

    #[tokio::test]
    async fn cancellation_releases_a_worker_waiting_for_capacity() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, _receiver) = mailbox::<Vec<u8>>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let allocations = std::cell::Cell::new(0usize);
        let mut source = Scripted::new(Vec::new(), &built);

        pipe.feed(23);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(
            read_one(&allocations),
            |_| false,
            |payload| payload,
            |_| None,
        )
        .await;
        assert!(matches!(turned, Turned::Sent), "{turned:?}");

        pipe.feed(29);
        // Scoped for the same reason as above: the parked turn borrows both counters.
        {
            let mut waiting = pin!(Turn {
                sender: &sender,
                cancel: &cancel,
                fd: &pipe.read,
                interest: tokio::io::Interest::READABLE,
                errors: &mut source,
            }
            .run(
                read_one(&allocations),
                |_| false,
                |payload| payload,
                |_| None
            ));
            assert!(polled(waiting.as_mut()).await.is_pending());
            cancel.cancel();
            let turned = waiting.await;
            assert!(matches!(turned, Turned::Cancelled), "{turned:?}");
        }
        assert_eq!(allocations.get(), 1);
        assert_eq!(source.asked, 0);
    }

    #[tokio::test]
    async fn the_production_turn_takes_one_error_per_turn() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, mut receiver) = mailbox::<&'static str>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let kernel = |e: &std::io::Error| e.kind() == std::io::ErrorKind::ConnectionRefused;
        let errored = |_: &std::os::fd::OwnedFd| -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
        };

        let mut backlog = vec![Drained::Local; 5_000];
        backlog.extend(vec![Drained::Neither; 5_000]);
        backlog.push(Drained::Remote);
        let mut source = Scripted::new(backlog, &built);

        for turn in 0..10_000 {
            pipe.feed(1);
            let asked = source.asked;
            let turned = Turn {
                sender: &sender,
                cancel: &cancel,
                fd: &pipe.read,
                interest: tokio::io::Interest::READABLE,
                errors: &mut source,
            }
            .run(errored, kernel, |_| "payload", |_| Some("reported"))
            .await;
            assert!(
                matches!(turned, Turned::Released),
                "turn {turn}: {turned:?}"
            );
            assert_eq!(source.asked, asked + 1, "turn {turn}: one message only");
            assert_eq!(built.get(), 1, "turn {turn}: one scratch");
        }

        pipe.feed(1);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(errored, kernel, |_| "payload", |_| Some("reported"))
        .await;
        assert!(matches!(turned, Turned::Reported), "{turned:?}");
        assert_eq!(receiver.recv().await, Some("reported"));

        cancel.cancel();
        pipe.feed(1);
        let asked = source.asked;
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(errored, kernel, |_| "payload", |_| Some("reported"))
        .await;
        assert!(matches!(turned, Turned::Cancelled), "{turned:?}");
        assert_eq!(source.asked, asked);
        assert_eq!(built.get(), 1);
    }

    #[tokio::test]
    async fn owner_closure_prevents_a_ready_socket_read() {
        let pipe = Pipe::new();
        let built = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let (sender, receiver) = mailbox::<Vec<u8>>();
        drop(receiver);
        let cancel = tokio_util::sync::CancellationToken::new();
        let allocations = std::cell::Cell::new(0usize);
        let mut source = Scripted::new(Vec::new(), &built);
        pipe.feed(17);
        let turned = Turn {
            sender: &sender,
            cancel: &cancel,
            fd: &pipe.read,
            interest: tokio::io::Interest::READABLE,
            errors: &mut source,
        }
        .run(
            read_one(&allocations),
            |_| false,
            |payload| payload,
            |_| None,
        )
        .await;
        assert!(matches!(turned, Turned::Closed), "{turned:?}");
        assert_eq!(allocations.get(), 0);
        assert_eq!(source.asked, 0);
    }
}
