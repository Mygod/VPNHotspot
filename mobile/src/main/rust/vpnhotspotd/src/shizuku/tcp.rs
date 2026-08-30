//! Client-side TCP termination backed by independently connected upstream sockets or virtual DNS.
use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle};
use smoltcp::socket::tcp::Socket;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::workers::{Ended, Identity, Workers};

use crate::shizuku::flow_setup;
use vpnhotspotd::shared::admission::{Admission, Class, Lease};
use vpnhotspotd::shared::bridge::{Bridge, Worker};
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::flow::{self, FlowId, Turns};
use vpnhotspotd::shared::flow_budget;
use vpnhotspotd::shared::ingress as boundary;

use crate::report;
use crate::shizuku::output::Output;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_device::Shim;
use crate::shizuku::tcp_dns::{self, Serving, Transactions};
use crate::shizuku::tcp_flow;

mod bridge;
mod dns;
mod ingress;
mod lifetime;
mod terminal;

pub(crate) use bridge::Attention;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Resolver,
    Upstream,
}

struct Flow {
    client: SocketAddr,
    destination: SocketAddr,
    /// Present exactly while this flow's upstream descriptor exists. Virtual-DNS flows never have one, and a
    /// cleanly completed upstream drops it before client-facing TCP closing state is retained.
    upstream_lease: Option<Lease>,
    bridge: Bridge,
    refresh: bool,
    serving: Serving,
    deadline: Option<Instant>,
    // Keep the client-facing socket until its closing handshake finishes after upstream I/O completes.
    client_closing: bool,
}

#[derive(Default)]
struct Counters {
    opened: u64,
    resolved: u64,
    answered_here: u64,
    denied: u64,
    unadmitted: u64,
    unsettled: u64,
    client_closing: u64,
    ingress: boundary::Counters,
    expired: u64,
    closed: u64,
    to_client: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "opened {} resolved {} answered-here {} denied {} unadmitted {} unsettled {} reset {} \
             tail-failed {} expired {} client-closing {} closed {} to-upstream {} to-client {} stale {} \
             unconsumed {}",
            self.opened,
            self.resolved,
            self.answered_here,
            self.denied,
            self.unadmitted,
            self.unsettled,
            self.ingress.reset,
            self.ingress.tail_failed,
            self.expired,
            self.client_closing,
            self.closed,
            self.ingress.to_upstream,
            self.to_client,
            self.ingress.stale,
            self.ingress.unconsumed
        )
    }
}

pub(crate) struct Engine {
    interface: Interface,
    sockets: flow_setup::Sockets,
    device: Shim,
    flows: Workers<SocketHandle, Flow>,
    queries: Transactions,
    sweep: CancellationToken,
    asks: mpsc::UnboundedSender<tcp_dns::Ask>,
    outgoing: Turns<SocketHandle>,
    counters: Counters,
    started: Instant,
}

impl Engine {
    pub(crate) fn new(mtu: usize, seed: u64) -> (Self, mpsc::UnboundedReceiver<tcp_dns::Ask>) {
        let mut device = Shim::new(mtu);
        let started = Instant::now();
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = seed;
        let mut interface = Interface::new(config, &mut device, SmolInstant::from_millis(0));
        // Interception listens on the remote destination rather than an address assigned to this interface.
        interface.set_any_ip(true);
        // Cargo explicitly pins smoltcp's maintained interface-address capacity to two: exactly these IPv4
        // and IPv6 interception addresses. A full vector refuses the push; the address is omitted and the
        // invariant failure is reported rather than hidden.
        interface.update_ip_addrs(|addresses| {
            for cidr in [
                IpCidr::new(IpAddress::v4(192, 0, 2, 1), 30),
                IpCidr::new(IpAddress::v6(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1), 64),
            ] {
                if addresses.push(cidr).is_err() {
                    report::message_with_details(
                        "shizuku.tcp_engine",
                        "the client-side stack refused one of its own addresses",
                        "InvalidInput",
                        [("address", cidr)],
                    );
                }
            }
        });
        let queries = Transactions::new();
        // Each transport remains sequential, while this aggregate owner handoff has no locally invented
        // transport-count cap. The session owner consumes one request per fair scheduler pass.
        let (asks, asking) = mpsc::unbounded_channel();
        (
            Self {
                interface,
                sockets: flow_setup::Sockets::new(),
                device,
                flows: Workers::new("shizuku.tcp_flow"),
                queries,
                sweep: CancellationToken::new(),
                asks,
                outgoing: Turns::default(),
                counters: Counters::default(),
                started,
            },
            asking,
        )
    }

    pub(crate) fn release(self, asking: mpsc::UnboundedReceiver<tcp_dns::Ask>) {
        self.queries.release();
        drop(self.flows);
        drop(self.outgoing);
        drop(self.sockets);
        drop(self.asks);
        drop(asking);
        drop(self.device);
    }

    fn now(&self) -> SmolInstant {
        SmolInstant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    /// Retires all flows, then drains transactions, returning the first local failure.
    pub(crate) async fn shutdown(
        &mut self,
        admission: &mut Admission,
        output: &mut Output,
    ) -> io::Result<()> {
        // Cancel the shared flow sweep before per-flow tokens so workers choose abortive shutdown.
        self.sweep.cancel();
        {
            let Engine {
                flows,
                sockets,
                outgoing,
                counters,
                ..
            } = self;
            debug_assert_eq!(
                outgoing.len(),
                flows.len(),
                "the round-robin order indexes exactly the live flows"
            );
            for handle in outgoing.iter() {
                let Some(held) = flows.get_mut(handle) else {
                    continue;
                };
                if held.cancel.is_cancelled() {
                    continue;
                }
                held.cancel.cancel();
                let socket = sockets.get_mut::<Socket>(*handle);
                if socket.remote_endpoint().is_some() {
                    counters.ingress.reset += 1;
                }
                socket.abort();
            }
        }
        self.poll(output);
        loop {
            let Some((handle, incarnation)) = self
                .flows
                .iter()
                .find(|(_, held)| held.record.client_closing)
                .map(|(handle, held)| (*handle, held.id))
            else {
                break;
            };
            self.finish_client_close(handle, incarnation, admission);
        }
        while self.flows.values().any(|held| !held.record.client_closing) {
            // Joining is the descriptor-close fence; accounting is released only from `close` afterwards.
            let terminal = self.flows.finished().await;
            self.close(terminal, admission, output);
        }
        debug_assert_eq!(
            self.sockets.iter().count(),
            self.flows.len(),
            "every socket belongs to exactly one live flow"
        );
        // Flows may still settle deliveries, so drain transactions last.
        self.queries.shutdown(admission)
    }

    fn open(
        &mut self,
        client: SocketAddr,
        destination: SocketAddr,
        hop_limit: u8,
        resolver: bool,
        now: Instant,
        admission: &mut Admission,
    ) -> Option<SocketHandle> {
        let source = if resolver {
            Source::Resolver
        } else {
            Source::Upstream
        };
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            return None;
        };
        let Engine {
            sockets,
            flows,
            outgoing,
            asks,
            sweep,
            counters,
            ..
        } = self;
        let mut ops = Admit {
            sockets,
            flows,
            asks,
            sweep,
            counters,
            admission,
            client,
            destination,
            hop_limit,
            source,
            now,
        };
        match flow::admit_flow(&mut ops, outgoing) {
            Ok(handle) => {
                self.counters.opened += 1;
                if resolver {
                    self.counters.resolved += 1;
                }
                Some(handle)
            }
            Err(flow::Refused::Unadmitted) => {
                self.counters.unadmitted += 1;
                None
            }
            Err(flow::Refused::Unbuildable(())) => None,
        }
    }

    pub(crate) fn poll(&mut self, output: &mut Output) {
        self.quiesce(self.now(), output);
        self.reclaim_closed();
    }

    fn quiesce(&mut self, at: SmolInstant, output: &mut Output) {
        loop {
            let progressed = self.interface.poll(at, &mut self.device, &mut self.sockets);
            let emitted = match self.device.drain() {
                Some(packet) => {
                    output.packet(packet);
                    true
                }
                None => false,
            };
            if matches!(progressed, PollResult::None) && !emitted {
                break;
            }
        }
    }

    pub(super) fn reclaim_closed(&mut self) {
        let Engine {
            flows,
            sockets,
            outgoing,
            ..
        } = self;
        debug_assert_eq!(
            outgoing.len(),
            flows.len(),
            "the round-robin order indexes exactly the live flows"
        );
        for handle in outgoing.iter() {
            let Some(held) = flows.get(handle) else {
                continue;
            };
            held.record
                .bridge
                .teardown(sockets.get::<Socket>(*handle).state(), &held.cancel);
        }
    }

    fn socket(&mut self, handle: SocketHandle) -> Option<&mut Socket<'static>> {
        self.flows
            .contains(&handle)
            .then(|| self.sockets.get_mut::<Socket>(handle))
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} flows, {}, {}",
            self.flows.len(),
            self.queries.describe(),
            self.counters.describe()
        )
    }
}

struct Admit<'a> {
    sockets: &'a mut flow_setup::Sockets,
    flows: &'a mut Workers<SocketHandle, Flow>,
    asks: &'a mpsc::UnboundedSender<tcp_dns::Ask>,
    sweep: &'a CancellationToken,
    counters: &'a mut Counters,
    admission: &'a mut Admission,
    client: SocketAddr,
    destination: SocketAddr,
    hop_limit: u8,
    source: Source,
    now: Instant,
}

struct Built {
    flow: Flow,
    identity: Identity,
    transport: Option<Transport>,
    stream: Option<Worker>,
    control: Option<mpsc::Receiver<tcp_dns::Control>>,
    filled: Option<mpsc::Sender<Owned>>,
}

enum Transport {
    Resolver,
    Upstream(Result<socket2::Socket, Failure>),
}

impl Built {
    fn without_worker(flow: Flow, identity: Identity) -> Self {
        Self {
            flow,
            identity,
            transport: None,
            stream: None,
            control: None,
            filled: None,
        }
    }
}

impl Admit<'_> {
    fn prepare(&mut self) -> Result<flow_setup::Prepared, flow_setup::Denied> {
        flow_setup::prepare(
            self.sockets,
            self.flows,
            flow_setup::Sizing {
                flow: flow_budget::SIZING,
                hop_limit: self.hop_limit,
            },
            IpListenEndpoint::from(endpoint(self.destination)),
        )
    }

    fn assemble(
        &self,
        prepared: flow_setup::Prepared,
        upstream_lease: Option<Lease>,
        transport: Transport,
    ) -> (SocketHandle, u64, Built) {
        let flow_setup::Prepared {
            handle,
            identity,
            bridge,
            stream,
            serving,
            control,
            filled,
        } = prepared;
        let incarnation = identity.id;
        (
            handle,
            incarnation,
            Built {
                flow: Flow {
                    client: self.client,
                    destination: self.destination,
                    upstream_lease,
                    bridge,
                    refresh: false,
                    serving,
                    client_closing: false,
                    deadline: lifetime::deadline(
                        self.now,
                        self.sockets.get::<Socket>(handle).state(),
                    ),
                },
                identity,
                transport: Some(transport),
                stream: Some(stream),
                control: Some(control),
                filled: Some(filled),
            },
        )
    }
}

impl flow::FlowOps for Admit<'_> {
    type Handle = SocketHandle;
    type Record = Built;
    type Error = ();

    fn build(&mut self) -> Result<(SocketHandle, u64, Built), ()> {
        let prepared = match self.prepare() {
            Ok(prepared) => prepared,
            Err(denied) => {
                if let flow_setup::Denied::Listen(e) = denied {
                    report::message_with_details(
                        "shizuku.tcp_listen",
                        format!("the client-side stack cannot intercept this destination: {e}"),
                        "InvalidInput",
                        [("client", self.client), ("destination", self.destination)],
                    );
                }
                self.counters.denied += 1;
                return Err(());
            }
        };
        let (upstream_lease, transport) = match self.source {
            Source::Resolver => (None, Transport::Resolver),
            Source::Upstream => match crate::shizuku::egress::open_tcp(self.destination) {
                Err(failure) => (None, Transport::Upstream(Err(failure))),
                Ok(socket) => {
                    let Ok(lease) = self.admission.reserve(Class::General) else {
                        // Admission follows the synchronous open, so denial closes the candidate before any
                        // other owner turn and retains neither a descriptor nor client-side flow state.
                        drop(socket);
                        let flow_setup::Prepared {
                            handle,
                            identity,
                            bridge,
                            stream,
                            serving,
                            control,
                            filled,
                        } = prepared;
                        drop(identity);
                        drop(serving);
                        flow_setup::release(
                            self.sockets,
                            handle,
                            flow_setup::Leftovers {
                                bridge,
                                stream: Some(stream),
                                control: Some(control),
                                filled: Some(filled),
                            },
                        );
                        self.counters.denied += 1;
                        return Err(());
                    };
                    (Some(lease), Transport::Upstream(Ok(socket)))
                }
            },
        };
        let (handle, incarnation, built) = self.assemble(prepared, upstream_lease, transport);
        Ok((handle, incarnation, built))
    }

    fn unwind(&mut self, handle: SocketHandle, _incarnation: u64, record: Built) {
        let Built {
            flow,
            transport,
            stream,
            control,
            filled,
            ..
        } = record;
        let Flow {
            upstream_lease,
            bridge,
            serving,
            ..
        } = flow;
        // If admission refused after build, its unspawned worker future has already dropped the open socket.
        drop(transport);
        serving.close(self.admission);
        flow_setup::release(
            self.sockets,
            handle,
            flow_setup::Leftovers {
                bridge,
                stream,
                control,
                filled,
            },
        );
        if let Some(lease) = upstream_lease {
            self.admission.release(lease);
        }
    }

    fn admit(
        &mut self,
        handle: SocketHandle,
        _incarnation: u64,
        record: Built,
    ) -> Result<(), Built> {
        self.start(handle, record)
    }
}

impl Admit<'_> {
    #[allow(clippy::result_large_err)]
    fn start(&mut self, handle: SocketHandle, record: Built) -> Result<(), Built> {
        let Built {
            flow,
            identity,
            transport,
            stream,
            control,
            filled,
        } = record;
        let (Some(transport), Some(stream), Some(control), Some(filled)) =
            (transport, stream, control, filled)
        else {
            return Err(Built::without_worker(flow, identity));
        };
        let token = identity.cancel.clone();
        let Transport::Upstream(socket) = transport else {
            let asks = self.asks.clone();
            return self
                .flows
                .admit(
                    handle,
                    &identity,
                    flow,
                    tcp_dns::serve(
                        FlowId::new(handle, identity.id),
                        stream,
                        asks,
                        control,
                        filled,
                        token,
                    ),
                )
                .map_err(|(flow, _)| Built::without_worker(flow, identity));
        };
        drop(control);
        drop(filled);
        let destination = self.destination;
        let sweep = self.sweep.clone();
        self.flows
            .admit(handle, &identity, flow, async move {
                let connected = match socket {
                    Ok(socket) => tokio::select! {
                        biased;
                        () = token.cancelled() => return Ended::Expected,
                        connected = crate::shizuku::egress::connect_tcp(socket, destination) => connected,
                    },
                    Err(failure) => Err(failure),
                };
                match connected {
                    Ok(socket) => match tokio::net::TcpStream::from_std(socket.into()) {
                        Ok(upstream) => tcp_flow::splice(upstream, stream, token, sweep).await,
                        Err(e) => Ended::Failed {
                            context: "shizuku.tcp_upstream_adopt",
                            error: e,
                        },
                    },
                    Err(failure) => failure.ended("upstream connect"),
                }
            })
            .map_err(|(flow, _)| Built::without_worker(flow, identity))
    }
}

fn endpoint(address: SocketAddr) -> IpEndpoint {
    IpEndpoint::new(address.ip().into(), address.port())
}
