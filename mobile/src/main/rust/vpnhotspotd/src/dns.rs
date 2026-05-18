use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use libc::c_int;
use socket2::SockRef;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest, Ready};
use tokio::net::{
    TcpListener as TokioTcpListener, TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket,
};
use tokio::sync::Mutex;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::report;
use crate::socket::{is_connection_closed, set_nonblocking};
use vpnhotspotd::shared::dns_counter::DnsCounters;
use vpnhotspotd::shared::dns_wire;
use vpnhotspotd::shared::model::{
    daemon_counter_epoch, mac_string, ClientDnsPorts, Network, SessionConfig,
};
use vpnhotspotd::shared::proto::daemon;
use vpnhotspotd::shared::protocol::daemon_io_error_report_with_details;

pub(crate) const DNS_PORT: u16 = 53;
// android/multinetwork.h: ResNsendFlags::ANDROID_RESOLV_NO_RETRY.
const ANDROID_RESOLV_NO_RETRY: u32 = 1 << 0;
// Maximum DNS message size carried over TCP or EDNS0 UDP.
const DNS_MAX_PACKET: usize = 65_535;

pub(crate) struct Runtime {
    call_id: u64,
    downstream: String,
    bind_address: Ipv4Addr,
    reply_mark: u32,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    clients: HashMap<[u8; 6], ClientRuntime>,
    counters: DnsCounters,
}

struct ClientRuntime {
    tcp: Option<ClientListenerRuntime>,
    udp: Option<ClientListenerRuntime>,
}

struct ClientListenerRuntime {
    port: u16,
    stop: CancellationToken,
}

struct DnsCounterContext {
    counters: DnsCounters,
    mac: [u8; 6],
}

struct DnsResponse {
    bytes: Vec<u8>,
    sent_to_resolver: bool,
    received_from_resolver: bool,
}

pub(crate) type CounterSink = DnsCounters;

impl Runtime {
    pub(crate) fn start(
        call_id: u64,
        downstream: &str,
        bind_address: Ipv4Addr,
        reply_mark: u32,
        config: Arc<Mutex<SessionConfig>>,
        stop: CancellationToken,
        initial_config: &SessionConfig,
    ) -> Self {
        let mut runtime = Self {
            call_id,
            downstream: downstream.to_owned(),
            bind_address,
            reply_mark,
            config,
            stop,
            clients: HashMap::new(),
            counters: DnsCounters::new(daemon_counter_epoch(b"dns", call_id)),
        };
        runtime.replace_clients(initial_config);
        runtime
    }

    pub(crate) fn replace_clients(&mut self, config: &SessionConfig) {
        let mut next_macs = Vec::new();
        for client in &config.clients {
            if !next_macs.contains(&client.mac) {
                next_macs.push(client.mac);
            }
        }
        self.clients.retain(|mac, client| {
            if next_macs.contains(mac) {
                true
            } else {
                client.cancel();
                false
            }
        });
        for mac in next_macs {
            let needs_tcp = self
                .clients
                .get(&mac)
                .is_none_or(|client| client.tcp.is_none());
            let needs_udp = self
                .clients
                .get(&mac)
                .is_none_or(|client| client.udp.is_none());
            let tcp = if needs_tcp { self.start_tcp(mac) } else { None };
            let udp = if needs_udp { self.start_udp(mac) } else { None };
            let client = self.clients.entry(mac).or_insert(ClientRuntime {
                tcp: None,
                udp: None,
            });
            if needs_tcp {
                client.tcp = tcp;
            }
            if needs_udp {
                client.udp = udp;
            }
        }
    }

    pub(crate) fn ports(&self) -> Vec<ClientDnsPorts> {
        self.clients
            .iter()
            .filter_map(|(mac, client)| client.ports(*mac))
            .collect()
    }

    pub(crate) fn retain_ports(&mut self, committed: &[ClientDnsPorts]) {
        self.clients.retain(|mac, client| {
            let committed = committed.iter().find(|ports| ports.mac == *mac);
            if committed.and_then(|ports| ports.tcp).is_none() {
                if let Some(runtime) = client.tcp.take() {
                    runtime.stop.cancel();
                }
            }
            if committed.and_then(|ports| ports.udp).is_none() {
                if let Some(runtime) = client.udp.take() {
                    runtime.stop.cancel();
                }
            }
            if client.tcp.is_some() || client.udp.is_some() {
                true
            } else {
                client.cancel();
                false
            }
        });
    }

    pub(crate) fn counter_sink(&self) -> CounterSink {
        self.counters.clone()
    }

    pub(crate) async fn counters(&self) -> Vec<daemon::TrafficCounter> {
        match self
            .counters
            .counters(&self.downstream, self.clients.keys().copied())
        {
            Ok(counters) => counters,
            Err(e) => {
                report::io("dns.counter", e);
                Vec::new()
            }
        }
    }

    fn start_tcp(&self, mac: [u8; 6]) -> Option<ClientListenerRuntime> {
        let stop = self.stop.child_token();
        match create_tcp_listener(self.bind_address, self.reply_mark).and_then(|listener| {
            let port = listener.local_addr()?.port();
            spawn_tcp_loop(
                listener,
                self.config.clone(),
                stop.clone(),
                self.counters.clone(),
                mac,
            )?;
            Ok(port)
        }) {
            Ok(port) => Some(ClientListenerRuntime { port, stop }),
            Err(e) => {
                stop.cancel();
                report_start_error(
                    self.call_id,
                    "dns.tcp_start",
                    e,
                    &self.downstream,
                    self.bind_address,
                    mac,
                );
                None
            }
        }
    }

    fn start_udp(&self, mac: [u8; 6]) -> Option<ClientListenerRuntime> {
        let stop = self.stop.child_token();
        match create_udp_listener(self.bind_address, self.reply_mark).and_then(|socket| {
            let port = socket.local_addr()?.port();
            spawn_udp_loop(
                socket,
                self.config.clone(),
                stop.clone(),
                self.counters.clone(),
                mac,
            )?;
            Ok(port)
        }) {
            Ok(port) => Some(ClientListenerRuntime { port, stop }),
            Err(e) => {
                stop.cancel();
                report_start_error(
                    self.call_id,
                    "dns.udp_start",
                    e,
                    &self.downstream,
                    self.bind_address,
                    mac,
                );
                None
            }
        }
    }
}

impl ClientRuntime {
    fn ports(&self, mac: [u8; 6]) -> Option<ClientDnsPorts> {
        let tcp = self.tcp.as_ref().map(|runtime| runtime.port);
        let udp = self.udp.as_ref().map(|runtime| runtime.port);
        if tcp.is_some() || udp.is_some() {
            Some(ClientDnsPorts { mac, tcp, udp })
        } else {
            None
        }
    }

    fn cancel(&self) {
        if let Some(runtime) = self.tcp.as_ref() {
            runtime.stop.cancel();
        }
        if let Some(runtime) = self.udp.as_ref() {
            runtime.stop.cancel();
        }
    }
}

fn report_start_error(
    call_id: u64,
    context: &str,
    error: io::Error,
    downstream: &str,
    bind_address: Ipv4Addr,
    mac: [u8; 6],
) {
    report::report_for(
        Some(call_id),
        daemon_io_error_report_with_details(
            context,
            error,
            [
                ("downstream", downstream.to_owned()),
                ("bind_address", bind_address.to_string()),
                ("mac", mac_string(&mac)),
            ],
        ),
    );
}

struct ResolverQuery {
    fd: Option<RawFd>,
}

impl ResolverQuery {
    fn finish(mut self) -> io::Result<Vec<u8>> {
        let mut rcode = 0;
        let mut response = vec![0u8; DNS_MAX_PACKET];
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

fn create_tcp_listener(bind_address: Ipv4Addr, reply_mark: u32) -> io::Result<TcpListener> {
    let listener = TcpListener::bind((bind_address, 0))?;
    SockRef::from(&listener).set_mark(reply_mark)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn create_udp_listener(bind_address: Ipv4Addr, reply_mark: u32) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((bind_address, 0))?;
    SockRef::from(&socket).set_mark(reply_mark)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn spawn_tcp_loop(
    listener: TcpListener,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    counters: DnsCounters,
    mac: [u8; 6],
) -> io::Result<()> {
    let listener = TokioTcpListener::from_std(listener)?;
    spawn(async move {
        loop {
            select! {
                _ = stop.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((socket, _)) => {
                        let config = config.clone();
                        let counters = counters.clone();
                        let connection_stop = stop.child_token();
                        spawn(async move {
                            select! {
                                _ = connection_stop.cancelled() => {}
                                result = async {
                                    let snapshot = config.lock().await.clone();
                                    handle_tcp_connection_counted(socket, snapshot, counters, mac).await
                                } => if let Err(e) = result {
                                    if is_connection_closed(&e) {
                                        report::stderr!("dns tcp connection closed: {e}");
                                    } else {
                                        report::io("dns.tcp_connection", e);
                                    }
                                }
                            }
                        });
                    }
                    Err(e) => report::io("dns.tcp_accept", e),
                }
            }
        }
    });
    Ok(())
}

async fn handle_tcp_connection_counted(
    socket: TokioTcpStream,
    config: SessionConfig,
    counters: DnsCounters,
    mac: [u8; 6],
) -> io::Result<()> {
    handle_tcp_connection_counted_with_sink(socket, config, counters, mac).await
}

pub(crate) async fn handle_tcp_connection_counted_with_sink(
    socket: TokioTcpStream,
    config: SessionConfig,
    counters: CounterSink,
    mac: [u8; 6],
) -> io::Result<()> {
    handle_tcp_connection_inner(socket, config, Some(DnsCounterContext { counters, mac })).await
}

async fn handle_tcp_connection_inner(
    mut socket: TokioTcpStream,
    config: SessionConfig,
    counter: Option<DnsCounterContext>,
) -> io::Result<()> {
    loop {
        let mut header = [0u8; 2];
        match socket.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let length = u16::from_be_bytes(header) as usize;
        let mut query = vec![0u8; length];
        socket.read_exact(&mut query).await?;
        let Some(response) = resolve_or_error(&config, &query).await else {
            continue;
        };
        if let Some(counter) = counter.as_ref() {
            if let Err(e) = counter.counters.add_exchange(
                counter.mac,
                query.len(),
                response.bytes.len(),
                response.sent_to_resolver,
                response.received_from_resolver,
            ) {
                report::io("dns.counter", e);
            }
        }
        socket
            .write_all(&(response.bytes.len() as u16).to_be_bytes())
            .await?;
        socket.write_all(&response.bytes).await?;
        socket.flush().await?;
    }
}

fn spawn_udp_loop(
    socket: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    counters: DnsCounters,
    mac: [u8; 6],
) -> io::Result<()> {
    let socket = Arc::new(TokioUdpSocket::from_std(socket)?);
    spawn(async move {
        let mut buffer = [0u8; 65535];
        loop {
            select! {
                _ = stop.cancelled() => break,
                received = socket.recv_from(&mut buffer) => match received {
                    Ok((size, source)) => {
                        let snapshot = config.lock().await.clone();
                        let query = buffer[..size].to_vec();
                        let socket = socket.clone();
                        let query_stop = stop.child_token();
                        let counters = counters.clone();
                        spawn(async move {
                            select! {
                                _ = query_stop.cancelled() => {}
                                response = resolve_or_error(&snapshot, &query) => {
                                    if let Some(response) = response {
                                        if let Err(e) = counters.add_exchange(
                                            mac,
                                            query.len(),
                                            response.bytes.len(),
                                            response.sent_to_resolver,
                                            response.received_from_resolver,
                                        ) {
                                            report::io("dns.counter", e);
                                        }
                                        if let Err(e) = socket.send_to(&response.bytes, source).await {
                                            report::io_with_details(
                                                "dns.udp_response",
                                                e,
                                                [("source", source.to_string())],
                                            );
                                        }
                                    }
                                }
                            }
                        });
                    },
                    Err(e) => report::io("dns.udp_recv", e),
                }
            };
        }
    });
    Ok(())
}

async fn resolve_query_accounted(
    config: &SessionConfig,
    query: &[u8],
) -> (io::Result<Vec<u8>>, bool) {
    if let Some(primary) = config.primary_network {
        return query_network(primary, query).await;
    }
    if let Some(fallback) = config.fallback_network {
        return query_network(fallback, query).await;
    }
    (
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "no DNS upstream",
        )),
        false,
    )
}

async fn resolve_or_error(config: &SessionConfig, query: &[u8]) -> Option<DnsResponse> {
    let (result, sent_to_resolver) = resolve_query_accounted(config, query).await;
    match result {
        Ok(response) => Some(DnsResponse {
            bytes: response,
            sent_to_resolver,
            received_from_resolver: true,
        }),
        Err(e) => {
            report::stderr!("dns resolve failed: {e}");
            dns_wire::servfail_response(query).map(|response| DnsResponse {
                bytes: response,
                sent_to_resolver,
                received_from_resolver: false,
            })
        }
    }
}

pub(crate) async fn resolve_or_error_counted(
    config: &SessionConfig,
    query: &[u8],
    counters: &CounterSink,
    mac: [u8; 6],
) -> Option<Vec<u8>> {
    let response = resolve_or_error(config, query).await?;
    if let Err(e) = counters.add_exchange(
        mac,
        query.len(),
        response.bytes.len(),
        response.sent_to_resolver,
        response.received_from_resolver,
    ) {
        report::io("dns.counter", e);
    }
    Some(response.bytes)
}

async fn query_network(network: Network, query: &[u8]) -> (io::Result<Vec<u8>>, bool) {
    let fd = unsafe {
        android_res_nsend(
            network,
            query.as_ptr(),
            query.len(),
            ANDROID_RESOLV_NO_RETRY,
        )
    };
    if fd < 0 {
        return (Err(io::Error::from_raw_os_error(-fd)), false);
    }
    let fd = ResolverQuery { fd: Some(fd) };
    if let Err(e) = set_nonblocking(fd.as_raw_fd()) {
        return (Err(e), true);
    }
    let fd = match AsyncFd::new(fd) {
        Ok(fd) => fd,
        Err(e) => return (Err(e), true),
    };
    (read_resolver_result(fd).await, true)
}

async fn read_resolver_result(fd: AsyncFd<ResolverQuery>) -> io::Result<Vec<u8>> {
    // android_res_nresult is the public result reader/closer, but it performs synchronous reads.
    // dnsproxyd's resnsend handler writes one result and then drops the client socket, so wait for
    // peer close before handing the nonblocking fd back to the NDK reader.
    loop {
        let mut ready = fd.ready(Interest::READABLE | Interest::ERROR).await?;
        let state = ready.ready();
        if state.is_read_closed() || state.is_error() {
            drop(ready);
            return fd.into_inner().finish();
        }
        ready.clear_ready_matching(Ready::READABLE);
    }
}
