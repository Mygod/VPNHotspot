//! The TUN ingress task, and with it the owner of every piece of client-keyed dataplane state.
//!
//! It reads one packet at a time from the descriptor the app transferred, classifies it, and dispatches it
//! to the transport that owns it. Ownership lands here rather than in a task per protocol because this is
//! the only reader of that descriptor: the state a packet touches is reached without a lock, and there is no
//! per-packet copy or channel hop on the way in.
//!
//! It also owns every task those transports started, through [crate::workers]. A worker that
//! finishes - because it failed, because its peer went, or because a retirement asked it to - is joined here,
//! and only then may its record be removed and its budget refunded. That is what makes an acknowledged epoch
//! mean the descriptors are actually back rather than merely spoken for, and it is why this loop selects on
//! the transports' terminals alongside their traffic.
//!
//! Joined is not always the same as retired. A terminating TCP flow whose worker finished *cleanly* while its
//! client was still closing keeps its client socket and its charge - the descriptor is gone with the task, and
//! what is left is a teardown this loop lets finish. Such a flow is settled by the owner's own scan rather
//! than by a terminal; see [crate::tcp::Engine::finished].
//!
//! Admission is checked per packet against the session's last applied config rather than by starting and
//! stopping this task, because a packet Android already queued in the kernel carries no epoch and arrives
//! whether the daemon is serving or not. Dropping it on the read side is the only place that decision can be
//! made.
//!
//! Config changes arrive as a request that is answered, not as a notification. That is what lets the session
//! acknowledge an epoch only after the state keyed to the previous one is actually gone: retirement happens
//! here, and the answer is what says it finished.

use std::io;
use std::net::IpAddr;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::protocol::{describe_io_error, IoErrorReportExt};
use vpnhotspotd::shared::reassembly;

use crate::budget::MAX_DATAGRAM;
use vpnhotspotd::shared::admission::{
    largest_fitting, Admission, Class, Denied, Headroom, Lease, Request,
};

use crate::budget::Measured;
use crate::dispatch::{Counters, Dispatch};
use crate::echo;
use crate::gateway::Gateways;
use crate::output::Output;
use crate::report;
use crate::tcp;
use vpnhotspotd::shared::ipv4_identification::{Ipv4Identifications, Prepared, Terminal};

use crate::tun_writer::{self, Stamp, Writer, TERMINAL_DEPTH};
use crate::udp;
use crate::virtual_dns;
use vpnhotspotd::shared::egress::Egress;

/// One applied config, as the dataplane needs it. Delivered whole rather than field by field, so a packet is
/// never handled against a half-updated view.
pub(crate) struct Applied {
    pub(crate) admitting: bool,
    pub(crate) upstream_generation: u64,
    pub(crate) downstream_epoch: u64,
    /// Where this session's traffic leaves, decoded once by the session loop - see
    /// [vpnhotspotd::shared::egress]. Two facts rather than one, because the resolver and the terminating TCP
    /// engine need only a handle to bind, while an unconnected relay also needs the interface its replies
    /// must arrive on.
    pub(crate) egress: Egress,
    pub(crate) virtual_addresses: Arc<Vec<IpAddr>>,
    pub(crate) gateway_addresses: Vec<IpAddr>,
    pub(crate) downstream_mtu_floor: usize,
    /// Answered once retirement is complete. The session waits on it before acknowledging the config.
    pub(crate) retired: oneshot::Sender<()>,
}

/// How many incomplete reassembly contexts the table is prepared for.
///
/// Prepared and charged once, so accepting a fragment into a context the table already holds - or opening a
/// new one inside this - allocates only what the aggregate was told about. The nested fragment cap is what
/// actually bounds the bytes; this bounds the map that indexes them.
const FRAGMENT_CONTEXTS: usize = 256;

/// The fraction of what the byte total still has that the IPv4 identification table may take.
///
/// A sixteenth. It is one owner among many, and the traffic it serves is the exception rather than the rule -
/// only IPv4 output above the downstream floor ever asks for an Identification - so it may not be sized as
/// though it were the dataplane's main cost. Derived from the measured share rather than fixed at a count,
/// because what the table can afford is a fact about the device and a count is a guess about it.
const IDENTIFICATION_SHARE: u64 = 16;

/// Everything the dataplane must own before either of its two tasks exists.
///
/// "Reserved before allocated" is a property of one function rather than of the type system, and it is worth
/// saying which. [prepare] is the only production caller of [crate::tun_writer::channel], and it calls it on
/// the far side of the reserve that pays for it; this bundle is what it hands out, so no production code
/// needs a writer or a queue from anywhere else. `channel` is still `pub(crate)` and a test calls it
/// directly, so the compiler is not what stops the sequence this replaced - where the session built the
/// channels first and the ingress task charged for them afterwards, leaving their state and first blocks
/// briefly outside the ledger. What stops it is that there is one production path and it reads in order.
pub(crate) struct Dataplane {
    admission: Admission,
    fixed: Fixed,
    writer: Writer,
    terminals: mpsc::Receiver<Terminal>,
    /// When this session's Identification allocator opened, taken before anything it will be compared
    /// against exists. Its first sixty seconds deny every guarded datagram, because a session that started a
    /// moment after its predecessor stopped would otherwise hand out values the predecessor had just written
    /// and a receiver could still be holding - see [vpnhotspotd::shared::ipv4_identification].
    opened: Instant,
    /// This session's seed for the client-side TCP stack, read from the kernel before anything exists to use
    /// it - see [session_seed] and [crate::tcp::Engine::new].
    seed: u64,
}

/// What a seed failure is called when the app is told about it.
const SEED_CONTEXT: &str = "shizuku.tcp_seed";

/// One 64-bit seed for this session's client-side TCP stack, from whatever `fill` puts in the eight bytes.
///
/// Split from the syscall below so that the two things that can be wrong with a seed are decided in one
/// place, and a test can drive that place with bytes of its choosing rather than with luck. Both are
/// refusals rather than repairs:
///
/// - a short fill is a seed nobody chose, since the rest of it would be whatever this buffer started as;
/// - zero is refused because zero is smoltcp's *own default*, so a session that seeded with it would be
///   indistinguishable from a session that never seeded at all - which is the whole failure being closed.
///   Deterministic, and it costs nothing real: the kernel returning eight zero bytes is not a case worth
///   carrying a retry loop for, and refusing a session is the same answer the read failing already gets.
fn seed_from(fill: impl FnOnce(&mut [u8; 8]) -> io::Result<usize>) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    let filled = fill(&mut bytes)?;
    if filled != bytes.len() {
        return Err(io::Error::other(format!(
            "{filled} of {} seed bytes for this session's TCP stack",
            bytes.len()
        )));
    }
    match u64::from_ne_bytes(bytes) {
        0 => Err(io::Error::other(
            "the seed for this session's TCP stack came out zero, which is smoltcp's own default",
        )),
        seed => Ok(seed),
    }
}

/// One 64-bit seed for this session's client-side TCP stack, straight from the kernel.
///
/// Read where a session can still decline to start: an entropy failure is a refusal, not something to paper
/// over with a clock reading or a process id. What it seeds is smoltcp's own RNG, whose default is zero and
/// which is what supplies passive-open initial sequence numbers - so without it every session of this daemon
/// hands out the same ISNs, and the sequence space a restarted session opens in is the one its predecessor
/// already used.
///
/// `getrandom` through `rustix` rather than a randomness crate of this daemon's own: it is already a
/// dependency, the call is one syscall with no state to keep, and eight bytes is far below the 256 the kernel
/// delivers whole once its pool is initialised.
///
/// A failure is *reported* as well as returned. [crate::report::init] has already run by the time [prepare]
/// is called, so without this the app sees the control socket reach EOF and a generic process failure with
/// nothing in it about entropy; the returned error still ends the session, and nothing here continues on a
/// seed it did not get.
fn session_seed(fill: impl FnOnce(&mut [u8; 8]) -> io::Result<usize>) -> io::Result<u64> {
    match seed_from(fill) {
        Ok(seed) => Ok(seed),
        Err(failed) => {
            // Attached first and emitted from the attachment, so this failure is described exactly once: the
            // error the caller ends the session with carries the very report that was sent, which is how
            // [crate::app_session] knows not to describe it again at teardown. The errno survives, because
            // that is what the report records.
            let failed = failed.with_report_context(SEED_CONTEXT);
            report::report(describe_io_error(
                SEED_CONTEXT,
                &failed,
                std::iter::empty::<(&str, &str)>(),
            ));
            Err(failed)
        }
    }
}

/// The kernel's eight bytes, for the one caller that wants a real seed rather than a decided one.
fn kernel_seed_bytes(bytes: &mut [u8; 8]) -> io::Result<usize> {
    rustix::rand::getrandom(bytes, rustix::rand::GetRandomFlags::empty()).map_err(io::Error::from)
}

/// Reserves everything fixed, and only then builds what those reservations pay for.
///
/// Fails rather than starting a dataplane whose accounting does not fit, and fails having built nothing: on
/// either error below the channels do not exist yet, so nothing is left allocated and uncharged.
pub(crate) fn prepare(
    measured: Measured,
    mtu: usize,
) -> io::Result<(Dataplane, tun_writer::Queue)> {
    let opened = Instant::now();
    // Before any *dataplane* owner is charged or allocated, because it is the one thing here the accounting
    // has no opinion about: a session whose TCP stack cannot be seeded does not start. Not before
    // everything - the control writer and the reporter are already up, deliberately, which is what lets the
    // refusal below reach the app as a structured report and be flushed rather than as a closed socket.
    let seed = session_seed(kernel_seed_bytes)?;
    // The one admission owner, built here and never shared: every owner below reaches it through the ingress
    // task, which is the only thing that reads client traffic. A worker never sees it at all.
    let mut admission = Admission::new(measured.totals).map_err(io::Error::other)?;
    // Every fixed owner is charged before a single byte of it is allocated, and each failure is a session
    // that does not start rather than a denial blamed on traffic later.
    let fixed = reserve_fixed(&mut admission, mtu).map_err(|why| {
        io::Error::other(format!(
            "the dataplane's fixed reservations do not fit: {why:?}"
        ))
    })?;
    // Reserved above, allocated here, and in that order deliberately.
    let (writer, queue, terminals) = tun_writer::channel();
    Ok((
        Dataplane {
            admission,
            fixed,
            writer,
            terminals,
            opened,
            seed,
        },
        queue,
    ))
}

pub(crate) async fn run(
    fd: Arc<AsyncFd<OwnedFd>>,
    mtu: usize,
    dataplane: Dataplane,
    mut configs: mpsc::Receiver<Applied>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let Dataplane {
        mut admission,
        fixed,
        writer,
        mut terminals,
        opened,
        seed,
    } = dataplane;
    let mut counters = Counters::default();
    let mut admitting = false;
    let mut stamp = Stamp::default();
    let mut virtual_addresses = Arc::new(Vec::new());
    let mut gateways = Gateways::new();
    // one MTU is the largest packet the interface can deliver, so a full read is never truncated
    let mut buffer = vec![0u8; mtu];
    let mut output = Output::new(
        mtu,
        Prepared {
            tuples: fixed.tuples,
            // One guarded packet per writer queue slot plus the one the writer has in hand, which is exactly
            // the depth of the settlement channel - so the allocator never registers an ending the writer
            // could fail to deliver.
            tracked: TERMINAL_DEPTH,
            opened,
        },
        writer,
    );
    let mut fragments = reassembly::Table::with_capacity(FRAGMENT_CONTEXTS);
    let started = (|| {
        let (relay, events) = udp::Relay::new(&mut admission)?;
        let (echo, echoes) = echo::Relay::new(&mut admission)?;
        let dns = virtual_dns::Handoff::new(&mut admission)?;
        let (tcp, flows, asking) = tcp::Engine::new(mtu, seed, &mut admission)?;
        Ok::<_, Denied>((relay, events, echo, echoes, dns, tcp, flows, asking))
    })();
    let (mut relay, mut events, mut echo, mut echoes, mut dns, mut tcp, mut flows, mut asking) =
        match started {
            Ok(started) => started,
            Err(why) => {
                // Through the same fence as every other exit rather than a bare return. No traffic owner
                // exists to stop, but the writer's channels and the Identification table do, and they are
                // charged: returning here would give their bytes back while they were still allocated, or -
                // worse - never give them back at all and never say so.
                let failed = io::Error::other(format!(
                    "the dataplane's owners do not fit the measured totals: {why:?}"
                ));
                release_fixed(
                    output,
                    buffer,
                    fragments,
                    terminals,
                    fixed,
                    &mut admission,
                    None,
                )
                .await;
                // Said before the line below rather than left for a reader to misread. An owner that was
                // built before the failing one has been *dropped* - its memory is gone - but a lease is an
                // inert identity and dropping it releases nothing, so those rows are still charged in the
                // counts that follow. The direction is fail-closed, an over-charge on a session that is
                // ending anyway; releasing them in order would mean unwinding four differently-typed
                // constructors by hand, which is a larger change than the diagnostic is worth.
                report::stdout!(
                    "tun ingress: startup failed after {} lease(s) were taken; owners built before the \
                     failure were dropped rather than released, so they are counted below",
                    admission.outstanding_leases()
                );
                report::stdout!("admission {}", admission.describe());
                return Err(failed);
            }
        };
    // Whatever ends this loop - EOF, cancellation, or a failure - the fence below it runs. A failure that
    // returned from inside would leave every worker to be aborted by the runtime instead of joined, which is
    // the one thing this task must never do with a descriptor.
    let result = loop {
        // Read before the select rather than inside it. Every arm is a future the macro builds up front, so a
        // deadline or a guard evaluated there would borrow an owner that a terminal arm has already borrowed
        // mutably - and these are the answers those arms would have given anyway.
        let mapping_deadline = relay.next_deadline();
        let echo_deadline = echo.next_deadline();
        let fragment_deadline = fragments.next_deadline();
        let tcp_deadline = tcp.next_deadline();
        let readable = tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(()),
            config = configs.recv() => {
                let Some(config) = config else {
                    break Ok(());
                };
                let retired = std::mem::replace(&mut stamp, Stamp {
                    generation: config.upstream_generation,
                    epoch: config.downstream_epoch,
                });
                // The writer is retired first, and this is a command with an answer rather than a published
                // value: when it returns, the writer has adopted the new stamp and abandoned whatever write it
                // was parked in, so no packet of the retired stamp can reach a client from here on. Everything
                // the retired state left queued then fails the dequeue comparison, and so does anything an
                // old-generation task enqueues before the sweeps below join it - which is why the sweeps do
                // not need to precede this. The terminal packets they write carry the new stamp and pass.
                //
                // Only on a real change: an admit-only update retires nothing, and preempting a write for one
                // would drop a client's packet for no reason.
                if stamp != retired {
                    if let Err(e) = output.writer().retire(stamp).await {
                        break Err(e);
                    }
                }
                output.set_floor(config.downstream_mtu_floor);
                // Each of these cancels what it owns, joins every task it started, and refunds only then, so
                // when the last one returns nothing of the retired stamp holds a descriptor. The virtual-DNS
                // handoff is deliberately absent: it owns no TUN-visible state and no selected-network socket,
                // and an in-flight query is never cancelled to free capacity - its answer is discarded on
                // arrival instead. The TCP engine draws the same distinction inside itself, because it holds
                // both kinds: a generation change retires the flows that hold a selected-network socket and
                // leaves the DNS-over-TCP transports, which hold none, running with their clients.
                //
                // This task is also the serialized owner every one of those queries is published by, and the
                // acknowledgement below is what that ordering is for: a query already accepted here was
                // accepted before this config was read, and one still queued is accepted after it - under the
                // successor, which is what it will be answered against.
                relay.apply(stamp, config.egress.relay_upstream, &mut admission).await;
                echo.apply(stamp, config.egress.relay_upstream, &mut admission).await;
                dns.apply(stamp, config.egress.selected_network);
                gateways.set(&config.gateway_addresses);
                tcp.apply(
                    stamp,
                    config.egress.selected_network,
                    output.floor(),
                    &mut admission,
                    &mut output,
                ).await;
                if stamp.epoch != retired.epoch {
                    // Keyed by TUN-visible tuples like everything else, and holding no descriptor, so the
                    // whole table goes at once and its bytes are freed here rather than awaited.
                    fragments.retire(&mut admission, &fixed.fragments);
                    // reported together, because the retirement that just happened is exactly what makes
                    // the relay's own numbers final for that epoch
                    report::stdout!("tun ingress {}", counters.describe(retired.epoch));
                    report_owners(&relay, &echo, &fragments, &dns, &tcp, &output, &admission);
                    counters = Counters::default();
                }
                admitting = config.admitting;
                virtual_addresses = config.virtual_addresses;
                // Only now, with every worker of the previous stamp joined and every refund made, may the
                // session acknowledge the config.
                if config.retired.send(()).is_err() {
                    break Err(io::Error::other("the session abandoned a config it sent"));
                }
                continue;
            }
            // The terminal arms come before the traffic ones, because a worker that has finished is holding a
            // record and a charge that nothing else will release: taking its completion first is what keeps a
            // busy relay from deferring its own refunds.
            //
            // A guarded packet's ending is the same kind of thing and comes first among them: until it is
            // applied, that packet holds one of the allocator's tracking slots and its tuple's sequence
            // cannot be reused, and the writer has already done everything it can with it. After the config
            // arm rather than before, for the same reason every other terminal is: a config must never be
            // starved by traffic, and which datagrams are guarded is a client's choice.
            terminal = terminals.recv() => {
                match terminal {
                    Some(terminal) => output.terminal(terminal),
                    // The writer task has gone, which ends this session through its own result.
                    None => break Ok(()),
                }
                continue;
            }
            terminal = relay.finished() => {
                relay.close(terminal, &mut admission);
                continue;
            }
            terminal = echo.finished() => {
                echo.closed(terminal, &mut admission);
                continue;
            }
            // Three kinds, because two things outlive the flow that started them. A DNS-over-TCP transaction
            // settles when the platform is actually done, which can be after the config that swept its flow
            // was acknowledged. And a flow can outlive its own *worker*: both workers finish as soon as their
            // ordered work is done, while the client's teardown still has a FIN to retransmit and an
            // acknowledgment to wait for, so a clean terminal detaches the flow and the third kind is the
            // client side of it finally finishing.
            finished = tcp.finished() => {
                match finished {
                    tcp::Finished::Flow(terminal) => tcp.close(terminal, &mut admission, &mut output),
                    tcp::Finished::Transaction(terminal) => tcp.settle(terminal, &mut admission),
                    tcp::Finished::Detached { handle, worker } => {
                        tcp.settled(handle, worker, &mut admission)
                    }
                }
                continue;
            }
            // One arm, because an answer is not an event this loop acts on: it is state parked on the
            // transaction it belongs to, and the event is the worker completing. That is also what makes the
            // two orders the scheduler may present - answer first, or terminal first - into one order.
            terminal = dns.settled() => {
                dns.settle(terminal, &mut output, &mut admission);
                continue;
            }
            event = events.recv() => {
                match event {
                    Some(event) => relay.handle(event, &mut output),
                    // impossible while the relay holds a sender
                    None => break Ok(()),
                }
                continue;
            }
            event = echoes.recv() => {
                match event {
                    Some(event) => echo.handle(event, &mut output, &mut admission),
                    // impossible while the echo relay holds a sender
                    None => break Ok(()),
                }
                continue;
            }
            // Unguarded, because what travels here now carries no payload: it is a wake naming one exact
            // identity, and the payload waits in that flow's own mailbox until its turn comes round. The
            // guard this replaces was the head-of-line blocking - one flow's undelivered chunk stopped every
            // other flow's events from being read at all.
            // A DNS-over-TCP transport asking for a query to be admitted, or saying that an answer has been
            // fully delivered. Before the traffic arms, because a transport waiting on an answer is a client
            // already waiting, and both answers are cheap.
            ask = asking.recv() => {
                match ask {
                    // Answered against the session's current admission state, not the one the transport was
                    // opened under: a stopping session drains the exchanges it already owns and starts none.
                    Some(ask) => tcp.ask(ask, admitting, &mut admission),
                    // impossible while the engine holds a sender
                    None => break Ok(()),
                }
                continue;
            }
            flow = flows.recv() => {
                match flow {
                    Some(flow) => tcp.handle(flow, admitting, now(), &mut output),
                    // impossible while the engine holds a sender
                    None => break Ok(()),
                }
                continue;
            }
            () = sleep_until_deadline(mapping_deadline) => {
                relay.sweep(&mut admission);
                continue;
            }
            () = sleep_until_deadline(echo_deadline) => {
                echo.sweep(&mut admission);
                continue;
            }
            () = sleep_until_deadline(fragment_deadline) => {
                Dispatch {
                    counters: &mut counters,
                    relay: &mut relay,
                    echo: &mut echo,
                    dns: &mut dns,
                    tcp: &mut tcp,
                    fragments: &mut fragments,
                    output: &mut output,
                    admission: &mut admission,
                    fragment_lease: &fixed.fragments,
                    gateways: &gateways,
                    stamp,
                    virtual_addresses: &virtual_addresses,
                }.expire(now());
                continue;
            }
            // The stack's own retransmission and delayed-acknowledgement timers, which only fire if it is
            // polled, and the outer idle floors this daemon owns - one arm, because whichever is first the
            // answer is the same two steps. The stack runs first, so a socket that has just finished is
            // already retiring by the time the floors are read.
            //
            // The instant is captured *before* that poll and the floors are judged against it, which defers
            // by one loop any flow that came due while the stack was running: the poll advances smoltcp on
            // its own reading of the clock, so a later reading here would expire a flow against a moment the
            // stack had not been asked about yet. Deferring is the conservative direction - a floor is a
            // minimum - and the next wake is immediate, because that flow's deadline is by then in the past.
            () = sleep_until_deadline(tcp_deadline) => {
                let now = now();
                tcp.poll(&mut output);
                tcp.expire(now, &mut output);
                continue;
            }
            readable = fd.readable() => readable,
        };
        let mut guard = match readable {
            Ok(guard) => guard,
            Err(e) => break Err(e),
        };
        let read = match guard.try_io(|inner| {
            // SAFETY: buffer is owned here and its length is what the kernel is told to write.
            let read = unsafe {
                libc::read(
                    std::os::fd::AsRawFd::as_raw_fd(inner.get_ref()),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(read as usize)
            }
        }) {
            Ok(Ok(read)) => read,
            Ok(Err(e)) => break Err(e),
            // readiness was stale; wait for it again
            Err(_would_block) => continue,
        };
        if read == 0 {
            continue;
        }
        if !admitting {
            // Only ACTIVE admits traffic. Counting rather than reporting keeps a client that keeps sending
            // while the session is not serving from producing one report per packet.
            counters.unadmitted += 1;
            continue;
        }
        Dispatch {
            counters: &mut counters,
            relay: &mut relay,
            echo: &mut echo,
            dns: &mut dns,
            tcp: &mut tcp,
            fragments: &mut fragments,
            output: &mut output,
            admission: &mut admission,
            fragment_lease: &fixed.fragments,
            gateways: &gateways,
            stamp,
            virtual_addresses: &virtual_addresses,
        }
        .accept(&buffer[..read], now());
    };
    // The session is over, so every owner is told to stop and every task it started is joined before this
    // returns. Nothing daemon-owned outlives this task: the egress task is joined by the session loop, and the
    // descriptor the two of them share is closed by whichever of their `Arc`s goes last.
    //
    // This is as far as a process can go. A resolver transaction submitted to `dnsproxyd` belongs to Android,
    // so what is recovered here is this process's descriptors and tasks, and nothing claims the platform's own
    // accounting is settled by it.
    relay.shutdown(&mut admission).await;
    echo.shutdown(&mut admission).await;
    tcp.shutdown(&mut admission, &mut output).await;
    dns.shutdown(&mut output, &mut admission).await;
    report::stdout!("tun ingress {}", counters.describe(stamp.epoch));
    report_owners(&relay, &echo, &fragments, &dns, &tcp, &output, &admission);
    // Every owner gives back its own retained capacity, after everything it admitted has been settled. What
    // remains outstanding after this is a leak, and the line below is where it is visible.
    // Each of these takes the ingress task's half of that owner's reply channel with it, because the lease it
    // releases covers the channel and every payload its slots may hold - see [crate::udp::Relay::release].
    relay.release(events, &mut admission);
    echo.release(echoes, &mut admission);
    dns.release(&mut admission);
    // The engine's fixed lease pays for the readiness and ask channels, so the ingress task's halves of both
    // go with it - see [crate::tcp::Engine::release].
    tcp.release(flows, asking, &mut admission);
    release_fixed(
        output,
        buffer,
        fragments,
        terminals,
        fixed,
        &mut admission,
        Some(&cancel),
    )
    .await;
    report::stdout!("admission {}", admission.describe());
    result
}

/// Gives the fixed reservations back, and only once every allocation they paid for is physically gone.
///
/// The order is the whole point, and it is the reverse of [prepare]. Releasing while the thing still exists
/// would be an under-charge for as long as the gap lasted, which is the fail-open case the aggregate exists
/// to prevent - and the gap this closes was not a moment but the rest of the session's teardown.
///
/// - `fragments` is retired against its own lease while it is still here, then dropped.
/// - `output` goes next, and dropping it is what starts everything else: it owns the Identification table
///   those bytes were reserved for, and the only [Writer] in the process, so the writer's packet and
///   retirement senders die with it.
/// - The egress task then finds its packet channel closed - or, if it is parked waiting for a kernel that
///   will not take a write, finds its retirement channel closed, which preempts it exactly as a real
///   retirement would.
/// - That task's last act is to drop its settlement sender, so draining this receiver to `None` is proof
///   that the queue, both receivers and any packet it had in hand are gone. It is the channel the writer's
///   own reservation already paid for, so nothing here needs a second handshake to say the same thing.
/// - The drained receiver is then dropped, and that is what finally frees the channel. A closed channel is
///   not a freed one: the sender going away is what ends the wait, but the shared state and its blocks live
///   until the last endpoint does, and this is the last endpoint. Releasing on the close alone would give
///   back bytes for an allocation this task was still holding.
///
/// `cancel` is not what makes that wait terminate - dropping `output` is, on both of the writer's waits -
/// and it is passed here only so the loop's own exit does not depend on the session having cancelled it
/// already. `None` on the startup-failure path, where the writer has never been given a packet.
async fn release_fixed(
    output: Output,
    buffer: Vec<u8>,
    mut fragments: reassembly::Table,
    mut terminals: mpsc::Receiver<Terminal>,
    fixed: Fixed,
    admission: &mut Admission,
    cancel: Option<&CancellationToken>,
) {
    fragments.retire(admission, &fixed.fragments);
    drop(output);
    drop(buffer);
    drop(fragments);
    if let Some(cancel) = cancel {
        // Already cancelled on most paths; harmless on the ones where the loop ended for its own reasons,
        // and it is what stops an egress task that is between packets rather than reading one.
        cancel.cancel();
    }
    // Terminals for packets whose allocator no longer exists, discarded on the way past: what is being
    // waited for is the close, not the contents.
    while terminals.recv().await.is_some() {}
    drop(terminals);
    fixed.release(admission);
}

/// Every byte-only owner that exists for the whole session, charged once before any of them is built.
///
/// Four leases rather than one number, because a denial has to be able to say *which* owner did not fit.
/// They are taken in one call and given back in one call - see [Fixed::release] - so this is a breakdown of
/// a single lifetime rather than four separate ones.
struct Fixed {
    /// Everything the TUN writer owns for the session: its packet queue and the payloads in it, the one
    /// packet it has taken out of that queue and is writing, its retirement channel, and the settlement
    /// channel a guarded packet's ending travels back on. One lease rather than four, because the four are
    /// built by one call and released by one drop, and a denial naming one of them would say nothing a
    /// denial naming the writer does not.
    writer_queue: Lease,
    /// The ingress read buffer, plus the transient peak one completed reassembly or one output packetization
    /// reaches. At most one of each exists at a time, because this is a single owner that consumes each
    /// before it asks for the next.
    scratch: Lease,
    /// The reassembly table's own map, and the aggregate every context it holds is charged against.
    fragments: Lease,
    /// The IPv4 identification table, as one owner rather than a record per tuple.
    identifications: Lease,
    /// How many tuples that table was prepared for, so the table and its charge cannot disagree.
    tuples: usize,
}

impl Fixed {
    fn release(self, admission: &mut Admission) {
        admission.release(self.writer_queue);
        admission.release(self.scratch);
        admission.release(self.fragments);
        admission.release(self.identifications);
    }
}

fn reserve_fixed(admission: &mut Admission, mtu: usize) -> Result<Fixed, Denied> {
    // Four allocations in one lease, and what they are is the writer's own to say - see
    // [crate::tun_writer::footprint], which is the equation this reserve and that construction share.
    let writer_queue = tun_writer::footprint(mtu).ok_or(Denied::Arithmetic)?;
    let writer_queue = admission.reserve(Request::bytes(writer_queue, Class::General))?;
    // Reserved-class: a packet already accepted must not fail to be written because relayed traffic filled
    // the share. Checked rather than saturating: an `mtu` that made this wrap would be clamped to a figure
    // smaller than what is really allocated, which is the one direction an admission bound may not err in.
    let scratch = (mtu as u64)
        .checked_add(2 * MAX_DATAGRAM as u64)
        .ok_or(Denied::Arithmetic)?;
    let scratch = admission.reserve(Request::bytes(scratch, Class::Reserved))?;
    let fragments = reassembly::Table::footprint(FRAGMENT_CONTEXTS)
        .ok_or(Denied::Arithmetic)
        .and_then(|bytes| admission.reserve(Request::bytes(bytes, Class::General)))?;
    // One table owner, charged once, rather than a record per tuple: a tuple is not a descriptor and holds
    // nothing but a small sequence, a pending count and a timestamp, so spending an aggregate record on each
    // would let a client that talks to many destinations exhaust the budget for mappings and flows with a few
    // bytes apiece.
    //
    // How many tuples is derived rather than chosen. A full table refuses a *new* tuple unless it can give
    // away the slot of one that can no longer collide with anything - see [Ipv4Identifications] - so the
    // capacity decides how many concurrently-sending tuples may have oversized output, and the right size for
    // that is whatever a documented share of the aggregate will hold. A share rather than the whole of it,
    // because this is one owner among many and the traffic it serves is the exception rather than the rule:
    // only IPv4 output above the downstream floor ever asks. What the share buys is that *count* - the charge
    // below is the rows' own state, and the container's own indexing around them is count-bounded rather than
    // measured. Reclaiming inside it is logical: the bound does not move, and the charge was taken for that
    // bound rather than for the rows in it, so the one charge covers the table for the whole session however
    // many tuples pass through it.
    let headroom = admission.general_headroom();
    let tuples = largest_fitting(
        Headroom {
            // Tuples are not records; what bounds this table is the share of general bytes below.
            records: u32::MAX,
            bytes: headroom.bytes / IDENTIFICATION_SHARE,
        },
        0,
        Ipv4Identifications::footprint,
    );
    let identifications = Ipv4Identifications::footprint(tuples)
        .ok_or(Denied::Arithmetic)
        .and_then(|bytes| admission.reserve(Request::bytes(bytes, Class::General)))?;
    Ok(Fixed {
        writer_queue,
        scratch,
        fragments,
        identifications,
        tuples,
    })
}

/// One report for every owner at once, because they share the budget and the writer: reading any of them
/// alone would leave the others' share of those unexplained.
fn report_owners(
    relay: &udp::Relay,
    echo: &echo::Relay,
    fragments: &reassembly::Table,
    dns: &virtual_dns::Handoff,
    tcp: &tcp::Engine,
    output: &Output,
    admission: &Admission,
) {
    report::stdout!("udp relay {}", relay.describe());
    report::stdout!("echo relay {}", echo.describe());
    report::stdout!("reassembly {}", fragments.describe());
    report::stdout!("virtual dns {}", dns.describe());
    report::stdout!("tcp engine {}", tcp.describe());
    report::stdout!("tun output {}", output.describe());
    report::stdout!("admission {}", admission.describe());
}

/// This task's own reading of the clock.
///
/// The runtime's rather than the standard library's, because every deadline this task computes is one it
/// later sleeps on through [sleep_until_deadline], and those two have to be the same clock or a deadline is
/// measured against a moment the sleep never sees. Outside a test harness the two *are* the same call:
/// tokio's controllable clock only exists when the `test-util` feature is built, and that feature is a
/// dev-dependency, so it is never unified into the daemon binary.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// Waits for the next expiry, or forever when there is none. `pending` rather than a poll interval, so an
/// idle table costs no wakeups at all.
async fn sleep_until_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::SocketAddr;
    use std::time::Duration;

    use smoltcp::wire::{TcpControl, TcpRepr, TcpSeqNumber};

    use crate::budget;

    const MTU: usize = 1_500;

    /// Every seed a session could be given, decided rather than sampled.
    ///
    /// Driven through the production seam [seed_from], with the bytes supplied by this test instead of by the
    /// kernel, so each case is an assertion about the rule and not about what a draw happened to be. The
    /// zero case is the one that matters: zero is smoltcp's own default, so a seed of zero and never having
    /// seeded at all are the same interface, and it must not be reachable. What the *kernel* path adds on
    /// top of this is one syscall, which the read below exercises without asserting anything about the value
    /// it returns.
    #[test]
    fn a_seed_is_refused_unless_it_is_whole_and_not_the_default() {
        assert_eq!(
            seed_from(|bytes| {
                *bytes = 0x0102_0304_0506_0708u64.to_ne_bytes();
                Ok(bytes.len())
            })
            .expect("a whole, non-default seed"),
            0x0102_0304_0506_0708,
            "the bytes are taken as they are, in native order"
        );
        // The lowest seed that is not the default still passes, so what is refused below is zero itself
        // rather than a range somebody widened.
        assert_eq!(
            seed_from(|bytes| {
                *bytes = 1u64.to_ne_bytes();
                Ok(bytes.len())
            })
            .expect("one is a seed"),
            1
        );
        let zero = seed_from(|bytes| {
            *bytes = [0; 8];
            Ok(bytes.len())
        })
        .expect_err("zero is what an unseeded interface already has");
        assert!(zero.to_string().contains("zero"), "{zero}");
        let short = seed_from(|bytes| Ok(bytes.len() - 1))
            .expect_err("a partly filled seed is a seed nobody chose");
        assert!(short.to_string().contains("7 of 8"), "{short}");
        let failed = seed_from(|_| Err(io::Error::from_raw_os_error(libc::EAGAIN)))
            .expect_err("and a fill that failed is not a seed at all");
        assert_eq!(failed.raw_os_error(), Some(libc::EAGAIN));
    }

    /// A seed this process cannot get is described to the app, not just returned.
    ///
    /// The failure mode without it is silence with a shape: [prepare] returns before either task is spawned,
    /// the session ends, and what the app sees is the control socket reaching EOF - a generic process failure
    /// naming nothing. So the one thing asserted here is that the report exists and says which step failed,
    /// through the same registry the session installs and the same collector the writer's own fatal-report
    /// test uses.
    ///
    /// Driven through the production [session_seed] itself, with only the fill replaced: the reporting and
    /// the decision to return the error anyway are the real ones, and nothing about the value of a real seed
    /// is asserted.
    #[tokio::test]
    async fn a_seed_failure_is_reported_before_the_session_ends() {
        let _reporting = crate::report::exclusive().await;
        let (control, mut published) = mpsc::unbounded_channel();
        let reporter = crate::report::init_owned(control.clone(), |_, _| Vec::new())
            .expect("no other conversation owns reporting");
        let collector = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(message) = published.recv().await {
                let crate::report::ControllerMessage::Nonfatal { report, .. } = message else {
                    continue;
                };
                if report.context == SEED_CONTEXT {
                    seen.push(report);
                }
            }
            seen
        });

        // The production path, on the branch a kernel refusal takes, and the error still comes back.
        let failed = session_seed(|_| Err(io::Error::from_raw_os_error(libc::EAGAIN)))
            .expect_err("a seed that could not be read is not a seed");
        assert_eq!(
            vpnhotspotd::shared::protocol::error_errno(&failed),
            Some(libc::EAGAIN),
            "the session still ends on the original failure, errno and all"
        );
        // And it comes back *carrying* the report that was just emitted, which is what stops the session's
        // own teardown describing the same failure a second time - see [crate::app_session].
        let carried = vpnhotspotd::shared::protocol::reported_io_error_report(&failed)
            .expect("the returned error carries the report that was emitted");
        assert_eq!(SEED_CONTEXT, carried.context);
        assert_eq!(Some(libc::EAGAIN), carried.errno);

        reporter.finish().await.expect("the flush completes");
        drop(control);
        let reports = collector.await.expect("the collector joined");
        let [reported] = &reports[..] else {
            panic!("exactly one report about the seed, not {}", reports.len())
        };
        assert_eq!(reported.context, SEED_CONTEXT, "named as the seed step");
        assert_eq!(
            reported.errno,
            Some(libc::EAGAIN),
            "carrying the kernel's own errno: {reported:?}"
        );
        assert!(
            reported.file.ends_with("tun_reader.rs") && reported.line > 0,
            "and where in this daemon it happened: {}:{}",
            reported.file,
            reported.line
        );
    }

    /// Nothing the writer owns exists before the bytes it owns are charged for.
    ///
    /// The order is what this is about, and the reserve is the only fallible step in it: a
    /// [tun_writer::Queue] is what proves the three channels were built, [prepare] is the only production
    /// call that makes one, and it makes one strictly after the reserve it is on the far side of. So a
    /// budget that refuses the writer's own bytes, answering `Err` with no queue, is the whole claim - and
    /// the sequence this replaced, where the session built the channels and the ingress task charged for
    /// them afterwards, could not produce that answer because it had already allocated by the time it found
    /// out.
    ///
    /// Starved by exactly one byte, derived from the writer's own equation, and with the fragment cap
    /// brought inside the smaller total. The earlier version of this simply flattened `byte_total` onto the
    /// reserved floor and left the cap where the device measured it, which is a total `Admission::new`
    /// rejects outright - so it proved that invalid totals are refused, not that the sizing path refuses a
    /// valid budget that is too small, and its bare `is_err` could not tell the two apart.
    #[tokio::test]
    async fn a_budget_a_byte_short_of_the_writer_refuses_and_charges_nothing() {
        let mut starved = budget::measure().await.expect("the host measures");
        let owed = tun_writer::footprint(MTU).expect("chargeable");
        starved.totals.byte_total = starved.totals.reserved_byte_floor + owed - 1;
        starved.totals.fragment_cap = starved.totals.fragment_cap.min(starved.totals.byte_total);

        // The production sizing path, on a ledger this test can still read afterwards. The baseline is not
        // zero: a fresh aggregate already carries its own ledger's bytes, which is the point of taking it
        // here rather than assuming an empty one.
        let mut admission =
            Admission::new(starved.totals).expect("the totals are internally valid");
        let (charged, leases) = (admission.bytes_charged(), admission.outstanding_leases());
        let refused = match reserve_fixed(&mut admission, MTU) {
            Err(refused) => refused,
            Ok(_) => panic!("a byte short of the writer is short"),
        };
        // The writer's own reserve is the first one and the one that fails, so nothing after it ran either.
        assert_eq!(refused, Denied::Bytes);
        assert_eq!(
            admission.bytes_charged(),
            charged,
            "a refused reserve leaves the ledger where it found it"
        );
        assert_eq!(admission.outstanding_leases(), leases);

        // And through the production call, where that refusal - not the totals themselves - is what stops
        // the session before a channel exists.
        let failed = prepare(starved, MTU)
            .err()
            .expect("no dataplane starts on it");
        assert!(
            failed.to_string().contains("fixed reservations do not fit"),
            "{failed}"
        );
    }

    /// Owners that cannot be built still leave through the fence rather than around it.
    ///
    /// Bytes enough for everything fixed and a little over, so `prepare` succeeds and what fails is a
    /// traffic owner - the branch that used to be a bare `return`, leaving the writer's channels and the
    /// Identification table charged with nobody to give them back. Sized from the real equations rather than
    /// guessed at: the writer's own footprint plus the reassembly table's, and then a slack far smaller than
    /// the megabytes the UDP relay's reply queue asks for, so the first owner to reach the aggregate is
    /// refused. Starving records instead would not do it - every owner sized by records solves its prepared
    /// count against the headroom and degrades to zero rather than failing.
    ///
    /// Driven with the real egress task alongside, because the fence waits on that task's settlement sender
    /// closing: a branch that skipped the fence would return just as fast, and one that reached a fence with
    /// no egress task to end would never return at all.
    #[tokio::test]
    async fn owners_that_do_not_fit_still_leave_through_the_fence() {
        let mut cramped = budget::measure().await.expect("the host measures");
        let fixed_bytes = tun_writer::footprint(MTU).expect("chargeable")
            + reassembly::Table::footprint(FRAGMENT_CONTEXTS).expect("chargeable");
        cramped.totals.byte_total = cramped.totals.reserved_byte_floor + fixed_bytes + 64 * 1024;
        cramped.totals.fragment_cap = cramped.totals.fragment_cap.min(cramped.totals.byte_total);
        let (dataplane, queue) = prepare(cramped, MTU).expect("the fixed owners still fit");

        let mut ends = [0 as libc::c_int; 2];
        // SAFETY: pipe2 fills the two descriptors it is given and reads nothing else.
        assert_eq!(
            unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_NONBLOCK) },
            0
        );
        // SAFETY: both descriptors were just created and are owned by nothing else.
        let (reader, write) = unsafe {
            use std::os::fd::FromRawFd;
            (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1]))
        };
        let fd = Arc::new(AsyncFd::new(write).expect("pollable"));
        let cancel = CancellationToken::new();
        let egress = tokio::spawn(crate::tun_writer::run(
            Arc::clone(&fd),
            MTU,
            queue,
            cancel.clone(),
        ));
        let (_configs, requests) = mpsc::channel(1);

        let failed = match run(fd, MTU, dataplane, requests, cancel).await {
            Err(failed) => failed,
            Ok(()) => panic!("a dataplane with no records for its owners must not serve"),
        };
        assert!(failed.to_string().contains("owners do not fit"), "{failed}");
        // Reached only because the fence completed: the egress task saw its senders go and ended itself.
        egress.await.expect("joined").expect("a clean writer");
        drop(reader);
    }

    /// A prepared dataplane arrives with the writer's whole footprint already charged, before either task
    /// exists to put a packet in it.
    #[tokio::test]
    async fn prepare_charges_the_writer_before_either_task_runs() {
        let measured = budget::measure().await.expect("the host measures");
        let owed = tun_writer::footprint(MTU).expect("chargeable");
        let (dataplane, queue) = prepare(measured, MTU).expect("prepared");
        let Dataplane {
            mut admission,
            fixed,
            writer,
            terminals,
            ..
        } = dataplane;
        assert!(
            admission.bytes_charged() >= owed,
            "{} charged is less than the {owed} the writer owns",
            admission.bytes_charged()
        );
        assert_eq!(
            admission.outstanding_leases(),
            4,
            "the four fixed leases and nothing else"
        );
        drop((writer, terminals, queue));
        fixed.release(&mut admission);
        assert_eq!(admission.outstanding_leases(), 0);
    }

    /// The fixed reservation is given back only once the things it paid for are physically gone.
    ///
    /// The egress endpoints are held here rather than by a spawned writer, which is the point: the fence
    /// must wait for whoever holds them, and it must not be able to tell the difference. Without the wait,
    /// the release below happens while the queue, both receivers and the settlement sender are all still
    /// alive - bytes given back for allocations that still exist, which is the fail-open direction the
    /// aggregate is for.
    ///
    /// `None` for the cancellation token is exactly what the owner-initialization failure path passes, so
    /// this covers that call as well as the loop's.
    #[tokio::test(start_paused = true)]
    async fn fixed_bytes_outlive_the_egress_endpoints() {
        let measured = budget::measure().await.expect("the host measures");
        let (dataplane, queue) = prepare(measured, MTU).expect("prepared");
        let Dataplane {
            mut admission,
            fixed,
            writer,
            terminals,
            opened,
            ..
        } = dataplane;
        let charged = admission.bytes_charged();
        let writer_bytes = tun_writer::footprint(MTU).expect("chargeable");
        let output = Output::new(
            MTU,
            Prepared {
                tuples: fixed.tuples,
                tracked: TERMINAL_DEPTH,
                opened,
            },
            writer,
        );
        {
            let mut fence = std::pin::pin!(release_fixed(
                output,
                vec![0u8; MTU],
                reassembly::Table::with_capacity(FRAGMENT_CONTEXTS),
                terminals,
                fixed,
                &mut admission,
                None,
            ));
            // Paused time advances only when everything else is parked, so reaching this arm means the fence
            // really is waiting rather than merely slow.
            tokio::select! {
                biased;
                () = &mut fence => panic!("released while the egress endpoints were still alive"),
                () = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            // The last thing the egress task drops, standing in for the egress task dropping it.
            drop(queue);
            (&mut fence).await;
        }
        assert_eq!(
            admission.outstanding_leases(),
            0,
            "every fixed lease is back once its allocation is gone"
        );
        assert!(
            admission.bytes_charged() + writer_bytes <= charged,
            "the writer's own bytes were among what came back"
        );
    }

    /// The virtual address these tests answer for, so a client's TCP connection to port 53 becomes the one
    /// flow kind a host can carry: it terminates locally and opens no socket on a `Network` no host has.
    const RESOLVER: &str = "192.0.2.53";

    /// A real dataplane on a socket pair: both daemon tasks running, and the client's end of the TUN.
    ///
    /// Everything in it is production - [run] and [crate::tun_writer::run] are the tasks under test, and what
    /// this adds is only the client and the session loop.
    struct Running {
        tun: tokio::net::UnixDatagram,
        configs: mpsc::Sender<Applied>,
        cancel: CancellationToken,
        ingress: tokio::task::JoinHandle<io::Result<()>>,
        egress: tokio::task::JoinHandle<io::Result<()>>,
    }

    impl Running {
        async fn start() -> Self {
            let measured = budget::measure().await.expect("the host measures");
            let (dataplane, queue) = prepare(measured, MTU).expect("prepared");

            let mut ends = [0 as libc::c_int; 2];
            // SAFETY: socketpair fills the two descriptors it is given and reads nothing else. Datagram,
            // because a TUN delivers whole packets and the ingress task reads one per call.
            assert_eq!(
                unsafe {
                    libc::socketpair(
                        libc::AF_UNIX,
                        libc::SOCK_DGRAM | libc::SOCK_NONBLOCK,
                        0,
                        ends.as_mut_ptr(),
                    )
                },
                0
            );
            // SAFETY: both descriptors were just created and are owned by nothing else.
            let (daemon, client_side) = unsafe {
                use std::os::fd::FromRawFd;
                (
                    OwnedFd::from_raw_fd(ends[0]),
                    std::os::unix::net::UnixDatagram::from_raw_fd(ends[1]),
                )
            };
            let tun = tokio::net::UnixDatagram::from_std(client_side).expect("pollable");
            let fd = Arc::new(AsyncFd::new(daemon).expect("pollable"));
            let cancel = CancellationToken::new();
            let egress = tokio::spawn(crate::tun_writer::run(
                Arc::clone(&fd),
                MTU,
                queue,
                cancel.clone(),
            ));
            let (configs, requests) = mpsc::channel(1);
            let ingress = tokio::spawn(run(fd, MTU, dataplane, requests, cancel.clone()));
            Self {
                tun,
                configs,
                cancel,
                ingress,
                egress,
            }
        }

        /// One config, applied the way the session loop applies one, and awaited to its acknowledgement.
        async fn apply(&self, admitting: bool, epoch: u64) {
            let (retired, acknowledged) = oneshot::channel();
            self.configs
                .send(Applied {
                    admitting,
                    upstream_generation: 1,
                    downstream_epoch: epoch,
                    egress: Egress {
                        selected_network: Some(1),
                        relay_upstream: None,
                    },
                    virtual_addresses: Arc::new(vec![RESOLVER.parse().expect("an address")]),
                    gateway_addresses: Vec::new(),
                    downstream_mtu_floor: MTU,
                    retired,
                })
                .await
                .expect("the ingress task is reading");
            acknowledged.await.expect("the config is acknowledged");
        }

        /// One client's SYN to the virtual resolver.
        fn syn(client: SocketAddr) -> Vec<u8> {
            crate::tcp::tests::segment(
                client,
                SocketAddr::new(RESOLVER.parse().expect("an address"), 53),
                TcpRepr {
                    src_port: client.port(),
                    dst_port: 53,
                    control: TcpControl::Syn,
                    seq_number: TcpSeqNumber(1_000),
                    ack_number: None,
                    window_len: 32_768,
                    window_scale: None,
                    max_seg_size: Some(1_400),
                    sack_permitted: false,
                    sack_ranges: [None; 3],
                    timestamp: None,
                    payload: &[],
                },
            )
        }

        /// Ends the session and joins both tasks, answering with the client's end so whatever the daemon
        /// wrote before it stopped can still be read off it.
        async fn stop(self) -> tokio::net::UnixDatagram {
            self.cancel.cancel();
            drop(self.configs);
            self.ingress
                .await
                .expect("joined")
                .expect("a clean session");
            self.egress.await.expect("joined").expect("a clean writer");
            self.tun
        }
    }

    /// The outer idle floor fires through the owner loop's own deadline arm, not just through the engine.
    ///
    /// The wiring this pins is three lines in [run]: the combined deadline, the poll, and the expiry that
    /// follows it. Deleting the `tcp.expire` call leaves the arm waking and doing nothing, and the reset below
    /// never arrives.
    ///
    /// The clock is the runtime's, which is why this is possible at all without a four-minute wait: every
    /// deadline this task computes comes from [now], and [now] is the same clock `sleep_until` measures
    /// against - so freezing it and stepping once past the floor is exactly what the passage of four minutes
    /// would do. The flow is left at `SYN-RECEIVED`, whose floor is the transitory four minutes.
    #[tokio::test]
    async fn an_idle_floor_fires_through_the_owner_loops_own_deadline_arm() {
        let session = Running::start().await;
        session.apply(true, 1).await;

        let client = crate::tcp::tests::client(11_100);
        let mut answer = vec![0u8; MTU];
        session
            .tun
            .send(&Running::syn(client))
            .await
            .expect("the TUN takes it");
        let read = session
            .tun
            .recv(&mut answer)
            .await
            .expect("the stack answers");
        assert_eq!(
            crate::tcp::tests::parse(&answer[..read]).control,
            TcpControl::Syn,
            "a SYN-ACK, so the flow exists and is half open"
        );

        // One step past the transitory floor that half-open flow is sitting on. Nothing else in the daemon
        // has a deadline this near, so the arm that wakes is the TCP one.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(241)).await;
        tokio::time::resume();

        let read = tokio::time::timeout(Duration::from_secs(5), session.tun.recv(&mut answer))
            .await
            .expect("the floor is reached and the owner loop acts on it")
            .expect("a packet");
        let terminal = crate::tcp::tests::parse(&answer[..read]);
        assert_eq!(terminal.control, TcpControl::Rst, "the client is told");
        assert_eq!(terminal.destination, client);

        session.stop().await;
    }

    /// A session that stops serving acknowledges at once and still lets the state it already had be
    /// retired - through the real owner loop rather than a copy of its select.
    ///
    /// Everything here is production: [run] is the task under test, the TUN is a socket pair the real
    /// [crate::tun_writer::run] writes back to, and the test is only the client and the session loop.
    ///
    /// The reset assertion is about ordering inside that loop, and it is observable because of the writer's
    /// dequeue gate: a retirement's reset is written under the *successor* stamp, so if the config arm built
    /// that packet before adopting the new stamp it would be purged on the way out and never reach the
    /// client. One reset on the wire, for the connection that existed, is that ordering.
    ///
    /// One thing this deliberately does *not* try to prove, because this boundary cannot: that a client
    /// arriving *after* the stop is refused. The loop's select is biased with TUN input
    /// last, and a refusal produces nothing, so from outside there is no way to tell a packet dropped by the
    /// admission check from one the loop had not read yet - a run of this test showed the second client's
    /// SYN still unread at cancellation, which would have made any assertion about it vacuous. That check is
    /// this task's own pre-existing `admitting` gate rather than anything the idle floors changed; what they
    /// did add for `STOPPING` - a payload that drains without refreshing a lifetime, and a resolver exchange
    /// that is refused rather than started - is proved in [crate::tcp::lifetime] and [crate::tcp::dns].
    ///
    /// A DNS-over-TCP destination rather than an ordinary one, because that is the flow kind a host can
    /// carry: it terminates locally and opens no socket on a `Network` no host has.
    #[tokio::test]
    async fn stopping_acknowledges_and_still_retires_what_exists() {
        let session = Running::start().await;

        session.apply(true, 1).await;

        // One client connection, opened the way a client opens one.
        let client = crate::tcp::tests::client(11_000);
        let mut answer = vec![0u8; MTU];
        let mut written = Vec::new();
        session
            .tun
            .send(&Running::syn(client))
            .await
            .expect("the TUN takes it");
        let read = session
            .tun
            .recv(&mut answer)
            .await
            .expect("the stack answers");
        written.push(crate::tcp::tests::parse(&answer[..read]));
        assert_eq!(written[0].control, TcpControl::Syn, "a SYN-ACK");
        assert_eq!(written[0].destination, client);

        // STOPPING, on the same stamp so this config retires nothing by itself.
        session.apply(false, 1).await;

        // A downstream epoch change while still stopping: a stopping session drains and retires what it
        // already holds, and the first connection is exactly that.
        session.apply(false, 2).await;
        loop {
            let read = tokio::time::timeout(Duration::from_secs(5), session.tun.recv(&mut answer))
                .await
                .expect("the retirement reaches the client")
                .expect("a packet");
            let packet = crate::tcp::tests::parse(&answer[..read]);
            let terminal = packet.control == TcpControl::Rst;
            written.push(packet);
            if terminal {
                break;
            }
        }

        // Everything else the daemon ever put on the wire, taken once both its tasks have finished, so this
        // is the whole of it rather than whatever had arrived by some deadline.
        let tun = session.stop().await;
        while let Ok(read) = tun.try_recv(&mut answer) {
            written.push(crate::tcp::tests::parse(&answer[..read]));
        }

        let terminals: Vec<_> = written
            .iter()
            .filter(|packet| packet.control == TcpControl::Rst)
            .collect();
        assert_eq!(
            terminals.len(),
            1,
            "one terminal reset, and only one - so it was not purged by its own stamp"
        );
        assert_eq!(
            terminals[0].destination, client,
            "for the connection the stopping session was still holding"
        );
    }
}
