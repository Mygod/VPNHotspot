use std::io;
use std::net::{Ipv4Addr, TcpListener, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use libc::{c_int, fcntl, F_GETFL, F_SETFL, O_NONBLOCK};
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
use crate::socket::is_connection_closed;
use vpnhotspotd::shared::dns_wire;
use vpnhotspotd::shared::model::{Network, SessionConfig};

pub(crate) const DNS_PORT: u16 = 53;
// android/multinetwork.h: ResNsendFlags::ANDROID_RESOLV_NO_RETRY.
const ANDROID_RESOLV_NO_RETRY: u32 = 1 << 0;
// Maximum DNS message size carried over TCP or EDNS0 UDP.
const DNS_MAX_PACKET: usize = 65_535;

pub(crate) struct Runtime {
    pub(crate) tcp_port: u16,
    pub(crate) udp_port: u16,
}

impl Runtime {
    pub(crate) fn start(
        bind_address: Ipv4Addr,
        reply_mark: u32,
        config: Arc<Mutex<SessionConfig>>,
        stop: CancellationToken,
    ) -> io::Result<Self> {
        let tcp_listener = create_tcp_listener(bind_address, reply_mark)?;
        let tcp_port = tcp_listener.local_addr()?.port();
        let udp_socket = create_udp_listener(bind_address, reply_mark)?;
        let udp_port = udp_socket.local_addr()?.port();
        spawn_tcp_loop(tcp_listener, config.clone(), stop.clone())?;
        if let Err(e) = spawn_udp_loop(udp_socket, config, stop.clone()) {
            stop.cancel();
            return Err(e);
        }
        Ok(Self { tcp_port, udp_port })
    }
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

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
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
) -> io::Result<()> {
    let listener = TokioTcpListener::from_std(listener)?;
    spawn(async move {
        loop {
            select! {
                _ = stop.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((socket, _)) => {
                        let config = config.clone();
                        let connection_stop = stop.child_token();
                        spawn(async move {
                            select! {
                                _ = connection_stop.cancelled() => {}
                                result = async {
                                    let snapshot = config.lock().await.clone();
                                    handle_tcp_connection(socket, snapshot).await
                                } => if let Err(e) = result {
                                    if is_connection_closed(&e) {
                                        eprintln!("dns tcp connection closed: {e}");
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

pub(crate) async fn handle_tcp_connection(
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
        let Some(response) = resolve_or_error(&config, &query).await else {
            continue;
        };
        socket
            .write_all(&(response.len() as u16).to_be_bytes())
            .await?;
        socket.write_all(&response).await?;
        socket.flush().await?;
    }
}

fn spawn_udp_loop(
    socket: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
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
                        spawn(async move {
                            select! {
                                _ = query_stop.cancelled() => {}
                                response = resolve_or_error(&snapshot, &query) => {
                                    if let Some(response) = response {
                                        if let Err(e) = socket.send_to(&response, source).await {
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

pub(crate) async fn resolve_query(config: &SessionConfig, query: &[u8]) -> io::Result<Vec<u8>> {
    if let Some(primary) = config.primary_network {
        return query_network(primary, query).await;
    }
    if let Some(fallback) = config.fallback_network {
        return query_network(fallback, query).await;
    }
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "no DNS upstream",
    ))
}

pub(crate) async fn resolve_or_error(config: &SessionConfig, query: &[u8]) -> Option<Vec<u8>> {
    match resolve_query(config, query).await {
        Ok(response) => Some(response),
        Err(e) => {
            eprintln!("dns resolve failed: {e}");
            dns_wire::servfail_response(query)
        }
    }
}

async fn query_network(network: Network, query: &[u8]) -> io::Result<Vec<u8>> {
    let fd = unsafe {
        android_res_nsend(
            network,
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
    read_resolver_result(AsyncFd::new(fd)?).await
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
