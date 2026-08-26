//! The TUN ingress task, and with it the owner of every piece of client-keyed dataplane state.
//!
//! It reads one packet at a time from the descriptor the app transferred, classifies it, and dispatches it
//! to the transport that owns it. Ownership lands here rather than in a task per protocol because this is
//! the only reader of that descriptor: the state a packet touches is reached without a lock, and there is no
//! per-packet copy or channel hop on the way in.
//!
//! It also owns every task those transports started, through [crate::shizuku::workers]. A worker that
//! finishes - because it failed, because its peer went, or because a retirement asked it to - is joined here,
//! and only then may its record be removed and its budget refunded. That is what makes an acknowledged epoch
//! mean the descriptors are actually back rather than merely spoken for, and it is why this loop selects on
//! the transports' terminals alongside their traffic.
//!
//! Joined is not always the same as retired. A terminating TCP flow whose worker finished *cleanly* while its
//! client was still closing keeps its client socket and its charge - the descriptor is gone with the task, and
//! what is left is a teardown this loop lets finish. Such a flow is settled by the owner's own scan rather
//! than by a terminal; see [crate::shizuku::tcp::Engine::finished].
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
use std::net::{IpAddr, SocketAddr};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};
use vpnhotspotd::shared::reassembly;

use crate::shizuku::budget::MAX_DATAGRAM;
use vpnhotspotd::shared::admission::{
    largest_fitting, Admission, Class, Denied, Headroom, Lease, Request,
};

use crate::report;
use crate::shizuku::budget::Measured;
use crate::shizuku::dispatch::{Counters, Dispatch};
use crate::shizuku::echo;
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::tcp;
use vpnhotspotd::shared::ipv4_identification::{Ipv4Identifications, Prepared, Terminal};

use crate::shizuku::tun_writer::{self, Stamp, TERMINAL_DEPTH};
use crate::shizuku::udp;
use crate::shizuku::virtual_dns;
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
/// saying which. [prepare] is the only production caller of [crate::shizuku::tun_writer::channel], and it calls it on
/// the far side of the reserve that pays for it; this bundle is what it hands out, so no production code
/// needs a writer or a queue from anywhere else. There is one production path and it reserves before
/// constructing the channels.
pub(crate) struct Dataplane {
    admission: Admission,
    fixed: Fixed,
    terminals: mpsc::Receiver<Terminal>,
    /// The packet emitter and the writer handle it enqueues through. Built here rather than in [run] because
    /// it holds the writer half of the reserved channels, and the failure fence below needs to drop it.
    output: Output,
    buffer: Vec<u8>,
    fragments: reassembly::Table,
    /// The four traffic owners, every one of which can be denied. They are constructed here, before the ACK,
    /// which is the whole point of this bundle: a denial is a start that failed rather than a session the app
    /// was told was ready.
    relay: udp::Relay,
    events: mpsc::Receiver<crate::shizuku::reply::Event<SocketAddr>>,
    echo: echo::Relay,
    echoes: mpsc::Receiver<crate::shizuku::reply::Event<crate::shizuku::echo_socket::Family>>,
    dns: virtual_dns::Handoff,
    tcp: tcp::Engine,
    flows: mpsc::Receiver<crate::shizuku::tcp_flow::Event>,
    asking: mpsc::Receiver<crate::shizuku::tcp_dns::Ask>,
}

/// What a seed failure is called when the app is told about it.
const SEED_CONTEXT: &str = "shizuku.tcp_seed";

/// One 64-bit seed for this session's client-side TCP stack, from whatever `fill` puts in the eight bytes.
///
/// Split from the syscall below so that the two things that can be wrong with a seed are decided in one
/// place. Both are refusals rather than repairs:
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
/// A failure is *described* rather than emitted here. It happens before the start call has been
/// acknowledged, so the session it refuses is one the app is still waiting on, and
/// [crate::shizuku::app_session] answers that call with exactly this report - errno, context and location
/// included. Emitting it here as well would put one entropy failure in front of the user twice, once as a
/// nonfatal and once as the start call's error.
fn session_seed(fill: impl FnOnce(&mut [u8; 8]) -> io::Result<usize>) -> io::Result<u64> {
    seed_from(fill).map_err(|failed| failed.with_report_context(SEED_CONTEXT))
}

/// The kernel's eight bytes, for the one caller that wants a real seed rather than a decided one.
fn kernel_seed_bytes(bytes: &mut [u8; 8]) -> io::Result<usize> {
    rustix::rand::getrandom(bytes, rustix::rand::GetRandomFlags::empty()).map_err(io::Error::from)
}

/// Reserves everything fixed, and only then builds every owner those reservations pay for.
///
/// All of it, and before either dataplane task exists. That is the readiness boundary: the start call is
/// acknowledged on this returning, so every fallible step a usable session needs happens here rather than
/// inside [run], and a denial can no longer be answered as ready and only then terminate. [run] receives a
/// dataplane that already fits, which is why it constructs nothing.
///
/// Fails having left nothing allocated and uncharged: the reserves taken before a failing step are given
/// back through the same fence a running session ends by.
pub(crate) async fn prepare(
    measured: Measured,
    mtu: usize,
) -> io::Result<(Dataplane, tun_writer::Queue)> {
    let opened = Instant::now();
    // Before any *dataplane* owner is charged or allocated, because it is the one thing here the accounting
    // has no opinion about: a session whose TCP stack cannot be seeded does not start. Not before
    // everything - the control writer and the reporter are already up, deliberately, which is what lets the
    // refusal below reach the app as this call's structured error rather than as a closed socket.
    let seed = session_seed(kernel_seed_bytes)?;
    // The one admission owner, built here and never shared: every owner below reaches it through the ingress
    // task, which is the only thing that reads client traffic. A worker never sees it at all.
    let mut admission = Admission::new(measured.totals)
        .map_err(io::Error::other)
        .with_report_context("shizuku.dataplane.admission")?;
    // Every fixed owner is charged before a single byte of it is allocated, and each failure is a session
    // that does not start rather than a denial blamed on traffic later.
    let fixed = reserve_fixed(&mut admission, mtu)
        .map_err(|why| {
            io::Error::other(format!(
                "the dataplane's fixed reservations do not fit: {why:?}"
            ))
        })
        .with_report_context("shizuku.dataplane.reserve_fixed")?;
    // Reserved above, allocated here, and in that order deliberately.
    let (writer, queue, terminals) = tun_writer::channel();
    let output = Output::new(
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
    // one MTU is the largest packet the interface can deliver, so a full read is never truncated
    let buffer = vec![0u8; mtu];
    let fragments = reassembly::Table::with_capacity(FRAGMENT_CONTEXTS);
    let owners = (|| {
        let (relay, events) = udp::Relay::new(&mut admission)?;
        let (echo, echoes) = echo::Relay::new(&mut admission)?;
        let dns = virtual_dns::Handoff::new(&mut admission)?;
        let (tcp, flows, asking) = tcp::Engine::new(mtu, seed, &mut admission)?;
        Ok::<_, Denied>((relay, events, echo, echoes, dns, tcp, flows, asking))
    })();
    let (relay, events, echo, echoes, dns, tcp, flows, asking) = match owners {
        Ok(owners) => owners,
        Err(why) => {
            // Through the same fence as a running session's exit rather than a bare return. No traffic owner
            // exists to stop, but the writer's channels and the Identification table do, and they are
            // charged: returning here would give their bytes back while they were still allocated, or -
            // worse - never give them back at all and never say so.
            // Named here, at the constructor set that was denied, so a start refused for want of budget
            // says which half of the dataplane did not fit rather than reaching the app as a session that
            // failed at teardown for no stated reason.
            let failed = io::Error::other(format!(
                "the dataplane's owners do not fit the measured totals: {why:?}"
            ))
            .with_report_context("shizuku.dataplane.owners");
            // First, because the queue holds the settlement channel's sending half and the fence below waits
            // for exactly that half to go. Nothing has been spawned yet, so this frame is its only owner.
            drop(queue);
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
                "dataplane startup failed after {} lease(s) were taken; owners built before the \
                 failure were dropped rather than released, so they are counted below",
                admission.outstanding_leases()
            );
            report::stdout!("admission {}", admission.describe());
            return Err(failed);
        }
    };
    Ok((
        Dataplane {
            admission,
            fixed,
            terminals,
            output,
            buffer,
            fragments,
            relay,
            events,
            echo,
            echoes,
            dns,
            tcp,
            flows,
            asking,
        },
        queue,
    ))
}

pub(crate) async fn run(
    fd: Arc<AsyncFd<OwnedFd>>,
    dataplane: Dataplane,
    mut configs: mpsc::Receiver<Applied>,
    cancel: CancellationToken,
) -> io::Result<()> {
    // Nothing is constructed here, and nothing here can fail before the loop: [prepare] owns every fallible
    // step, so a task that exists at all is one whose dataplane already fits.
    let Dataplane {
        mut admission,
        fixed,
        mut terminals,
        mut output,
        mut buffer,
        mut fragments,
        mut relay,
        mut events,
        mut echo,
        mut echoes,
        mut dns,
        mut tcp,
        mut flows,
        mut asking,
    } = dataplane;
    let mut counters = Counters::default();
    let mut admitting = false;
    let mut stamp = Stamp::default();
    let mut virtual_addresses = Arc::new(Vec::new());
    let mut gateways = Gateways::new();
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
                        break Err(e.with_report_context("shizuku.tun_ingress.retire"));
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
                    break Err(io::Error::other("the session abandoned a config it sent")
                        .with_report_context("shizuku.tun_ingress.acknowledge"));
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
        // Both of the descriptor's own failures are described where they happen. Attached rather than
        // emitted: this ends the session, so the report travels out on the error and the session decides its
        // one destination - see [crate::shizuku::app_session]. Describing them only at teardown would name
        // that teardown and its source line, which says nothing about which half of the ingress descriptor
        // stopped working, and would drop nothing of the errno but everything of where it came from.
        let mut guard = match readable {
            Ok(guard) => guard,
            Err(e) => break Err(e.with_report_context("shizuku.tun_ingress.readable")),
        };
        let read = match guard.try_io(|inner| {
            rustix::io::read(inner.get_ref(), buffer.as_mut_slice()).map_err(io::Error::from)
        }) {
            Ok(Ok(read)) => read,
            Ok(Err(e)) => break Err(e.with_report_context("shizuku.tun_ingress.read")),
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
    // releases covers the channel and every payload its slots may hold - see [crate::shizuku::udp::Relay::release].
    relay.release(events, &mut admission);
    echo.release(echoes, &mut admission);
    dns.release(&mut admission);
    // The engine's fixed lease pays for the readiness and ask channels, so the ingress task's halves of both
    // go with it - see [crate::shizuku::tcp::Engine::release].
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
///   those bytes were reserved for, and the only [tun_writer::Writer] in the process, so the writer's packet and
///   retirement senders die with it.
/// - The egress task then finds its packet channel closed - or, if it is parked waiting for a kernel that
///   will not take a write, finds its retirement channel closed, which preempts it exactly as a real
///   retirement would.
/// - That task's last act is to drop its settlement sender, so draining this receiver to `None` is proof
///   that the queue, both receivers and any packet it had in hand are gone. It is the channel the writer's
///   own reservation already paid for, so nothing here needs a second handshake to say the same thing.
///   [prepare]'s own failure path has no such task, and drops the queue itself before calling this for the
///   same reason: what the drain waits for is that sending half going away, whoever holds it.
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
    // [crate::shizuku::tun_writer::footprint], which is the equation this reserve and that construction share.
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
/// measured against a moment the sleep never sees.
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
