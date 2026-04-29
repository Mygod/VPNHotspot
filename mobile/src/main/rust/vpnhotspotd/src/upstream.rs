use std::io;
use std::mem::size_of_val;
use std::net::{Ipv6Addr, SocketAddrV6, TcpListener, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Duration;

use libc::{c_int, c_void, setsockopt, socklen_t, EINPROGRESS, IPPROTO_IPV6, IPV6_RECVORIGDSTADDR};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::{TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket};

use crate::model::{ipv6_to_u128, Route, SessionConfig, Upstream};
use crate::socket::await_connect;

const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg_attr(target_os = "android", link(name = "android"))]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
}

pub(crate) fn select_upstream(config: &SessionConfig, destination: Ipv6Addr) -> Option<&Upstream> {
    let destination = ipv6_to_u128(destination);
    let mut best = None;
    if let Some(primary) = config.primary.as_ref() {
        if let Some(prefix_len) = longest_prefix_match(&primary.routes, destination) {
            best = Some((prefix_len, primary));
        }
    }
    if let Some(fallback) = config.fallback.as_ref() {
        if let Some(prefix_len) = longest_prefix_match(&fallback.routes, destination) {
            if best
                .as_ref()
                .is_none_or(|(current, _)| prefix_len > *current)
            {
                best = Some((prefix_len, fallback));
            }
        }
    }
    best.map(|(_, upstream)| upstream)
}

pub(crate) fn create_tproxy_tcp_listener(mark: u32) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_mark(mark)?;
    socket.set_only_v6(true)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    )))?;
    socket.listen(32)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

pub(crate) fn create_tproxy_udp_listener(mark: u32) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_mark(mark)?;
    socket.set_only_v6(true)?;
    socket.set_ip_transparent_v6(true)?;
    let one = 1;
    if unsafe {
        setsockopt(
            socket.as_raw_fd(),
            IPPROTO_IPV6,
            IPV6_RECVORIGDSTADDR,
            &one as *const _ as *const c_void,
            size_of_val(&one) as socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    socket.bind(&SockAddr::from(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    )))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

pub(crate) async fn connect_tcp(
    upstream: &Upstream,
    destination: SocketAddrV6,
) -> io::Result<TokioTcpStream> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    if unsafe { android_setsocknetwork(upstream.network_handle, socket.as_raw_fd()) } != 0 {
        let error = io::Error::last_os_error();
        return Err(error);
    }
    bind_upstream_socket(&socket, &upstream.interface)?;
    socket.set_nonblocking(true)?;
    if let Err(error) = socket.connect(&SockAddr::from(destination)) {
        let raw_os_error = error.raw_os_error();
        if error.kind() != io::ErrorKind::WouldBlock && raw_os_error != Some(EINPROGRESS) {
            return Err(error);
        }
        await_connect(&socket, TCP_CONNECT_TIMEOUT).await?;
    }
    TokioTcpStream::from_std(socket.into())
}

pub(crate) async fn connect_udp(
    upstream: &Upstream,
    destination: SocketAddrV6,
) -> io::Result<TokioUdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    if unsafe { android_setsocknetwork(upstream.network_handle, socket.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    bind_upstream_socket(&socket, &upstream.interface)?;
    socket.connect(&SockAddr::from(destination))?;
    socket.set_nonblocking(true)?;
    TokioUdpSocket::from_std(socket.into())
}

fn bind_upstream_socket(socket: &Socket, interface: &str) -> io::Result<()> {
    if interface.is_empty() {
        return Ok(());
    }
    socket.bind_device(Some(interface.as_bytes()))
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
