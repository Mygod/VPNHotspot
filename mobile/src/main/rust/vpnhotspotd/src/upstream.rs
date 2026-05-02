use std::io;
use std::mem::size_of_val;
use std::net::{Ipv6Addr, SocketAddrV6, TcpListener, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Duration;

use libc::{c_int, c_void, setsockopt, socklen_t, EINPROGRESS, IPPROTO_IPV6, IPV6_RECVORIGDSTADDR};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::{TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket};

use crate::model::Network;
use crate::socket::await_connect;

const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[link(name = "android")]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
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
    network: Network,
    destination: SocketAddrV6,
) -> io::Result<TokioTcpStream> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    set_socket_network(network, socket.as_raw_fd())?;
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
    network: Network,
    destination: SocketAddrV6,
) -> io::Result<TokioUdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.connect(&SockAddr::from(destination))?;
    socket.set_nonblocking(true)?;
    TokioUdpSocket::from_std(socket.into())
}

fn set_socket_network(network: Network, fd: c_int) -> io::Result<()> {
    if unsafe { android_setsocknetwork(network, fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
