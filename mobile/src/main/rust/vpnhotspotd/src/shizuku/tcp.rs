//! Client-side TCP termination backed by independently connected upstream sockets or virtual DNS.
use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use smoltcp::iface::{Config, Interface, PollResult, SocketHandle, SocketStorage};
use smoltcp::socket::tcp::Socket;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::workers::{Ended, Identity, Workers};

use crate::shizuku::flow_setup;
use vpnhotspotd::shared::admission::{
    largest_fitting, linear_footprint, Admission, Class, Denied, Lease, Request,
};
use vpnhotspotd::shared::bridge::{Bridge, Worker};
use vpnhotspotd::shared::flow::{self, FlowId, Turns};
use vpnhotspotd::shared::flow_budget;
use vpnhotspotd::shared::ingress as boundary;
use vpnhotspotd::shared::reply_bound::{built_depth, channel_footprint};

use crate::report;
use crate::shizuku::output::Output;
use crate::shizuku::owned::Owned;
use crate::shizuku::tcp_device::Shim;
use crate::shizuku::tcp_dns::{self, Serving, Transactions};
use crate::shizuku::tcp_flow;
use crate::shizuku::tun_writer::Stamp;

mod bridge;
mod dns;
mod ingress;
mod lifetime;
mod terminal;

pub(crate) use bridge::Attention;

const SOCKET_SLOT_BYTES: u64 = std::mem::size_of::<SocketStorage<'static>>() as u64;

fn flow_footprint() -> Option<u64> {
    flow_budget::footprint::<Owned, tcp_dns::Control>(&flow_budget::SIZING)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Resolver,
    Upstream(Network),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Upstream,
    Resolver,
}

impl Source {
    fn kind(self) -> Kind {
        match self {
            Self::Resolver => Kind::Resolver,
            Self::Upstream(_) => Kind::Upstream,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retirement {
    Everything,
    // Generation changes preserve virtual-DNS transports because they hold no selected-network socket.
    Upstreams,
}

impl Retirement {
    fn retires(self, kind: Kind) -> bool {
        matches!(
            (self, kind),
            (Self::Everything, _) | (Self::Upstreams, Kind::Upstream)
        )
    }
}

struct Flow {
    client: SocketAddr,
    destination: SocketAddr,
    kind: Kind,
    lease: Lease,
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
    preserved: u64,
    unprepared: u64,
    unsettled: u64,
    client_closing: u64,
    no_upstream: u64,
    ingress: boundary::Counters,
    expired: u64,
    closed: u64,
    to_client: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "opened {} resolved {} answered-here {} denied {} preserved {} no-upstream {} reset {} \
             tail-failed {} expired {} client-closing {} closed {} to-upstream {} to-client {} stale {} \
             unconsumed {}",
            self.opened,
            self.resolved,
            self.answered_here,
            self.denied,
            self.preserved,
            self.no_upstream,
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
    stamp: Stamp,
    upstream: Option<Network>,
    sweep: CancellationToken,
    asks: mpsc::Sender<tcp_dns::Ask>,
    outgoing: Turns<SocketHandle>,
    tables: Lease,
    prepared: usize,
    counters: Counters,
    started: Instant,
}

impl Engine {
    pub(crate) fn new(
        mtu: usize,
        seed: u64,
        admission: &mut Admission,
    ) -> Result<(Self, mpsc::Receiver<tcp_dns::Ask>), Denied> {
        let mut device = Shim::new(mtu);
        let started = Instant::now();
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = seed;
        let mut interface = Interface::new(config, &mut device, SmolInstant::from_millis(0));
        // Interception listens on the remote destination rather than an address assigned to this interface.
        interface.set_any_ip(true);
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
        let per_flow = flow_footprint().ok_or(Denied::Arithmetic)?;
        // Transactions has no implicit lease refund: reserve it before sizing flows, and every failure before
        // Engine takes ownership must call release.
        let queries = Transactions::new(admission)?;
        // Solve from general headroom so TCP cannot consume the reserved DNS and packet-completion floors.
        let prepared = largest_fitting(admission.general_headroom(), per_flow, |flows| {
            tables_footprint(flows, mtu)
        });
        let bytes = match tables_footprint(prepared, mtu) {
            Some(bytes) => bytes,
            None => {
                queries.release(admission);
                return Err(Denied::Arithmetic);
            }
        };
        let tables = match admission.reserve(Request::bytes(bytes, Class::General)) {
            Ok(tables) => tables,
            Err(why) => {
                queries.release(admission);
                return Err(why);
            }
        };
        let (asks, asking) = mpsc::channel(submission_depth(prepared));
        Ok((
            Self {
                interface,
                sockets: flow_setup::Sockets::new(prepared),
                device,
                flows: Workers::with_capacity("shizuku.tcp_flow", prepared),
                queries,
                stamp: Stamp::default(),
                upstream: None,
                sweep: CancellationToken::new(),
                asks,
                outgoing: Turns::with_capacity(prepared),
                tables,
                prepared,
                counters: Counters::default(),
                started,
            },
            asking,
        ))
    }

    pub(crate) fn release(self, asking: mpsc::Receiver<tcp_dns::Ask>, admission: &mut Admission) {
        // Drop every allocation covered by `tables` before refunding its lease.
        self.queries.release(admission);
        drop(self.flows);
        drop(self.outgoing);
        drop(self.sockets);
        drop(self.asks);
        drop(asking);
        drop(self.device);
        admission.release(self.tables);
    }

    fn now(&self) -> SmolInstant {
        SmolInstant::from_micros(self.started.elapsed().as_micros() as i64)
    }

    pub(crate) async fn apply(
        &mut self,
        stamp: Stamp,
        upstream: Option<Network>,
        admission: &mut Admission,
        output: &mut Output,
    ) {
        let retiring = (stamp != self.stamp).then_some(Retirement::Upstreams);
        // Resets emitted by retirement must carry the successor stamp or the TUN writer will discard them.
        self.stamp = stamp;
        self.upstream = upstream;
        if let Some(retiring) = retiring {
            self.retire(retiring, admission, output).await;
            self.sweep = CancellationToken::new();
        }
    }

    /// Retires all flows, then drains transactions, returning the first local failure.
    pub(crate) async fn shutdown(
        &mut self,
        admission: &mut Admission,
        output: &mut Output,
    ) -> io::Result<()> {
        self.retire(Retirement::Everything, admission, output).await;
        // Flows may still settle deliveries, so drain transactions last.
        self.queries.shutdown(admission)
    }

    async fn retire(&mut self, scope: Retirement, admission: &mut Admission, output: &mut Output) {
        // Cancel the shared upstream sweep before per-flow tokens so workers choose abortive shutdown.
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
                if !scope.retires(held.record.kind) {
                    counters.preserved += 1;
                    continue;
                }
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
                .find(|(_, held)| held.record.client_closing && scope.retires(held.record.kind))
                .map(|(handle, held)| (*handle, held.id))
            else {
                break;
            };
            self.finish_client_close(handle, incarnation, admission);
        }
        while self
            .flows
            .values()
            .any(|held| scope.retires(held.record.kind) && !held.record.client_closing)
        {
            // Joining is the descriptor-close fence; accounting is released only from `close` afterwards.
            let terminal = self.flows.finished().await;
            self.close(terminal, admission, output);
        }
        debug_assert_eq!(
            self.sockets.iter().count(),
            self.flows.len(),
            "every socket belongs to exactly one live flow"
        );
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
            let Some(upstream) = self.upstream else {
                self.counters.no_upstream += 1;
                return None;
            };
            Source::Upstream(upstream)
        };
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            return None;
        };
        let Engine {
            sockets,
            flows,
            outgoing,
            prepared,
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
        match flow::admit_flow(&mut ops, outgoing, *prepared) {
            Ok(handle) => {
                self.counters.opened += 1;
                if resolver {
                    self.counters.resolved += 1;
                }
                Some(handle)
            }
            Err(flow::Refused::AtCapacity(_)) => {
                self.counters.unprepared += 1;
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
                    output.packet(self.stamp, packet);
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
    asks: &'a mpsc::Sender<tcp_dns::Ask>,
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
    stream: Option<Worker>,
    control: Option<mpsc::Receiver<tcp_dns::Control>>,
    filled: Option<mpsc::Sender<Owned>>,
}

impl Built {
    fn without_worker(flow: Flow, identity: Identity) -> Self {
        Self {
            flow,
            identity,
            stream: None,
            control: None,
            filled: None,
        }
    }
}

impl Admit<'_> {
    fn prepare(&mut self) -> Result<flow_setup::Prepared, flow_setup::Denied> {
        let bytes = flow_footprint().ok_or(flow_setup::Denied::Grant)?;
        flow_setup::prepare(
            self.admission,
            self.sockets,
            self.flows,
            flow_setup::Sizing {
                bytes,
                flow: flow_budget::SIZING,
                hop_limit: self.hop_limit,
            },
            IpListenEndpoint::from(endpoint(self.destination)),
        )
    }

    fn assemble(&self, prepared: flow_setup::Prepared) -> (SocketHandle, u64, Built) {
        let flow_setup::Prepared {
            lease,
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
                    kind: self.source.kind(),
                    lease,
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

    fn has_room(&self) -> bool {
        self.flows.has_room()
    }

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
        let (handle, incarnation, built) = self.assemble(prepared);
        Ok((handle, incarnation, built))
    }

    fn unwind(&mut self, handle: SocketHandle, _incarnation: u64, record: Built) {
        let Built {
            flow,
            stream,
            control,
            filled,
            ..
        } = record;
        let Flow {
            lease,
            bridge,
            serving,
            ..
        } = flow;
        serving.close(self.admission);
        flow_setup::release(
            self.admission,
            self.sockets,
            handle,
            flow_setup::Leftovers {
                lease,
                bridge,
                stream,
                control,
                filled,
            },
        );
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
            stream,
            control,
            filled,
        } = record;
        let (Some(stream), Some(control), Some(filled)) = (stream, control, filled) else {
            return Err(Built::without_worker(flow, identity));
        };
        let token = identity.cancel.clone();
        let Source::Upstream(upstream) = self.source else {
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
                let connected = tokio::select! {
                    biased;
                    () = token.cancelled() => return Ended::Expected,
                    connected = crate::shizuku::egress::connect_tcp(upstream, destination) => connected,
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

fn tables_footprint(flows: usize, mtu: usize) -> Option<u64> {
    Workers::<SocketHandle, Flow>::footprint(flows)?
        .checked_add(linear_footprint(
            flows,
            std::mem::size_of::<SocketHandle>() as u64,
        )?)?
        .checked_add(channel_footprint::<tcp_dns::Ask>(
            submission_depth(flows),
            flows,
        )?)?
        .checked_add(linear_footprint(flows, SOCKET_SLOT_BYTES)?)?
        .checked_add(mtu as u64)
}

/// At most one pending ask per prepared sequential transport.
fn submission_depth(flows: usize) -> usize {
    built_depth(flows)
}

fn endpoint(address: SocketAddr) -> IpEndpoint {
    IpEndpoint::new(address.ip().into(), address.port())
}
