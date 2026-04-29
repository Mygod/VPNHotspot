use std::collections::HashMap;
use std::env;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{
    Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6, TcpListener, TcpStream, UdpSocket,
};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::ptr::null;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{
    TcpListener as TokioTcpListener, TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket,
    UnixStream,
};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::time;
use tokio_util::sync::CancellationToken;

const AF_UNIX: c_int = 1;
const AF_INET6: c_int = 10;

const SOCK_STREAM: c_int = 1;
const SOCK_DGRAM: c_int = 2;
const SOCK_RAW: c_int = 3;

const SOL_SOCKET: c_int = 1;
const SO_ERROR: c_int = 4;
const SO_MARK: c_int = 36;
const SO_REUSEADDR: c_int = 2;
const SO_BINDTODEVICE: c_int = 25;

const IPPROTO_ICMPV6: c_int = 58;
const IPPROTO_IPV6: c_int = 41;

const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0x800;
const EINPROGRESS: i32 = 115;

const MSG_DONTWAIT: c_int = 0x40;
const IPV6_RECVORIGDSTADDR: c_int = 74;
const IPV6_TRANSPARENT: c_int = 75;
const IPV6_V6ONLY: c_int = 26;
const IPV6_UNICAST_HOPS: c_int = 16;
const IPV6_MULTICAST_HOPS: c_int = 18;

const STATUS_OK: u8 = 0;
const STATUS_ERROR: u8 = 1;

const CMD_START_SESSION: u32 = 1;
const CMD_REPLACE_SESSION: u32 = 2;
const CMD_REMOVE_SESSION: u32 = 3;
const CMD_SHUTDOWN: u32 = 4;

const DNS_PORT: u16 = 53;
const RA_PERIOD: Duration = Duration::from_secs(30);
const DEPRECATED_RA_PERIOD: Duration = Duration::from_secs(3);
const DEPRECATED_RA_WINDOW: Duration = Duration::from_secs(15);
const LOOP_SLEEP: Duration = Duration::from_millis(50);
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TCP_WORKER_COUNT: usize = 8;
const UDP_ASSOC_IDLE: Duration = Duration::from_secs(60);
const UDP_ASSOC_MAX: usize = 1024;
const ANDROID_RESOLV_NO_RETRY: u32 = 1;
const MAX_CONTROL_PACKET_SIZE: usize = 65535;

#[repr(C)]
struct SockAddr {
    sa_family: u16,
    sa_data: [u8; 14],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct In6Addr {
    s6_addr: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrIn6 {
    sin6_family: u16,
    sin6_port: u16,
    sin6_flowinfo: u32,
    sin6_addr: In6Addr,
    sin6_scope_id: u32,
}

#[repr(C)]
struct SockAddrUn {
    sun_family: u16,
    sun_path: [c_char; 108],
}

#[repr(C)]
struct Iovec {
    iov_base: *mut c_void,
    iov_len: usize,
}

#[repr(C)]
struct MsgHdr {
    msg_name: *mut c_void,
    msg_namelen: u32,
    msg_iov: *mut Iovec,
    msg_iovlen: usize,
    msg_control: *mut c_void,
    msg_controllen: usize,
    msg_flags: c_int,
}

#[repr(C)]
struct Cmsghdr {
    cmsg_len: usize,
    cmsg_level: c_int,
    cmsg_type: c_int,
}

#[derive(Clone, Copy)]
struct BorrowedRawFd(RawFd);

impl AsRawFd for BorrowedRawFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

struct ResolverQuery {
    fd: Option<RawFd>,
}

impl ResolverQuery {
    fn finish(mut self, rcode: &mut c_int, answer: &mut [u8]) -> c_int {
        unsafe {
            android_res_nresult(
                self.fd.take().unwrap(),
                rcode,
                answer.as_mut_ptr(),
                answer.len(),
            )
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
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
    fn android_res_nsend(network: u64, msg: *const u8, msglen: usize, flags: u32) -> c_int;
    fn android_res_nresult(fd: c_int, rcode: *mut c_int, answer: *mut u8, anslen: usize) -> c_int;
    fn android_res_cancel(nsend_fd: c_int);
}

unsafe extern "C" {
    fn bind(fd: c_int, addr: *const SockAddr, addrlen: u32) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn connect(fd: c_int, addr: *const SockAddr, addrlen: u32) -> c_int;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn getsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut u32,
    ) -> c_int;
    fn if_nametoindex(name: *const c_char) -> c_uint;
    fn listen(fd: c_int, backlog: c_int) -> c_int;
    fn recvfrom(
        fd: c_int,
        buf: *mut c_void,
        len: usize,
        flags: c_int,
        addr: *mut SockAddr,
        addrlen: *mut u32,
    ) -> isize;
    fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> isize;
    fn sendto(
        fd: c_int,
        buf: *const c_void,
        len: usize,
        flags: c_int,
        addr: *const SockAddr,
        addrlen: u32,
    ) -> isize;
    fn setsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: u32,
    ) -> c_int;
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct Route {
    prefix: u128,
    prefix_len: u8,
}

#[derive(Clone)]
struct Upstream {
    network_handle: u64,
    interface: String,
    routes: Vec<Route>,
}

#[derive(Clone)]
struct SessionConfig {
    session_id: String,
    downstream: String,
    router: Ipv6Addr,
    gateway: Ipv6Addr,
    prefix_len: u8,
    reply_mark: u32,
    dns_bind_address: Ipv4Addr,
    mtu: u32,
    deprecated_prefixes: Vec<Route>,
    primary: Option<Upstream>,
    fallback: Option<Upstream>,
}

#[derive(Clone, Copy)]
struct SessionPorts {
    tcp: u16,
    udp: u16,
    dns_tcp: u16,
    dns_udp: u16,
}

struct Session {
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    ports: SessionPorts,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AssociationKey {
    client: SocketAddrV6,
    destination: SocketAddrV6,
}

struct UdpAssociation {
    id: u64,
    socket: Arc<TokioUdpSocket>,
    last_active: Instant,
    stop: CancellationToken,
}

enum UdpAssociationEvent {
    Active(AssociationKey, u64),
    Closed(AssociationKey, u64),
}

enum RaRequest {
    RouterSolicitation(SocketAddrV6),
    Ignored,
    WouldBlock,
}

struct Args {
    socket_name: String,
    connection_file: PathBuf,
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let args = parse_args()?;
    let mut controller = connect_control_socket(&args.socket_name, &args.connection_file).await?;
    eprintln!("connected to {}", args.socket_name);
    let mut sessions = HashMap::<String, Session>::new();
    loop {
        let packet = match recv_packet(&mut controller).await {
            Ok(packet) => packet,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                eprintln!("controller recv failed: {e}");
                break;
            }
        };
        let response = match handle_packet(&packet, &mut sessions).await {
            Ok(HandleResult::Reply(reply)) => Some(reply),
            Ok(HandleResult::Shutdown(reply)) => {
                if let Err(e) = send_packet(&mut controller, &reply).await {
                    eprintln!("controller send failed: {e}");
                }
                break;
            }
            Err(e) => Some(error_packet(e)),
        };
        if let Some(response) = response {
            if let Err(e) = send_packet(&mut controller, &response).await {
                eprintln!("controller send failed: {e}");
                break;
            }
        }
    }
    for session in sessions.values() {
        withdraw_session_prefixes(session).await;
        session.stop.cancel();
    }
    Ok(())
}

enum HandleResult {
    Reply(Vec<u8>),
    Shutdown(Vec<u8>),
}

async fn handle_packet(
    packet: &[u8],
    sessions: &mut HashMap<String, Session>,
) -> io::Result<HandleResult> {
    let mut parser = Parser::new(packet);
    match parser.read_u32()? {
        CMD_START_SESSION => {
            let config = parser.read_session_config()?;
            if sessions.contains_key(&config.session_id) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "session already exists",
                ));
            }
            let session = start_session(config).await?;
            let reply = ports_packet(session.ports);
            let session_id = session.config.lock().await.session_id.clone();
            sessions.insert(session_id, session);
            Ok(HandleResult::Reply(reply))
        }
        CMD_REPLACE_SESSION => {
            let config = parser.read_session_config()?;
            let session = sessions
                .get(&config.session_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session not found"))?;
            *session.config.lock().await = config;
            Ok(HandleResult::Reply(ok_packet()))
        }
        CMD_REMOVE_SESSION => {
            let session_id = parser.read_utf()?;
            if let Some(session) = sessions.remove(&session_id) {
                withdraw_session_prefixes(&session).await;
                session.stop.cancel();
            }
            Ok(HandleResult::Reply(ok_packet()))
        }
        CMD_SHUTDOWN => {
            for session in sessions.values() {
                withdraw_session_prefixes(session).await;
                session.stop.cancel();
            }
            Ok(HandleResult::Shutdown(ok_packet()))
        }
        command => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown command {command}"),
        )),
    }
}

async fn start_session(config: SessionConfig) -> io::Result<Session> {
    let stop = CancellationToken::new();
    let shared = Arc::new(Mutex::new(config.clone()));

    let tcp_listener = create_tproxy_tcp_listener(config.reply_mark)?;
    let tcp = tcp_listener.local_addr()?.port();
    let udp_listener = create_tproxy_udp_listener(config.reply_mark)?;
    let udp = udp_listener.local_addr()?.port();

    let dns_tcp_listener = TcpListener::bind(SocketAddrV4::new(config.dns_bind_address, 0))?;
    dns_tcp_listener.set_nonblocking(true)?;
    let dns_tcp = dns_tcp_listener.local_addr()?.port();
    let dns_udp_socket = UdpSocket::bind(SocketAddrV4::new(config.dns_bind_address, 0))?;
    dns_udp_socket.set_nonblocking(true)?;
    let dns_udp = dns_udp_socket.local_addr()?.port();

    spawn_tcp_loop(tcp_listener, shared.clone(), stop.clone())?;
    spawn_udp_loop(udp_listener, shared.clone(), stop.clone())?;
    spawn_dns_tcp_loop(dns_tcp_listener, shared.clone(), stop.clone())?;
    spawn_dns_udp_loop(dns_udp_socket, shared.clone(), stop.clone())?;
    spawn_ra_loop(shared.clone(), stop.clone())?;

    Ok(Session {
        config: shared,
        stop,
        ports: SessionPorts {
            tcp,
            udp,
            dns_tcp,
            dns_udp,
        },
    })
}

fn spawn_tcp_loop(
    listener: TcpListener,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = TokioTcpListener::from_std(listener)?;
    tokio::spawn(async move {
        let limit = Arc::new(Semaphore::new(TCP_WORKER_COUNT));
        loop {
            let permit = tokio::select! {
                _ = stop.cancelled() => break,
                permit = limit.clone().acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            tokio::select! {
                _ = stop.cancelled() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((socket, _)) => {
                            let config = config.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                if let Err(e) = handle_tcp_connection(socket, config).await {
                                    eprintln!("tcp proxy failed: {e}");
                                }
                            });
                        }
                        Err(e) => eprintln!("tcp accept failed: {e}"),
                    }
                }
            }
        }
    });
    Ok(())
}

async fn handle_tcp_connection(
    inbound: TokioTcpStream,
    config: Arc<Mutex<SessionConfig>>,
) -> io::Result<()> {
    let destination = match inbound.local_addr()? {
        SocketAddr::V6(destination) => destination,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected ipv6 destination",
            ))
        }
    };
    let snapshot = config.lock().await.clone();
    set_sockopt(
        inbound.as_raw_fd(),
        SOL_SOCKET,
        SO_MARK,
        &snapshot.reply_mark,
    )?;
    if destination.ip() == &snapshot.gateway && destination.port() == DNS_PORT {
        return handle_dns_tcp_connection(inbound, snapshot).await;
    }
    let upstream = select_upstream(&snapshot, *destination.ip())
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no upstream route"))?;
    let outbound = connect_upstream_tcp(&upstream, destination).await?;
    relay_tcp(inbound, outbound).await
}

async fn relay_tcp(mut inbound: TokioTcpStream, mut outbound: TokioTcpStream) -> io::Result<()> {
    tokio::io::copy_bidirectional(&mut inbound, &mut outbound)
        .await
        .map(|_| ())
}

fn spawn_udp_loop(
    listener: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = AsyncFd::new(listener)?;
    tokio::spawn(async move {
        let listener_fd = listener.get_ref().as_raw_fd();
        let mut associations = HashMap::<AssociationKey, UdpAssociation>::new();
        let (association_event_tx, mut association_event_rx) = mpsc::unbounded_channel();
        let mut next_association_id = 0u64;
        let mut buffer = [0u8; 65535];
        let handle_association_event =
            |associations: &mut HashMap<AssociationKey, UdpAssociation>, event| match event {
                UdpAssociationEvent::Active(key, id) => {
                    if let Some(association) = associations.get_mut(&key) {
                        if association.id == id {
                            association.last_active = Instant::now();
                        }
                    }
                }
                UdpAssociationEvent::Closed(key, id) => {
                    let remove = associations
                        .get(&key)
                        .map_or(false, |association| association.id == id);
                    if remove {
                        if let Some(association) = associations.remove(&key) {
                            association.stop.cancel();
                        }
                    }
                }
            };
        loop {
            while let Ok(event) = association_event_rx.try_recv() {
                handle_association_event(&mut associations, event);
            }
            let now = Instant::now();
            associations.retain(|_, association| {
                let active = now.duration_since(association.last_active) < UDP_ASSOC_IDLE;
                if !active {
                    association.stop.cancel();
                }
                active
            });
            let next_expiry = associations
                .values()
                .map(|association| association.last_active + UDP_ASSOC_IDLE)
                .min();

            tokio::select! {
                _ = stop.cancelled() => break,
                _ = async {
                    if let Some(deadline) = next_expiry {
                        time::sleep_until(time::Instant::from_std(deadline)).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
                event = association_event_rx.recv() => match event {
                    Some(event) => handle_association_event(&mut associations, event),
                    None => break,
                },
                ready = listener.readable() => {
                    let Ok(mut ready) = ready else {
                        break;
                    };
                    loop {
                        match recv_udp_packet(listener_fd, &mut buffer) {
                            Ok((size, client, destination)) => {
                                let activity = Instant::now();
                                let snapshot = config.lock().await.clone();
                                if destination.ip() == &snapshot.gateway && destination.port() == DNS_PORT {
                                    let query = buffer[..size].to_vec();
                                    let query_stop = stop.child_token();
                                    tokio::spawn(async move {
                                        tokio::select! {
                                            _ = query_stop.cancelled() => {}
                                            result = resolve_dns_query(&snapshot, &query) => match result {
                                                Ok(response) => {
                                                    if let Err(e) = send_udp_response(
                                                        destination,
                                                        client,
                                                        snapshot.reply_mark,
                                                        &response,
                                                    ).await {
                                                        eprintln!("dns udp response failed: {e}");
                                                    }
                                                }
                                                Err(e) => eprintln!("dns udp resolve failed: {e}"),
                                            }
                                        }
                                    });
                                    continue;
                                }
                                if destination.ip().is_multicast()
                                    || destination.ip().is_unicast_link_local()
                                    || destination.ip().is_loopback()
                                    || destination.ip().is_unspecified()
                                {
                                    continue;
                                }
                                let upstream = match select_upstream(&snapshot, *destination.ip()) {
                                    Some(network) => network,
                                    None => continue,
                                };
                                let key = AssociationKey { client, destination };
                                if !associations.contains_key(&key) {
                                    while associations.len() >= UDP_ASSOC_MAX {
                                        let stale = associations
                                            .iter()
                                            .min_by_key(|(_, association)| association.last_active)
                                            .map(|(key, _)| *key);
                                        if let Some(stale) = stale {
                                            if let Some(association) = associations.remove(&stale) {
                                                association.stop.cancel();
                                            }
                                        } else {
                                            break;
                                        }
                                    }
                                    let upstream = match connect_upstream_udp(&upstream, destination).await {
                                        Ok(socket) => Arc::new(socket),
                                        Err(e) => {
                                            eprintln!("udp connect failed: {e}");
                                            continue;
                                        }
                                    };
                                    let association_stop = stop.child_token();
                                    let association_id = next_association_id;
                                    next_association_id = next_association_id.wrapping_add(1);
                                    tokio::spawn(run_udp_association(
                                        key,
                                        association_id,
                                        upstream.clone(),
                                        config.clone(),
                                        association_stop.clone(),
                                        association_event_tx.clone(),
                                    ));
                                    associations.insert(
                                        key,
                                        UdpAssociation {
                                            id: association_id,
                                            socket: upstream,
                                            last_active: activity,
                                            stop: association_stop,
                                        },
                                    );
                                }
                                let association = associations.get_mut(&key).unwrap();
                                association.last_active = activity;
                                if let Err(e) = association.socket.send(&buffer[..size]).await {
                                    eprintln!("udp send failed: {e}");
                                    if let Some(association) = associations.remove(&key) {
                                        association.stop.cancel();
                                    }
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                ready.clear_ready();
                                break;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                eprintln!("udp recv failed: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        }
        for association in associations.values() {
            association.stop.cancel();
        }
    });
    Ok(())
}

async fn run_udp_association(
    key: AssociationKey,
    id: u64,
    socket: Arc<TokioUdpSocket>,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    association_event_tx: mpsc::UnboundedSender<UdpAssociationEvent>,
) {
    let mut buffer = [0u8; 65535];
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            result = socket.recv(&mut buffer) => match result {
                Ok(size) => {
                    let mark = config.lock().await.reply_mark;
                    if let Err(e) = send_udp_response(key.destination, key.client, mark, &buffer[..size]).await {
                        eprintln!("udp response failed: {e}");
                        break;
                    }
                    let _ = association_event_tx.send(UdpAssociationEvent::Active(key, id));
                }
                Err(e) => {
                    eprintln!("udp upstream recv failed: {e}");
                    break;
                }
            }
        }
    }
    stop.cancel();
    let _ = association_event_tx.send(UdpAssociationEvent::Closed(key, id));
}

async fn send_udp_response(
    source: SocketAddrV6,
    target: SocketAddrV6,
    mark: u32,
    payload: &[u8],
) -> io::Result<()> {
    let socket = TokioUdpSocket::from_std(create_udp_reply_socket(source, mark)?)?;
    socket.send_to(payload, SocketAddr::V6(target)).await?;
    Ok(())
}

fn spawn_dns_tcp_loop(
    listener: TcpListener,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = TokioTcpListener::from_std(listener)?;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((socket, _)) => {
                        let config = config.clone();
                        tokio::spawn(async move {
                            let snapshot = config.lock().await.clone();
                            if let Err(e) = handle_dns_tcp_connection(socket, snapshot).await {
                                eprintln!("dns tcp failed: {e}");
                            }
                        });
                    }
                    Err(e) => eprintln!("dns tcp accept failed: {e}"),
                }
            }
        }
    });
    Ok(())
}

async fn handle_dns_tcp_connection(
    mut socket: TokioTcpStream,
    config: SessionConfig,
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
        let response = resolve_dns_query(&config, &query).await?;
        socket
            .write_all(&(response.len() as u16).to_be_bytes())
            .await?;
        socket.write_all(&response).await?;
        socket.flush().await?;
    }
}

fn spawn_dns_udp_loop(
    socket: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let socket = Arc::new(TokioUdpSocket::from_std(socket)?);
    tokio::spawn(async move {
        let mut buffer = [0u8; 65535];
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                received = socket.recv_from(&mut buffer) => match received {
                    Ok((size, source)) => {
                        let snapshot = config.lock().await.clone();
                        let query = buffer[..size].to_vec();
                        let socket = socket.clone();
                        let query_stop = stop.child_token();
                        tokio::spawn(async move {
                            tokio::select! {
                                _ = query_stop.cancelled() => {}
                                result = resolve_dns_query(&snapshot, &query) => match result {
                                    Ok(response) => {
                                        let _ = socket.send_to(&response, source).await;
                                    }
                                    Err(e) => eprintln!("dns udp resolve failed: {e}"),
                                }
                            }
                        });
                    },
                    Err(e) => eprintln!("dns udp recv failed: {e}"),
                }
            };
        }
    });
    Ok(())
}

fn spawn_ra_loop(config: Arc<Mutex<SessionConfig>>, stop: CancellationToken) -> io::Result<()> {
    tokio::spawn(async move {
        let snapshot = config.lock().await.clone();
        let socket = match create_ra_recv_socket(&snapshot.downstream, snapshot.reply_mark) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("ra socket failed: {e}");
                return;
            }
        };
        let socket = unsafe { OwnedFd::from_raw_fd(socket) };
        let socket = match AsyncFd::new(socket) {
            Ok(socket) => socket,
            Err(e) => {
                eprintln!("ra async socket failed: {e}");
                return;
            }
        };
        let mut last_sent: Option<Instant> = None;
        let mut last_deprecated_sent: Option<Instant> = None;
        let mut deprecated_prefixes = HashMap::<Route, Instant>::new();
        let mut buffer = [0u8; 1500];
        loop {
            let now = Instant::now();
            let (current, send_current) = {
                let mut current = config.lock().await;
                let new_deprecated_prefixes = std::mem::take(&mut current.deprecated_prefixes);
                let send_current = !new_deprecated_prefixes.is_empty();
                for prefix in new_deprecated_prefixes {
                    deprecated_prefixes.insert(prefix, now + DEPRECATED_RA_WINDOW);
                }
                (current.clone(), send_current)
            };
            deprecated_prefixes.retain(|_, deadline| *deadline > now);
            if !deprecated_prefixes.is_empty()
                && last_deprecated_sent.map_or(true, |last| last.elapsed() >= DEPRECATED_RA_PERIOD)
            {
                withdraw_prefixes_once(
                    &current,
                    &deprecated_prefixes.keys().copied().collect::<Vec<_>>(),
                    true,
                )
                .await;
                last_deprecated_sent = Some(now);
            }
            if send_current || last_sent.map_or(true, |last| last.elapsed() >= RA_PERIOD) {
                let _ = send_ra(&current, None).await;
                last_sent = Some(now);
            }
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = time::sleep(LOOP_SLEEP) => {}
                ready = socket.readable() => {
                    let Ok(mut ready) = ready else {
                        break;
                    };
                    loop {
                        match recv_ra_request(socket.get_ref().as_raw_fd(), &mut buffer) {
                            Ok(RaRequest::RouterSolicitation(source)) => {
                                let _ = send_ra(&current, Some(source)).await;
                            }
                            Ok(RaRequest::Ignored) => continue,
                            Ok(RaRequest::WouldBlock) => {
                                ready.clear_ready();
                                break;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                eprintln!("ra recv failed: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        }
    });
    Ok(())
}

fn create_ra_recv_socket(interface: &str, mark: u32) -> io::Result<RawFd> {
    let fd = create_socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6)?;
    let hops = 255;
    let binding = CString::new(interface)?.into_bytes_with_nul();
    set_sockopt_bytes(fd, SOL_SOCKET, SO_BINDTODEVICE, &binding)?;
    set_sockopt(fd, SOL_SOCKET, SO_MARK, &mark)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &hops)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &hops)?;
    set_nonblocking(fd)?;
    Ok(fd)
}

fn create_ra_send_socket(interface: &str, mark: u32, router: Ipv6Addr) -> io::Result<RawFd> {
    let fd = create_socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6)?;
    let hops = 255;
    let binding = CString::new(interface)?.into_bytes_with_nul();
    set_sockopt_bytes(fd, SOL_SOCKET, SO_BINDTODEVICE, &binding)?;
    set_sockopt(fd, SOL_SOCKET, SO_MARK, &mark)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &hops)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_MULTICAST_HOPS, &hops)?;
    let scope_id = if router.is_unicast_link_local() {
        interface_index(interface)?
    } else {
        0
    };
    syscall(unsafe {
        bind(
            fd,
            &raw_addr_v6(SocketAddrV6::new(router, 0, 0, scope_id)) as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    })?;
    set_nonblocking(fd)?;
    Ok(fd)
}

fn recv_ra_request(fd: RawFd, buffer: &mut [u8]) -> io::Result<RaRequest> {
    let mut address: SockAddrIn6 = unsafe { zeroed() };
    let mut length = size_of::<SockAddrIn6>() as u32;
    let size = unsafe {
        recvfrom(
            fd,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len(),
            MSG_DONTWAIT,
            &mut address as *mut _ as *mut SockAddr,
            &mut length,
        )
    };
    if size < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            return Ok(RaRequest::WouldBlock);
        }
        return Err(error);
    }
    if size == 0 || buffer[0] != 133 {
        return Ok(RaRequest::Ignored);
    }
    Ok(RaRequest::RouterSolicitation(socket_addr_v6_from_raw(
        address,
    )))
}

async fn send_ra(config: &SessionConfig, target: Option<SocketAddrV6>) -> io::Result<()> {
    let fd = create_ra_send_socket(&config.downstream, config.reply_mark, config.router)?;
    let ifindex = interface_index(&config.downstream)?;
    let destination =
        target.unwrap_or_else(|| SocketAddrV6::new("ff02::1".parse().unwrap(), 0, 0, ifindex));
    let packet = make_ra_packet(config.gateway, config.prefix_len, config.mtu);
    let result = sendto_all(
        fd,
        &packet,
        raw_addr_v6(destination),
        size_of::<SockAddrIn6>() as u32,
    )
    .await;
    unsafe {
        close(fd);
    }
    result
}

async fn send_deprecated_ra(
    fd: RawFd,
    config: &SessionConfig,
    prefix: Route,
    keep_router: bool,
) -> io::Result<()> {
    let ifindex = interface_index(&config.downstream)?;
    let destination = SocketAddrV6::new("ff02::1".parse().unwrap(), 0, 0, ifindex);
    let deprecated_gateway = Ipv6Addr::from(prefix.prefix);
    let packet = make_ra_packet_with_lifetimes(
        deprecated_gateway,
        deprecated_gateway,
        prefix.prefix_len,
        config.mtu,
        if keep_router { 1800 } else { 0 },
        0,
        0,
        0,
    );
    sendto_all(
        fd,
        &packet,
        raw_addr_v6(destination),
        size_of::<SockAddrIn6>() as u32,
    )
    .await
}

async fn withdraw_prefixes_once(config: &SessionConfig, prefixes: &[Route], keep_router: bool) {
    let fd = match create_ra_send_socket(&config.downstream, config.reply_mark, config.router) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("ra send socket failed: {e}");
            return;
        }
    };
    for prefix in prefixes.iter().cloned() {
        let _ = send_deprecated_ra(fd, config, prefix, keep_router).await;
    }
    unsafe {
        close(fd);
    }
}

async fn withdraw_session_prefixes(session: &Session) {
    let snapshot = session.config.lock().await.clone();
    let mut prefixes = snapshot.deprecated_prefixes.clone();
    prefixes.push(Route {
        prefix: ipv6_to_u128(snapshot.gateway),
        prefix_len: snapshot.prefix_len,
    });
    withdraw_prefixes_once(&snapshot, &prefixes, false).await;
}

fn make_ra_packet(gateway: Ipv6Addr, prefix_len: u8, mtu: u32) -> Vec<u8> {
    make_ra_packet_with_lifetimes(gateway, gateway, prefix_len, mtu, 1800, 3600, 1800, 600)
}

fn make_ra_packet_with_lifetimes(
    dns_server: Ipv6Addr,
    advertised_prefix: Ipv6Addr,
    prefix_len: u8,
    mtu: u32,
    router_lifetime: u16,
    valid_lifetime: u32,
    preferred_lifetime: u32,
    rdnss_lifetime: u32,
) -> Vec<u8> {
    let mut packet = Vec::with_capacity(80);
    packet.push(134);
    packet.push(0);
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.push(64);
    packet.push(0);
    packet.extend_from_slice(&router_lifetime.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());

    packet.push(3);
    packet.push(4);
    packet.push(prefix_len);
    packet.push(0xc0);
    packet.extend_from_slice(&valid_lifetime.to_be_bytes());
    packet.extend_from_slice(&preferred_lifetime.to_be_bytes());
    packet.extend_from_slice(&0u32.to_be_bytes());
    packet.extend_from_slice(&network_prefix(advertised_prefix, prefix_len));

    packet.push(5);
    packet.push(1);
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&mtu.to_be_bytes());

    packet.push(25);
    packet.push(3);
    packet.extend_from_slice(&0u16.to_be_bytes());
    packet.extend_from_slice(&rdnss_lifetime.to_be_bytes());
    packet.extend_from_slice(&dns_server.octets());
    packet
}

async fn resolve_dns_query(config: &SessionConfig, query: &[u8]) -> io::Result<Vec<u8>> {
    if let Some(primary) = config.primary.as_ref() {
        match query_network(primary.network_handle, query).await {
            Ok(response) => return Ok(response),
            Err(e) => eprintln!("dns primary failed: {e}"),
        }
    }
    if let Some(fallback) = config.fallback.as_ref() {
        return query_network(fallback.network_handle, query).await;
    }
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "no DNS upstream",
    ))
}

async fn query_network(network_handle: u64, query: &[u8]) -> io::Result<Vec<u8>> {
    let fd = unsafe {
        android_res_nsend(
            network_handle,
            query.as_ptr(),
            query.len(),
            ANDROID_RESOLV_NO_RETRY,
        )
    };
    if fd < 0 {
        return Err(io::Error::from_raw_os_error(-fd));
    }
    let fd = ResolverQuery { fd: Some(fd) };
    set_nonblocking(fd.as_raw_fd())?;
    let fd = AsyncFd::new(fd)?;
    let _ = fd.readable().await?;
    let mut response = vec![0u8; 4096];
    let mut rcode = 0;
    let size = fd.into_inner().finish(&mut rcode, &mut response);
    if size < 0 {
        return Err(io::Error::from_raw_os_error(-size));
    }
    response.truncate(size as usize);
    Ok(response)
}

fn create_tproxy_tcp_listener(mark: u32) -> io::Result<TcpListener> {
    let fd = create_socket(AF_INET6, SOCK_STREAM, 0)?;
    let one = 1;
    set_sockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one)?;
    set_sockopt(fd, SOL_SOCKET, SO_MARK, &mark)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &one)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_TRANSPARENT, &one)?;
    bind_v6_any(fd)?;
    syscall(unsafe { listen(fd, 32) })?;
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn create_tproxy_udp_listener(mark: u32) -> io::Result<UdpSocket> {
    let fd = create_socket(AF_INET6, SOCK_DGRAM, 0)?;
    let one = 1;
    set_sockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one)?;
    set_sockopt(fd, SOL_SOCKET, SO_MARK, &mark)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &one)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_TRANSPARENT, &one)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_RECVORIGDSTADDR, &one)?;
    bind_v6_any(fd)?;
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    socket.set_nonblocking(true)?;
    Ok(socket)
}

async fn connect_upstream_tcp(
    upstream: &Upstream,
    destination: SocketAddrV6,
) -> io::Result<TokioTcpStream> {
    let fd = unsafe { OwnedFd::from_raw_fd(create_socket(AF_INET6, SOCK_STREAM, 0)?) };
    if unsafe { android_setsocknetwork(upstream.network_handle, fd.as_raw_fd()) } != 0 {
        let error = io::Error::last_os_error();
        return Err(error);
    }
    bind_upstream_socket(fd.as_raw_fd(), &upstream.interface)?;
    set_nonblocking(fd.as_raw_fd())?;
    let connect_result = unsafe {
        connect(
            fd.as_raw_fd(),
            &raw_addr_v6(destination) as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    };
    if connect_result < 0 {
        let error = io::Error::last_os_error();
        let Some(raw_os_error) = error.raw_os_error() else {
            return Err(error);
        };
        if error.kind() != io::ErrorKind::WouldBlock && raw_os_error != EINPROGRESS {
            return Err(error);
        }
        wait_for_socket_connect(fd.as_raw_fd(), TCP_CONNECT_TIMEOUT).await?;
    }
    TokioTcpStream::from_std(unsafe { TcpStream::from_raw_fd(fd.into_raw_fd()) })
}

async fn connect_upstream_udp(
    upstream: &Upstream,
    destination: SocketAddrV6,
) -> io::Result<TokioUdpSocket> {
    let fd = unsafe { OwnedFd::from_raw_fd(create_socket(AF_INET6, SOCK_DGRAM, 0)?) };
    if unsafe { android_setsocknetwork(upstream.network_handle, fd.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    bind_upstream_socket(fd.as_raw_fd(), &upstream.interface)?;
    syscall(unsafe {
        connect(
            fd.as_raw_fd(),
            &raw_addr_v6(destination) as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    })?;
    set_nonblocking(fd.as_raw_fd())?;
    TokioUdpSocket::from_std(unsafe { UdpSocket::from_raw_fd(fd.into_raw_fd()) })
}

fn bind_upstream_socket(fd: RawFd, interface: &str) -> io::Result<()> {
    if interface.is_empty() {
        return Ok(());
    }
    let binding = CString::new(interface)?.into_bytes_with_nul();
    set_sockopt_bytes(fd, SOL_SOCKET, SO_BINDTODEVICE, &binding)
}

fn create_udp_reply_socket(source: SocketAddrV6, mark: u32) -> io::Result<UdpSocket> {
    let fd = create_socket(AF_INET6, SOCK_DGRAM, 0)?;
    let one = 1;
    set_sockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one)?;
    set_sockopt(fd, SOL_SOCKET, SO_MARK, &mark)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_TRANSPARENT, &one)?;
    syscall(unsafe {
        bind(
            fd,
            &raw_addr_v6(source) as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    })?;
    set_nonblocking(fd)?;
    Ok(unsafe { UdpSocket::from_raw_fd(fd) })
}

fn recv_udp_packet(
    fd: RawFd,
    buffer: &mut [u8],
) -> io::Result<(usize, SocketAddrV6, SocketAddrV6)> {
    let mut source: SockAddrIn6 = unsafe { zeroed() };
    let source_len = size_of::<SockAddrIn6>() as u32;
    let mut control = [0u8; 128];
    let mut iov = Iovec {
        iov_base: buffer.as_mut_ptr() as *mut c_void,
        iov_len: buffer.len(),
    };
    let mut message = MsgHdr {
        msg_name: &mut source as *mut _ as *mut c_void,
        msg_namelen: source_len,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr() as *mut c_void,
        msg_controllen: control.len(),
        msg_flags: 0,
    };
    let size = unsafe { recvmsg(fd, &mut message, MSG_DONTWAIT) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let source = socket_addr_v6_from_raw(source);
    let mut destination = None;
    let mut current = first_cmsg(&message);
    while !current.is_null() {
        unsafe {
            if (*current).cmsg_level == IPPROTO_IPV6 && (*current).cmsg_type == IPV6_RECVORIGDSTADDR
            {
                let raw = cmsg_data(current) as *const SockAddrIn6;
                destination = Some(socket_addr_v6_from_raw(*raw));
                break;
            }
            current = next_cmsg(&message, current);
        }
    }
    let destination = destination.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing original destination")
    })?;
    Ok((size as usize, source, destination))
}

async fn connect_control_socket(
    socket_name: &str,
    connection_file: &PathBuf,
) -> io::Result<UnixStream> {
    let address = abstract_unix_addr(socket_name)?;
    let length = abstract_unix_addr_len(socket_name);
    let start = Instant::now();
    loop {
        if !connection_file.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "connection file missing",
            ));
        }
        let fd = create_socket(AF_UNIX, SOCK_STREAM, 0)?;
        set_nonblocking(fd)?;
        if unsafe { connect(fd, &address as *const _ as *const SockAddr, length) } == 0 {
            return UnixStream::from_std(unsafe { StdUnixStream::from_raw_fd(fd) });
        }
        let error = io::Error::last_os_error();
        let raw_os_error = error.raw_os_error();
        if error.kind() == io::ErrorKind::WouldBlock || raw_os_error == Some(EINPROGRESS) {
            match wait_for_socket_connect(
                fd,
                DAEMON_STARTUP_TIMEOUT.saturating_sub(start.elapsed()),
            )
            .await
            {
                Ok(()) => return UnixStream::from_std(unsafe { StdUnixStream::from_raw_fd(fd) }),
                Err(e) => {
                    unsafe {
                        close(fd);
                    }
                    if e.kind() != io::ErrorKind::TimedOut
                        && start.elapsed() < DAEMON_STARTUP_TIMEOUT
                    {
                        time::sleep(LOOP_SLEEP).await;
                        continue;
                    }
                    return Err(e);
                }
            }
        } else {
            unsafe {
                close(fd);
            }
        }
        if start.elapsed() >= DAEMON_STARTUP_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("control socket connect timed out: {error}"),
            ));
        }
        time::sleep(LOOP_SLEEP).await;
    }
}

async fn recv_packet(socket: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    socket.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_CONTROL_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid control frame length {length}"),
        ));
    }
    let mut buffer = vec![0u8; length];
    socket.read_exact(&mut buffer).await?;
    Ok(buffer)
}

async fn send_packet(socket: &mut UnixStream, packet: &[u8]) -> io::Result<()> {
    if packet.is_empty() || packet.len() > MAX_CONTROL_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid control frame length {}", packet.len()),
        ));
    }
    socket
        .write_all(&(packet.len() as u32).to_be_bytes())
        .await?;
    socket.write_all(packet).await?;
    socket.flush().await
}

fn parse_args() -> io::Result<Args> {
    let mut args = env::args().skip(1);
    let mut socket_name = None;
    let mut connection_file = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket-name" => socket_name = args.next(),
            "--connection-file" => connection_file = args.next().map(PathBuf::from),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument {arg}"),
                ))
            }
        }
    }
    Ok(Args {
        socket_name: socket_name
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing --socket-name"))?,
        connection_file: connection_file.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing --connection-file")
        })?,
    })
}

fn select_upstream(config: &SessionConfig, destination: Ipv6Addr) -> Option<Upstream> {
    let destination = ipv6_to_u128(destination);
    let mut best = None;
    if let Some(primary) = config.primary.as_ref() {
        if let Some(prefix_len) = longest_prefix_match(&primary.routes, destination) {
            best = Some((prefix_len, primary.clone()));
        }
    }
    if let Some(fallback) = config.fallback.as_ref() {
        if let Some(prefix_len) = longest_prefix_match(&fallback.routes, destination) {
            if best
                .as_ref()
                .map_or(true, |(current, _)| prefix_len > *current)
            {
                best = Some((prefix_len, fallback.clone()));
            }
        }
    }
    best.map(|(_, upstream)| upstream)
}

fn longest_prefix_match(routes: &[Route], destination: u128) -> Option<u8> {
    routes
        .iter()
        .filter(|route| prefix_matches(destination, route.prefix, route.prefix_len))
        .map(|route| route.prefix_len)
        .max()
}

fn prefix_matches(destination: u128, prefix: u128, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        true
    } else {
        let shift = 128 - prefix_len as u32;
        destination >> shift == prefix >> shift
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deprecated_ra_uses_deprecated_dns_server() {
        let dns_server: Ipv6Addr = "fd47:6b7c:2186:b452::1".parse().unwrap();
        let packet = make_ra_packet_with_lifetimes(dns_server, dns_server, 64, 1500, 0, 0, 0, 0);
        assert_eq!(&packet[40..44], &0u32.to_be_bytes());
        assert_eq!(&packet[packet.len() - 16..], &dns_server.octets());
    }
}

fn network_prefix(address: Ipv6Addr, prefix_len: u8) -> [u8; 16] {
    let shift = 128u32.saturating_sub(prefix_len as u32);
    (ipv6_to_u128(address) & (!0u128 << shift)).to_be_bytes()
}

fn ipv6_to_u128(address: Ipv6Addr) -> u128 {
    u128::from_be_bytes(address.octets())
}

fn bind_v6_any(fd: RawFd) -> io::Result<()> {
    let address = SockAddrIn6 {
        sin6_family: AF_INET6 as u16,
        sin6_port: 0,
        sin6_flowinfo: 0,
        sin6_addr: In6Addr { s6_addr: [0; 16] },
        sin6_scope_id: 0,
    };
    syscall(unsafe {
        bind(
            fd,
            &address as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    })
}

fn abstract_unix_addr(name: &str) -> io::Result<SockAddrUn> {
    let bytes = name.as_bytes();
    if bytes.len() + 1 > 108 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket name too long",
        ));
    }
    let mut address: SockAddrUn = unsafe { zeroed() };
    address.sun_family = AF_UNIX as u16;
    for (index, byte) in bytes.iter().enumerate() {
        address.sun_path[index + 1] = *byte as c_char;
    }
    Ok(address)
}

fn abstract_unix_addr_len(name: &str) -> u32 {
    (size_of::<u16>() + 1 + name.len()) as u32
}

fn raw_addr_v6(address: SocketAddrV6) -> SockAddrIn6 {
    SockAddrIn6 {
        sin6_family: AF_INET6 as u16,
        sin6_port: address.port().to_be(),
        sin6_flowinfo: address.flowinfo(),
        sin6_addr: In6Addr {
            s6_addr: address.ip().octets(),
        },
        sin6_scope_id: address.scope_id(),
    }
}

fn socket_addr_v6_from_raw(address: SockAddrIn6) -> SocketAddrV6 {
    SocketAddrV6::new(
        Ipv6Addr::from(address.sin6_addr.s6_addr),
        u16::from_be(address.sin6_port),
        address.sin6_flowinfo,
        address.sin6_scope_id,
    )
}

fn interface_index(interface: &str) -> io::Result<u32> {
    let name = CString::new(interface)?;
    let index = unsafe { if_nametoindex(name.as_ptr()) };
    if index == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(index)
    }
}

fn create_socket(domain: c_int, ty: c_int, protocol: c_int) -> io::Result<RawFd> {
    let fd = unsafe { socket(domain, ty, protocol) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

fn set_sockopt<T>(fd: RawFd, level: c_int, optname: c_int, value: &T) -> io::Result<()> {
    syscall(unsafe {
        setsockopt(
            fd,
            level,
            optname,
            value as *const T as *const c_void,
            size_of::<T>() as u32,
        )
    })
}

fn set_sockopt_bytes(fd: RawFd, level: c_int, optname: c_int, value: &[u8]) -> io::Result<()> {
    syscall(unsafe {
        setsockopt(
            fd,
            level,
            optname,
            value.as_ptr() as *const c_void,
            value.len() as u32,
        )
    })
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    syscall(unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) })
}

async fn wait_for_socket_connect(fd: RawFd, timeout: Duration) -> io::Result<()> {
    if time::timeout(timeout, wait_for_output(fd)).await.is_err() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "tcp connect timed out",
        ));
    };
    let mut socket_error = 0;
    let mut socket_error_len = size_of::<c_int>() as u32;
    syscall(unsafe {
        getsockopt(
            fd,
            SOL_SOCKET,
            SO_ERROR,
            &mut socket_error as *mut _ as *mut c_void,
            &mut socket_error_len,
        )
    })?;
    if socket_error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(socket_error))
    }
}

async fn wait_for_output(fd: RawFd) -> io::Result<()> {
    let fd = AsyncFd::new(BorrowedRawFd(fd))?;
    let _ = fd.writable().await?;
    Ok(())
}

fn syscall(result: c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn sendto_all(
    fd: RawFd,
    mut packet: &[u8],
    address: SockAddrIn6,
    length: u32,
) -> io::Result<()> {
    while !packet.is_empty() {
        let written = unsafe {
            sendto(
                fd,
                packet.as_ptr() as *const c_void,
                packet.len(),
                0,
                &address as *const _ as *const SockAddr,
                length,
            )
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                wait_for_output(fd).await?;
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "socket write failed",
            ));
        }
        packet = &packet[written as usize..];
    }
    Ok(())
}

fn first_cmsg(message: &MsgHdr) -> *const Cmsghdr {
    if message.msg_controllen < size_of::<Cmsghdr>() {
        null()
    } else {
        message.msg_control as *const Cmsghdr
    }
}

unsafe fn next_cmsg(message: &MsgHdr, current: *const Cmsghdr) -> *const Cmsghdr {
    let next = current as usize + cmsg_align((*current).cmsg_len);
    let end = message.msg_control as usize + message.msg_controllen;
    if next + size_of::<Cmsghdr>() > end {
        null()
    } else {
        next as *const Cmsghdr
    }
}

unsafe fn cmsg_data(current: *const Cmsghdr) -> *const c_void {
    (current as *const u8).add(cmsg_align(size_of::<Cmsghdr>())) as *const c_void
}

fn cmsg_align(length: usize) -> usize {
    let align = size_of::<usize>();
    (length + align - 1) & !(align - 1)
}

fn ok_packet() -> Vec<u8> {
    vec![STATUS_OK]
}

fn ports_packet(ports: SessionPorts) -> Vec<u8> {
    let mut packet = vec![STATUS_OK];
    packet.extend_from_slice(&ports.tcp.to_be_bytes());
    packet.extend_from_slice(&ports.udp.to_be_bytes());
    packet.extend_from_slice(&ports.dns_tcp.to_be_bytes());
    packet.extend_from_slice(&ports.dns_udp.to_be_bytes());
    packet
}

fn error_packet(error: io::Error) -> Vec<u8> {
    let mut packet = vec![STATUS_ERROR];
    let message = error.to_string().into_bytes();
    packet.extend_from_slice(&(message.len() as u16).to_be_bytes());
    packet.extend_from_slice(&message);
    packet
}

struct Parser<'a> {
    packet: &'a [u8],
    offset: usize,
}

impl<'a> Parser<'a> {
    fn new(packet: &'a [u8]) -> Self {
        Self { packet, offset: 0 }
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        let bytes = self.read_exact(4)?;
        Ok(u32::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_u64(&mut self) -> io::Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_i32(&mut self) -> io::Result<i32> {
        Ok(self.read_u32()? as i32)
    }

    fn read_u16(&mut self) -> io::Result<u16> {
        let bytes = self.read_exact(2)?;
        Ok(u16::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        let bytes = self.read_exact(1)?;
        Ok(bytes[0])
    }

    fn read_bool(&mut self) -> io::Result<bool> {
        Ok(self.read_u8()? != 0)
    }

    fn read_utf(&mut self) -> io::Result<String> {
        let length = self.read_u16()? as usize;
        let bytes = self.read_exact(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"))
    }

    fn read_session_config(&mut self) -> io::Result<SessionConfig> {
        let session_id = self.read_utf()?;
        let _generation_id = self.read_i32()?;
        let downstream = self.read_utf()?;
        let router = self
            .read_utf()?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid router address"))?;
        let gateway = self
            .read_utf()?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid gateway address"))?;
        let prefix_len = self.read_i32()? as u8;
        let reply_mark = self.read_u32()?;
        let dns_bind_address = self
            .read_utf()?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid dns bind address"))?;
        let mtu = self.read_i32()? as u32;
        let deprecated_prefixes = self.read_routes()?;
        let primary = self.read_upstream()?;
        let fallback = self.read_upstream()?;
        Ok(SessionConfig {
            session_id,
            downstream,
            router,
            gateway,
            prefix_len,
            reply_mark,
            dns_bind_address,
            mtu,
            deprecated_prefixes,
            primary,
            fallback,
        })
    }

    fn read_upstream(&mut self) -> io::Result<Option<Upstream>> {
        if !self.read_bool()? {
            return Ok(None);
        }
        let network_handle = self.read_u64()?;
        let interface = self.read_utf()?;
        let dns_servers = self.read_i32()? as usize;
        for _ in 0..dns_servers {
            let _ = self.read_utf()?;
        }
        Ok(Some(Upstream {
            network_handle,
            interface,
            routes: self.read_routes()?,
        }))
    }

    fn read_routes(&mut self) -> io::Result<Vec<Route>> {
        let routes = self.read_i32()? as usize;
        let mut parsed = Vec::with_capacity(routes);
        for _ in 0..routes {
            let address: Ipv6Addr = self
                .read_utf()?
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid route address"))?;
            let prefix_len = self.read_i32()? as u8;
            parsed.push(Route {
                prefix: ipv6_to_u128(address),
                prefix_len,
            });
        }
        Ok(parsed)
    }

    fn read_exact(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.offset + count > self.packet.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short packet"));
        }
        let bytes = &self.packet[self.offset..self.offset + count];
        self.offset += count;
        Ok(bytes)
    }
}
