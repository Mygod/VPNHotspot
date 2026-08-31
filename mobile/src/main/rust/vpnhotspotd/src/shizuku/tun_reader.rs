//! Owns client-keyed dataplane state and applies configuration updates.
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep_until;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::ipv4_identification::Terminal;
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};
use vpnhotspotd::shared::reassembly;
use vpnhotspotd::shared::tun_handoff;

use crate::report;
use crate::shizuku::dispatch::{Counters, Dispatch};
use crate::shizuku::echo;
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::tcp;

use crate::shizuku::udp;
use crate::shizuku::virtual_dns;
use vpnhotspotd::shared::turn::{Expiry, Pass, Source};

pub(crate) struct Applied {
    pub(crate) admitting: bool,
    pub(crate) applied: oneshot::Sender<()>,
}

pub(crate) struct Dataplane {
    output: Output,
    buffer: Vec<u8>,
    fragments: reassembly::Table,
    relay: udp::Relay,
    events: mpsc::Receiver<crate::shizuku::reply::Event<SocketAddr>>,
    echo: echo::Relay,
    echoes: mpsc::Receiver<crate::shizuku::reply::Event<crate::shizuku::echo_socket::Family>>,
    /// Guarded-datagram endings the serial TUN writer sends back, which is what lets an IPv4 Identification
    /// tuple's sequence start again once its last fragment has aged out.
    settled: mpsc::Receiver<Terminal>,
    dns: virtual_dns::Handoff,
    tcp: tcp::Engine,
    asking: mpsc::UnboundedReceiver<crate::shizuku::tcp_dns::Ask>,
}

pub(crate) fn prepare(mtu: usize) -> io::Result<(Dataplane, tun_handoff::Queue)> {
    let mut seed_bytes = [0u8; 8];
    let filled = rustix::rand::getrandom(&mut seed_bytes, rustix::rand::GetRandomFlags::empty())
        .map_err(io::Error::from)
        .with_report_context("shizuku.tcp_seed")?;
    if filled != seed_bytes.len() {
        return Err(io::Error::other(format!(
            "{filled} of {} seed bytes for this session's TCP stack",
            seed_bytes.len()
        )))
        .with_report_context("shizuku.tcp_seed");
    }
    // Refuse smoltcp's zero default for passive-open sequence numbers.
    let seed = match u64::from_ne_bytes(seed_bytes) {
        0 => {
            return Err(io::Error::other(
                "the seed for this session's TCP stack came out zero",
            ))
            .with_report_context("shizuku.tcp_seed");
        }
        seed => seed,
    };
    let (writer, queue, settled) = tun_handoff::channel();
    // The Identification allocator's opening reuse quarantine is measured from here, which is the earliest
    // moment this session could put anything on the wire.
    let output = Output::new(mtu, now(), writer);
    let buffer = vec![0u8; mtu];
    let fragments = reassembly::Table::new();
    let (relay, events) = udp::Relay::new();
    let (echo, echoes) = echo::Relay::new();
    let dns = virtual_dns::Handoff::new();
    let (tcp, asking) = tcp::Engine::new(mtu, seed);
    Ok((
        Dataplane {
            output,
            buffer,
            fragments,
            relay,
            events,
            echo,
            echoes,
            settled,
            dns,
            tcp,
            asking,
        },
        queue,
    ))
}

pub(crate) async fn run(
    fd: Arc<AsyncFd<OwnedFd>>,
    dataplane: Dataplane,
    virtual_addresses: Arc<Vec<IpAddr>>,
    gateway_addresses: Vec<IpAddr>,
    mut configs: mpsc::Receiver<Applied>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let Dataplane {
        mut output,
        mut buffer,
        mut fragments,
        mut relay,
        mut events,
        mut echo,
        mut echoes,
        mut settled,
        mut dns,
        mut tcp,
        mut asking,
    } = dataplane;
    let mut counters = Counters::default();
    let mut admitting = false;
    let gateways = Gateways::new(gateway_addresses);
    // Meter ordinary dataplane sources. Cancellation stays first; configuration is intentionally prioritized,
    // while completion arms are bounded by work produced by metered sources. See `shared::turn`.
    let mut pass = Pass::default();
    let mut result = 'owner: loop {
        // Cache owner deadlines across the inner pass-reset retry.
        let mapping_deadline = relay.next_deadline();
        let echo_deadline = echo.next_deadline();
        let fragment_deadline = fragments.next_deadline();
        // One snapshot controls every output gate and the matching capacity wait.
        let accepting = output.accepting();
        // Maintenance cannot consume capacity that returns after this snapshot.
        let expiry = Expiry::from(accepting);
        let tcp_deadline = tcp.next_deadline(accepting);
        let readable = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break 'owner Ok(()),
                // Settlement precedes every arm that could leave its writer blocked.
                terminal = settled.recv() => {
                    match terminal {
                        Some(terminal) => output.settle(terminal),
                        // The writer task is gone, which already ends this session through its own result.
                        None => break 'owner Ok(()),
                    }
                    continue 'owner;
                }
                config = configs.recv() => {
                    let Some(config) = config else {
                        break 'owner Ok(());
                    };
                    admitting = config.admitting;
                    if config.applied.send(()).is_err() {
                        break 'owner Err(io::Error::other("the session abandoned a config it sent")
                            .with_report_context("shizuku.tun_ingress.acknowledge"));
                    }
                    continue 'owner;
                }
                terminal = relay.finished() => {
                    relay.close(terminal);
                    continue 'owner;
                }
                terminal = echo.finished() => {
                    echo.closed(terminal);
                    continue 'owner;
                }
                // Bridge traffic can keep this combined attention source continuously ready.
                attention = tcp.attention(), if pass.owed(Source::TcpAttention) => {
                    pass.take(Source::TcpAttention);
                    match attention {
                        tcp::Attention::Flow(terminal) => tcp.close(terminal, &mut output),
                        tcp::Attention::Transaction(terminal) => {
                            if let Err(e) = tcp.settle(terminal) {
                                break 'owner Err(e);
                            }
                        }
                        tcp::Attention::ClientClosed {
                            handle,
                            incarnation,
                        } => tcp.finish_client_close(handle, incarnation),
                        tcp::Attention::Traffic => tcp.traffic(admitting, now(), &mut output),
                    }
                    continue 'owner;
                }
                answered = dns.settled() => {
                    match answered {
                        virtual_dns::Settled::Terminal(terminal) => {
                            if let Err(e) = dns.settle(terminal, now(), &mut output) {
                                break 'owner Err(e);
                            }
                        }
                        virtual_dns::Settled::Ending(e) => break 'owner Err(e),
                    }
                    continue 'owner;
                }
                // Leave replies queued while there is no output capacity.
                event = events.recv(), if accepting && pass.owed(Source::UdpReply) => {
                    pass.take(Source::UdpReply);
                    match event {
                        Some(event) => relay.handle(event, now(), &mut output),
                        None => break 'owner Ok(()),
                    }
                    continue 'owner;
                }
                event = echoes.recv(), if accepting && pass.owed(Source::EchoReply) => {
                    pass.take(Source::EchoReply);
                    match event {
                        Some(event) => echo.handle(event, now(), &mut output),
                        None => break 'owner Ok(()),
                    }
                    continue 'owner;
                }
                ask = asking.recv(), if pass.owed(Source::TcpDnsAsk) => {
                    pass.take(Source::TcpDnsAsk);
                    match ask {
                        Some(ask) => {
                            if let Err(e) = tcp.ask(ask, admitting) {
                                break 'owner Err(e);
                            }
                        }
                        None => break 'owner Ok(()),
                    }
                    continue 'owner;
                }
                // During a stall, recurring deadlines maintain state without taking a pass turn or emitting.
                // This preserves later deadlines and the interrupted producer order; see [Expiry].
                () = sleep_until_deadline(mapping_deadline),
                    if !accepting || pass.owed(Source::MappingDeadline) =>
                {
                    if accepting {
                        pass.take(Source::MappingDeadline);
                    }
                    relay.sweep();
                    continue 'owner;
                }
                () = sleep_until_deadline(echo_deadline),
                    if !accepting || pass.owed(Source::EchoDeadline) =>
                {
                    if accepting {
                        pass.take(Source::EchoDeadline);
                    }
                    echo.sweep();
                    continue 'owner;
                }
                () = sleep_until_deadline(fragment_deadline),
                    if !accepting || pass.owed(Source::FragmentDeadline) =>
                {
                    if accepting {
                        pass.take(Source::FragmentDeadline);
                    }
                    Dispatch {
                        counters: &mut counters,
                        relay: &mut relay,
                        echo: &mut echo,
                        dns: &mut dns,
                        tcp: &mut tcp,
                        fragments: &mut fragments,
                        output: &mut output,
                        gateways: &gateways,
                        virtual_addresses: &virtual_addresses,
                    }.expire(now(), expiry);
                    continue 'owner;
                }
                // Under a stall only idle expiry is scheduled. Abort now; poll its reset only on a metered
                // delivering turn.
                () = sleep_until_deadline(tcp_deadline),
                    if !accepting || pass.owed(Source::TcpDeadline) =>
                {
                    if accepting {
                        pass.take(Source::TcpDeadline);
                    }
                    tcp.expire(now());
                    if expiry.delivering() {
                        tcp.poll(&mut output);
                    }
                    continue 'owner;
                }
                // TCP ingress settles synchronously; without output capacity its device packet has no wake.
                readable = fd.readable(), if accepting && pass.owed(Source::TunIngress) => {
                    pass.take(Source::TunIngress);
                    break readable;
                }
                // Capacity return restarts arbitration without emitting or resetting the interrupted pass.
                // This prevents TCP from taking every released slot.
                () = output.accepted(), if !accepting => continue 'owner,
                // Reset only with producers enabled; retry in place to retain cached deadlines.
                () = std::future::ready(()), if pass.started(accepting) => pass.end(),
            }
        };
        let mut guard = match readable {
            Ok(guard) => guard,
            Err(e) => break Err(e.with_report_context("shizuku.tun_ingress.readable")),
        };
        let read = match guard.try_io(|inner| {
            rustix::io::read(inner.get_ref(), buffer.as_mut_slice()).map_err(io::Error::from)
        }) {
            Ok(Ok(read)) => read,
            Ok(Err(e)) => break Err(e.with_report_context("shizuku.tun_ingress.read")),
            Err(_would_block) => continue,
        };
        if read == 0 {
            continue;
        }
        if !admitting {
            counters.unadmitted += 1;
            continue;
        }
        // This sole producer has not consumed the capacity that admitted the read.
        debug_assert!(
            output.accepting(),
            "a client packet was read without interface capacity to settle it"
        );
        if let Err(e) = (Dispatch {
            counters: &mut counters,
            relay: &mut relay,
            echo: &mut echo,
            dns: &mut dns,
            tcp: &mut tcp,
            fragments: &mut fragments,
            output: &mut output,
            gateways: &gateways,
            virtual_addresses: &virtual_addresses,
        })
        .accept(&buffer[..read], now())
        {
            break Err(e);
        }
    };
    relay.shutdown().await;
    echo.shutdown().await;
    // Drain owners before releasing them; keep the first failure and report later ones.
    result = report::keep_first(
        "shizuku.tun_ingress.tcp_shutdown",
        result,
        tcp.shutdown(&mut output).await,
    );
    result = report::keep_first(
        "shizuku.tun_ingress.dns_shutdown",
        result,
        dns.shutdown(now(), &mut output).await,
    );
    report::stdout!("tun ingress {}", counters.describe());
    report_owners(&relay, &echo, &fragments, &dns, &tcp, &output);
    relay.release(events);
    echo.release(echoes);
    // Explicitly, and here: this is the fence a writer still waiting to hand an ending back is released by,
    // and nothing above it needs the writer to have finished.
    drop(settled);
    dns.release();
    tcp.release(asking);
    fragments.retire();
    drop(output);
    drop(buffer);
    drop(fragments);
    cancel.cancel();
    result
}

fn report_owners(
    relay: &udp::Relay,
    echo: &echo::Relay,
    fragments: &reassembly::Table,
    dns: &virtual_dns::Handoff,
    tcp: &tcp::Engine,
    output: &Output,
) {
    report::stdout!("udp relay {}", relay.describe());
    report::stdout!("echo relay {}", echo.describe());
    report::stdout!("reassembly {}", fragments.describe());
    report::stdout!("virtual dns {}", dns.describe());
    report::stdout!("tcp engine {}", tcp.describe());
    report::stdout!("tun output {}", output.describe());
}

/// The one clock this session dates everything by, including the wire times its writer reports.
pub(crate) fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

async fn sleep_until_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}
