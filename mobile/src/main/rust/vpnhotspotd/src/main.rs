use std::collections::HashMap;
use std::env;
use std::ffi::{c_char, c_int, c_uint, c_void, CString};
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::net::{
    Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, SocketAddrV4, SocketAddrV6, TcpListener, TcpStream,
    UdpSocket,
};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::PathBuf;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
const MSG_NOSIGNAL: c_int = 0x4000;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;

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

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

#[link(name = "android")]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
    fn android_res_nsend(network: u64, msg: *const u8, msglen: usize, flags: u32) -> c_int;
    fn android_res_nresult(fd: c_int, rcode: *mut c_int, answer: *mut u8, anslen: usize) -> c_int;
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
    fn poll(fds: *mut PollFd, nfds: usize, timeout: c_int) -> c_int;
    fn recv(fd: c_int, buf: *mut c_void, len: usize, flags: c_int) -> isize;
    fn recvfrom(
        fd: c_int,
        buf: *mut c_void,
        len: usize,
        flags: c_int,
        addr: *mut SockAddr,
        addrlen: *mut u32,
    ) -> isize;
    fn recvmsg(fd: c_int, msg: *mut MsgHdr, flags: c_int) -> isize;
    fn send(fd: c_int, buf: *const c_void, len: usize, flags: c_int) -> isize;
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
    stop: Arc<AtomicBool>,
    ports: SessionPorts,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AssociationKey {
    client: SocketAddrV6,
    destination: SocketAddrV6,
}

struct UdpAssociation {
    socket: UdpSocket,
    last_active: Instant,
}

struct Args {
    socket_name: String,
    connection_file: PathBuf,
}

fn main() -> io::Result<()> {
    let args = parse_args()?;
    let controller = connect_control_socket(&args.socket_name, &args.connection_file)?;
    eprintln!("connected to {}", args.socket_name);
    let sessions = Arc::new(Mutex::new(HashMap::<String, Session>::new()));
    loop {
        let packet = match recv_packet(controller) {
            Ok(packet) => packet,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                eprintln!("controller recv failed: {e}");
                break;
            }
        };
        let response = match handle_packet(&packet, &sessions) {
            Ok(HandleResult::Reply(reply)) => Some(reply),
            Ok(HandleResult::Shutdown(reply)) => {
                if let Err(e) = send_packet(controller, &reply) {
                    eprintln!("controller send failed: {e}");
                }
                break;
            }
            Err(e) => Some(error_packet(e)),
        };
        if let Some(response) = response {
            if let Err(e) = send_packet(controller, &response) {
                eprintln!("controller send failed: {e}");
                break;
            }
        }
    }
    for session in sessions.lock().unwrap().values() {
        session.stop.store(true, Ordering::Relaxed);
    }
    unsafe {
        close(controller);
    }
    Ok(())
}

enum HandleResult {
    Reply(Vec<u8>),
    Shutdown(Vec<u8>),
}

fn handle_packet(
    packet: &[u8],
    sessions: &Arc<Mutex<HashMap<String, Session>>>,
) -> io::Result<HandleResult> {
    let mut parser = Parser::new(packet);
    match parser.read_u32()? {
        CMD_START_SESSION => {
            let config = parser.read_session_config()?;
            let mut sessions = sessions.lock().unwrap();
            if sessions.contains_key(&config.session_id) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "session already exists",
                ));
            }
            let session = start_session(config)?;
            let reply = ports_packet(session.ports);
            let session_id = session.config.lock().unwrap().session_id.clone();
            sessions.insert(session_id, session);
            Ok(HandleResult::Reply(reply))
        }
        CMD_REPLACE_SESSION => {
            let config = parser.read_session_config()?;
            let sessions = sessions.lock().unwrap();
            let session = sessions
                .get(&config.session_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session not found"))?;
            *session.config.lock().unwrap() = config;
            Ok(HandleResult::Reply(ok_packet()))
        }
        CMD_REMOVE_SESSION => {
            let session_id = parser.read_utf()?;
            if let Some(session) = sessions.lock().unwrap().remove(&session_id) {
                withdraw_session_prefixes(&session);
                session.stop.store(true, Ordering::Relaxed);
            }
            Ok(HandleResult::Reply(ok_packet()))
        }
        CMD_SHUTDOWN => {
            for session in sessions.lock().unwrap().values() {
                withdraw_session_prefixes(session);
                session.stop.store(true, Ordering::Relaxed);
            }
            Ok(HandleResult::Shutdown(ok_packet()))
        }
        command => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown command {command}"),
        )),
    }
}

fn start_session(config: SessionConfig) -> io::Result<Session> {
    let stop = Arc::new(AtomicBool::new(false));
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

    spawn_tcp_loop(tcp_listener, shared.clone(), stop.clone());
    spawn_udp_loop(udp_listener, shared.clone(), stop.clone());
    spawn_dns_tcp_loop(dns_tcp_listener, shared.clone(), stop.clone());
    spawn_dns_udp_loop(dns_udp_socket, shared.clone(), stop.clone());
    spawn_ra_loop(shared.clone(), stop.clone());

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

fn spawn_tcp_loop(listener: TcpListener, config: Arc<Mutex<SessionConfig>>, stop: Arc<AtomicBool>) {
    let (sender, receiver) = sync_channel::<TcpStream>(TCP_WORKER_COUNT);
    let receiver = Arc::new(Mutex::new(receiver));
    for _ in 0..TCP_WORKER_COUNT {
        let config = config.clone();
        let receiver = receiver.clone();
        let stop = stop.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let socket = match receiver.lock().unwrap().recv_timeout(LOOP_SLEEP) {
                    Ok(socket) => socket,
                    Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                if let Err(e) = handle_tcp_connection(socket, config.clone()) {
                    eprintln!("tcp proxy failed: {e}");
                }
            }
        });
    }
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((socket, _)) => {
                    if let Err(e) = sender.send(socket) {
                        eprintln!("tcp queue failed: {e}");
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(LOOP_SLEEP),
                Err(e) => {
                    eprintln!("tcp accept failed: {e}");
                    thread::sleep(LOOP_SLEEP);
                }
            }
        }
    });
}

fn handle_tcp_connection(inbound: TcpStream, config: Arc<Mutex<SessionConfig>>) -> io::Result<()> {
    inbound.set_nonblocking(false)?;
    let destination = match inbound.local_addr()? {
        SocketAddr::V6(destination) => destination,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected ipv6 destination",
            ))
        }
    };
    let snapshot = config.lock().unwrap().clone();
    set_sockopt(
        inbound.as_raw_fd(),
        SOL_SOCKET,
        SO_MARK,
        &snapshot.reply_mark,
    )?;
    if destination.ip() == &snapshot.gateway && destination.port() == DNS_PORT {
        return handle_dns_tcp_connection(inbound, &snapshot);
    }
    let upstream = select_upstream(&snapshot, *destination.ip())
        .ok_or_else(|| io::Error::new(io::ErrorKind::AddrNotAvailable, "no upstream route"))?;
    let outbound = connect_upstream_tcp(&upstream, destination)?;
    relay_tcp(inbound, outbound)
}

fn relay_tcp(mut inbound: TcpStream, mut outbound: TcpStream) -> io::Result<()> {
    let mut upstream_reader = outbound.try_clone()?;
    let mut downstream_writer = inbound.try_clone()?;
    let reverse = thread::spawn(move || -> io::Result<()> {
        let result = io::copy(&mut upstream_reader, &mut downstream_writer);
        let _ = downstream_writer.shutdown(Shutdown::Write);
        result.map(|_| ())
    });
    let forward = io::copy(&mut inbound, &mut outbound);
    let _ = outbound.shutdown(Shutdown::Write);
    let reverse_result = reverse
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("tcp relay panicked")));
    forward?;
    reverse_result
}

fn spawn_udp_loop(listener: UdpSocket, config: Arc<Mutex<SessionConfig>>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let listener_fd = listener.as_raw_fd();
        let mut associations = HashMap::<AssociationKey, UdpAssociation>::new();
        let mut buffer = [0u8; 65535];
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            associations.retain(|_, association| {
                now.duration_since(association.last_active) < UDP_ASSOC_IDLE
            });

            let mut keys = Vec::with_capacity(associations.len());
            let mut poll_fds = Vec::with_capacity(associations.len() + 1);
            poll_fds.push(PollFd {
                fd: listener_fd,
                events: POLLIN,
                revents: 0,
            });
            for (key, association) in associations.iter() {
                keys.push(*key);
                poll_fds.push(PollFd {
                    fd: association.socket.as_raw_fd(),
                    events: POLLIN,
                    revents: 0,
                });
            }
            let timeout = LOOP_SLEEP.as_millis().min(c_int::MAX as u128) as c_int;
            let result = unsafe { poll(poll_fds.as_mut_ptr(), poll_fds.len(), timeout) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                eprintln!("udp poll failed: {error}");
                thread::sleep(LOOP_SLEEP);
                continue;
            }
            if poll_fds[0].revents & POLLIN != 0 {
                loop {
                    match recv_udp_packet(listener_fd, &mut buffer) {
                        Ok((size, client, destination)) => {
                            let snapshot = config.lock().unwrap().clone();
                            if destination.ip() == &snapshot.gateway
                                && destination.port() == DNS_PORT
                            {
                                if let Ok(response) = resolve_dns_query(&snapshot, &buffer[..size])
                                {
                                    if let Err(e) = send_udp_response(
                                        destination,
                                        client,
                                        snapshot.reply_mark,
                                        &response,
                                    ) {
                                        eprintln!("dns udp response failed: {e}");
                                    }
                                }
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
                            let key = AssociationKey {
                                client,
                                destination,
                            };
                            if !associations.contains_key(&key) {
                                while associations.len() >= UDP_ASSOC_MAX {
                                    let stale = associations
                                        .iter()
                                        .min_by_key(|(_, association)| association.last_active)
                                        .map(|(key, _)| *key);
                                    if let Some(stale) = stale {
                                        associations.remove(&stale);
                                    } else {
                                        break;
                                    }
                                }
                                let upstream = match connect_upstream_udp(&upstream, destination) {
                                    Ok(socket) => socket,
                                    Err(e) => {
                                        eprintln!("udp connect failed: {e}");
                                        continue;
                                    }
                                };
                                associations.insert(
                                    key,
                                    UdpAssociation {
                                        socket: upstream,
                                        last_active: now,
                                    },
                                );
                            }
                            let association = associations.get_mut(&key).unwrap();
                            association.last_active = now;
                            if let Err(e) = association.socket.send(&buffer[..size]) {
                                eprintln!("udp send failed: {e}");
                                associations.remove(&key);
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            eprintln!("udp recv failed: {e}");
                            break;
                        }
                    }
                }
            }
            for (index, key) in keys.into_iter().enumerate() {
                if poll_fds[index + 1].revents == 0 {
                    continue;
                }
                let mut remove = false;
                if let Some(association) = associations.get_mut(&key) {
                    loop {
                        match association.socket.recv(&mut buffer) {
                            Ok(size) => {
                                association.last_active = Instant::now();
                                let mark = config.lock().unwrap().reply_mark;
                                if let Err(e) = send_udp_response(
                                    key.destination,
                                    key.client,
                                    mark,
                                    &buffer[..size],
                                ) {
                                    eprintln!("udp response failed: {e}");
                                    remove = true;
                                    break;
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                eprintln!("udp upstream recv failed: {e}");
                                remove = true;
                                break;
                            }
                        }
                    }
                }
                if remove {
                    associations.remove(&key);
                }
            }
        }
    });
}

fn send_udp_response(
    source: SocketAddrV6,
    target: SocketAddrV6,
    mark: u32,
    payload: &[u8],
) -> io::Result<()> {
    let socket = create_udp_reply_socket(source, mark)?;
    socket.send_to(payload, SocketAddr::V6(target))?;
    Ok(())
}

fn spawn_dns_tcp_loop(
    listener: TcpListener,
    config: Arc<Mutex<SessionConfig>>,
    stop: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((socket, _)) => {
                    let config = config.clone();
                    thread::spawn(move || {
                        let snapshot = config.lock().unwrap().clone();
                        if let Err(e) = handle_dns_tcp_connection(socket, &snapshot) {
                            eprintln!("dns tcp failed: {e}");
                        }
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(LOOP_SLEEP),
                Err(e) => {
                    eprintln!("dns tcp accept failed: {e}");
                    thread::sleep(LOOP_SLEEP);
                }
            }
        }
    });
}

fn handle_dns_tcp_connection(mut socket: TcpStream, config: &SessionConfig) -> io::Result<()> {
    socket.set_nonblocking(false)?;
    loop {
        let mut header = [0u8; 2];
        match socket.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let length = u16::from_be_bytes(header) as usize;
        let mut query = vec![0u8; length];
        socket.read_exact(&mut query)?;
        let response = resolve_dns_query(config, &query)?;
        socket.write_all(&(response.len() as u16).to_be_bytes())?;
        socket.write_all(&response)?;
        socket.flush()?;
    }
}

fn spawn_dns_udp_loop(socket: UdpSocket, config: Arc<Mutex<SessionConfig>>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let mut buffer = [0u8; 65535];
        while !stop.load(Ordering::Relaxed) {
            match socket.recv_from(&mut buffer) {
                Ok((size, source)) => {
                    let snapshot = config.lock().unwrap().clone();
                    match resolve_dns_query(&snapshot, &buffer[..size]) {
                        Ok(response) => {
                            let _ = socket.send_to(&response, source);
                        }
                        Err(e) => eprintln!("dns udp resolve failed: {e}"),
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(LOOP_SLEEP),
                Err(e) => {
                    eprintln!("dns udp recv failed: {e}");
                    thread::sleep(LOOP_SLEEP);
                }
            }
        }
    });
}

fn spawn_ra_loop(config: Arc<Mutex<SessionConfig>>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let snapshot = config.lock().unwrap().clone();
        let socket = match create_ra_recv_socket(&snapshot.downstream, snapshot.reply_mark) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("ra socket failed: {e}");
                return;
            }
        };
        let mut last_sent: Option<Instant> = None;
        let mut last_deprecated_sent: Option<Instant> = None;
        let mut deprecated_prefixes = HashMap::<Route, Instant>::new();
        let mut buffer = [0u8; 1500];
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            let (current, send_current) = {
                let mut current = config.lock().unwrap();
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
                );
                last_deprecated_sent = Some(now);
            }
            if send_current || last_sent.map_or(true, |last| last.elapsed() >= RA_PERIOD) {
                let _ = send_ra(&current, None);
                last_sent = Some(now);
            }
            match recv_ra_request(socket, &mut buffer) {
                Ok(Some(source)) => {
                    let _ = send_ra(&current, Some(source));
                }
                Ok(None) => thread::sleep(LOOP_SLEEP),
                Err(e) => {
                    eprintln!("ra recv failed: {e}");
                    thread::sleep(LOOP_SLEEP);
                }
            }
        }
        unsafe {
            close(socket);
        }
    });
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
    Ok(fd)
}

fn recv_ra_request(fd: RawFd, buffer: &mut [u8]) -> io::Result<Option<SocketAddrV6>> {
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
            return Ok(None);
        }
        return Err(error);
    }
    if size == 0 || buffer[0] != 133 {
        return Ok(None);
    }
    Ok(Some(socket_addr_v6_from_raw(address)))
}

fn send_ra(config: &SessionConfig, target: Option<SocketAddrV6>) -> io::Result<()> {
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
    );
    unsafe {
        close(fd);
    }
    result
}

fn send_deprecated_ra(
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
}

fn withdraw_prefixes_once(config: &SessionConfig, prefixes: &[Route], keep_router: bool) {
    let fd = match create_ra_send_socket(&config.downstream, config.reply_mark, config.router) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("ra send socket failed: {e}");
            return;
        }
    };
    for prefix in prefixes.iter().cloned() {
        let _ = send_deprecated_ra(fd, config, prefix, keep_router);
    }
    unsafe {
        close(fd);
    }
}

fn withdraw_session_prefixes(session: &Session) {
    let snapshot = session.config.lock().unwrap().clone();
    let mut prefixes = snapshot.deprecated_prefixes.clone();
    prefixes.push(Route {
        prefix: ipv6_to_u128(snapshot.gateway),
        prefix_len: snapshot.prefix_len,
    });
    withdraw_prefixes_once(&snapshot, &prefixes, false);
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

fn resolve_dns_query(config: &SessionConfig, query: &[u8]) -> io::Result<Vec<u8>> {
    if let Some(primary) = config.primary.as_ref() {
        match query_network(primary.network_handle, query) {
            Ok(response) => return Ok(response),
            Err(e) => eprintln!("dns primary failed: {e}"),
        }
    }
    if let Some(fallback) = config.fallback.as_ref() {
        return query_network(fallback.network_handle, query);
    }
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "no DNS upstream",
    ))
}

fn query_network(network_handle: u64, query: &[u8]) -> io::Result<Vec<u8>> {
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
    let mut response = vec![0u8; 4096];
    let mut rcode = 0;
    let size =
        unsafe { android_res_nresult(fd, &mut rcode, response.as_mut_ptr(), response.len()) };
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

fn connect_upstream_tcp(upstream: &Upstream, destination: SocketAddrV6) -> io::Result<TcpStream> {
    let fd = create_socket(AF_INET6, SOCK_STREAM, 0)?;
    if unsafe { android_setsocknetwork(upstream.network_handle, fd) } != 0 {
        let error = io::Error::last_os_error();
        unsafe {
            close(fd);
        }
        return Err(error);
    }
    if let Err(error) = bind_upstream_socket(fd, &upstream.interface) {
        unsafe {
            close(fd);
        }
        return Err(error);
    }
    set_nonblocking(fd)?;
    let connect_result = unsafe {
        connect(
            fd,
            &raw_addr_v6(destination) as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    };
    if connect_result < 0 {
        let error = io::Error::last_os_error();
        let Some(raw_os_error) = error.raw_os_error() else {
            unsafe {
                close(fd);
            }
            return Err(error);
        };
        if error.kind() != io::ErrorKind::WouldBlock && raw_os_error != EINPROGRESS {
            unsafe {
                close(fd);
            }
            return Err(error);
        }
        wait_for_tcp_connect(fd, TCP_CONNECT_TIMEOUT)?;
    }
    if let Err(error) = clear_nonblocking(fd) {
        unsafe {
            close(fd);
        }
        return Err(error);
    }
    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

fn connect_upstream_udp(upstream: &Upstream, destination: SocketAddrV6) -> io::Result<UdpSocket> {
    let fd = create_socket(AF_INET6, SOCK_DGRAM, 0)?;
    if unsafe { android_setsocknetwork(upstream.network_handle, fd) } != 0 {
        let error = io::Error::last_os_error();
        unsafe {
            close(fd);
        }
        return Err(error);
    }
    if let Err(error) = bind_upstream_socket(fd, &upstream.interface) {
        unsafe {
            close(fd);
        }
        return Err(error);
    }
    syscall(unsafe {
        connect(
            fd,
            &raw_addr_v6(destination) as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    })?;
    set_nonblocking(fd)?;
    Ok(unsafe { UdpSocket::from_raw_fd(fd) })
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
    set_sockopt(fd, SOL_SOCKET, SO_MARK, &mark)?;
    set_sockopt(fd, IPPROTO_IPV6, IPV6_TRANSPARENT, &one)?;
    syscall(unsafe {
        bind(
            fd,
            &raw_addr_v6(source) as *const _ as *const SockAddr,
            size_of::<SockAddrIn6>() as u32,
        )
    })?;
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

fn connect_control_socket(socket_name: &str, connection_file: &PathBuf) -> io::Result<RawFd> {
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
        if unsafe { connect(fd, &address as *const _ as *const SockAddr, length) } == 0 {
            return Ok(fd);
        }
        let error = io::Error::last_os_error();
        unsafe {
            close(fd);
        }
        if start.elapsed() >= DAEMON_STARTUP_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("control socket connect timed out: {error}"),
            ));
        }
        thread::sleep(LOOP_SLEEP);
    }
}

fn recv_packet(fd: RawFd) -> io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    recv_exact(fd, &mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_CONTROL_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid control frame length {length}"),
        ));
    }
    let mut buffer = vec![0u8; length];
    recv_exact(fd, &mut buffer)?;
    Ok(buffer)
}

fn send_packet(fd: RawFd, packet: &[u8]) -> io::Result<()> {
    if packet.is_empty() || packet.len() > MAX_CONTROL_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid control frame length {}", packet.len()),
        ));
    }
    send_all(fd, &(packet.len() as u32).to_be_bytes())?;
    send_all(fd, packet)
}

fn recv_exact(fd: RawFd, mut buffer: &mut [u8]) -> io::Result<()> {
    while !buffer.is_empty() {
        let size = unsafe { recv(fd, buffer.as_mut_ptr() as *mut c_void, buffer.len(), 0) };
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "controller disconnected",
            ));
        }
        if size < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        buffer = &mut buffer[size as usize..];
    }
    Ok(())
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

fn clear_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    syscall(unsafe { fcntl(fd, F_SETFL, flags & !O_NONBLOCK) })
}

fn wait_for_tcp_connect(fd: RawFd, timeout: Duration) -> io::Result<()> {
    let mut poll_fd = PollFd {
        fd,
        events: POLLOUT,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(c_int::MAX as u128) as c_int;
    let result = unsafe { poll(&mut poll_fd, 1, timeout_ms) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if result == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "tcp connect timed out",
        ));
    }
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

fn syscall(result: c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn send_all(fd: RawFd, mut packet: &[u8]) -> io::Result<()> {
    while !packet.is_empty() {
        let written = unsafe {
            send(
                fd,
                packet.as_ptr() as *const c_void,
                packet.len(),
                MSG_NOSIGNAL,
            )
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "control socket write failed",
            ));
        }
        packet = &packet[written as usize..];
    }
    Ok(())
}

fn sendto_all(fd: RawFd, mut packet: &[u8], address: SockAddrIn6, length: u32) -> io::Result<()> {
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
            return Err(io::Error::last_os_error());
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
