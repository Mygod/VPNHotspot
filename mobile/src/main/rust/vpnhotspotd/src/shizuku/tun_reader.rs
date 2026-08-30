//! Owns client-keyed dataplane state and applies admission updates.
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
use vpnhotspotd::shared::dns_debt;

use crate::report;
use crate::shizuku::budget::Measured;
use crate::shizuku::dispatch::{Counters, Dispatch};
use crate::shizuku::echo;
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::tcp;
use vpnhotspotd::shared::ipv4_identification::{Ipv4Identifications, Prepared, Terminal};

use crate::shizuku::tun_writer::{self, TERMINAL_DEPTH};
use crate::shizuku::udp;
use crate::shizuku::virtual_dns;

pub(crate) struct Applied {
    pub(crate) admitting: bool,
    pub(crate) applied: oneshot::Sender<()>,
}

// Bounds the reassembly table itself; the admission ledger separately bounds fragment bytes.
const FRAGMENT_CONTEXTS: usize = 256;

// Identification metadata may consume at most one sixteenth of remaining general bytes.
const IDENTIFICATION_SHARE: u64 = 16;

pub(crate) struct Dataplane {
    admission: Admission,
    fixed: Fixed,
    terminals: mpsc::Receiver<Terminal>,
    output: Output,
    buffer: Vec<u8>,
    fragments: reassembly::Table,
    relay: udp::Relay,
    events: mpsc::Receiver<crate::shizuku::reply::Event<SocketAddr>>,
    echo: echo::Relay,
    echoes: mpsc::Receiver<crate::shizuku::reply::Event<crate::shizuku::echo_socket::Family>>,
    dns: virtual_dns::Handoff,
    tcp: tcp::Engine,
    asking: mpsc::Receiver<crate::shizuku::tcp_dns::Ask>,
}

pub(crate) async fn prepare(
    measured: Measured,
    mtu: usize,
) -> io::Result<(Dataplane, tun_writer::Queue)> {
    let opened = Instant::now();
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
    let mut admission = Admission::new(measured.totals)
        .map_err(io::Error::other)
        .with_report_context("shizuku.dataplane.admission")?;
    let fixed = reserve_fixed(&mut admission, mtu)
        .map_err(|why| {
            io::Error::other(format!(
                "the dataplane's fixed reservations do not fit: {why:?}"
            ))
        })
        .with_report_context("shizuku.dataplane.reserve_fixed")?;
    // Fixed owners are reserved before their queues, buffers, and tables are allocated.
    let (writer, queue, terminals) = tun_writer::channel();
    let output = Output::new(
        mtu,
        Prepared {
            tuples: fixed.tuples,
            tracked: TERMINAL_DEPTH,
            opened,
        },
        writer,
    );
    let buffer = vec![0u8; mtu];
    let fragments = reassembly::Table::with_capacity(FRAGMENT_CONTEXTS);
    let owners = (|| {
        let (relay, events) = udp::Relay::new(&mut admission)?;
        let (echo, echoes) = echo::Relay::new(&mut admission)?;
        let dns = virtual_dns::Handoff::new(&mut admission)?;
        let (tcp, asking) = tcp::Engine::new(mtu, seed, &mut admission)?;
        Ok::<_, Denied>((relay, events, echo, echoes, dns, tcp, asking))
    })();
    let (relay, events, echo, echoes, dns, tcp, asking) = match owners {
        Ok(owners) => owners,
        Err(why) => {
            let failed = io::Error::other(format!(
                "the dataplane's owners do not fit the measured totals: {why:?}"
            ))
            .with_report_context("shizuku.dataplane.owners");
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
        mut asking,
    } = dataplane;
    let mut counters = Counters::default();
    let mut admitting = false;
    let gateways = Gateways::new(gateway_addresses);
    let mut result = loop {
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
                admitting = config.admitting;
                if config.applied.send(()).is_err() {
                    break Err(io::Error::other("the session abandoned a config it sent")
                        .with_report_context("shizuku.tun_ingress.acknowledge"));
                }
                continue;
            }
            terminal = terminals.recv() => {
                match terminal {
                    Some(terminal) => output.terminal(terminal),
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
            attention = tcp.attention() => {
                match attention {
                    tcp::Attention::Flow(terminal) => tcp.close(terminal, &mut admission, &mut output),
                    tcp::Attention::Transaction(terminal) => {
                        if let Err(e) = tcp.settle(terminal, &mut admission) {
                            break Err(e);
                        }
                    }
                    tcp::Attention::ClientClosed {
                        handle,
                        incarnation,
                    } => tcp.finish_client_close(handle, incarnation, &mut admission),
                    tcp::Attention::Traffic => tcp.traffic(admitting, now(), &mut output),
                }
                continue;
            }
            settled = dns.settled() => {
                match settled {
                    virtual_dns::Settled::Terminal(terminal) => {
                        if let Err(e) = dns.settle(terminal, &mut output, &mut admission) {
                            break Err(e);
                        }
                    }
                    virtual_dns::Settled::Ending(e) => break Err(e),
                }
                continue;
            }
            event = events.recv() => {
                match event {
                    Some(event) => relay.handle(event, &mut output),
                    None => break Ok(()),
                }
                continue;
            }
            event = echoes.recv() => {
                match event {
                    Some(event) => echo.handle(event, &mut output, &mut admission),
                    None => break Ok(()),
                }
                continue;
            }
            ask = asking.recv() => {
                match ask {
                    Some(ask) => {
                        if let Err(e) = tcp.ask(ask, admitting, &mut admission) {
                            break Err(e);
                        }
                    }
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
                    virtual_addresses: &virtual_addresses,
                }.expire(now());
                continue;
            }
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
        if let Err(e) = (Dispatch {
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
            virtual_addresses: &virtual_addresses,
        })
        .accept(&buffer[..read], now())
        {
            break Err(e);
        }
    };
    relay.shutdown(&mut admission).await;
    echo.shutdown(&mut admission).await;
    // Drain owners before releasing their leases; keep the first failure and report later ones.
    result = report::keep_first(
        "shizuku.tun_ingress.tcp_shutdown",
        result,
        tcp.shutdown(&mut admission, &mut output).await,
    );
    result = report::keep_first(
        "shizuku.tun_ingress.dns_shutdown",
        result,
        dns.shutdown(&mut output, &mut admission).await,
    );
    report::stdout!("tun ingress {}", counters.describe());
    report_owners(&relay, &echo, &fragments, &dns, &tcp, &output, &admission);
    relay.release(events, &mut admission);
    echo.release(echoes, &mut admission);
    dns.release(&mut admission);
    tcp.release(asking, &mut admission);
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
        cancel.cancel();
    }
    while terminals.recv().await.is_some() {}
    drop(terminals);
    fixed.release(admission);
}

struct Fixed {
    writer_queue: Lease,
    scratch: Lease,
    fragments: Lease,
    identifications: Lease,
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
    // Charge the TUN writer queue and input buffer in one fixed-I/O lease, avoiding another byte-only owner.
    let io = tun_writer::footprint(mtu)
        .and_then(|queue| dns_debt::fixed_io(queue, mtu as u64))
        .ok_or(Denied::Arithmetic)?;
    let writer_queue = admission.reserve(io)?;
    // Reserve one datagram plus one output fragment for packetization.
    let scratch = admission.reserve(Request::bytes(2 * MAX_DATAGRAM as u64, Class::Reserved))?;
    let fragments = reassembly::Table::footprint(FRAGMENT_CONTEXTS)
        .ok_or(Denied::Arithmetic)
        .and_then(|bytes| admission.reserve(Request::bytes(bytes, Class::General)))?;
    let headroom = admission.general_headroom();
    let tuples = largest_fitting(
        Headroom {
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

fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

async fn sleep_until_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}
