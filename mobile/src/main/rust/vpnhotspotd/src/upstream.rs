use std::io;
use std::net::SocketAddrV6;
use std::os::fd::AsRawFd;

use libc::{c_int, EINPROGRESS};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::{TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket};

use crate::socket::await_connect;
use vpnhotspotd::shared::model::Network;

#[link(name = "android")]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
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
        await_connect(&socket).await?;
    }
    TokioTcpStream::from_std(socket.into())
}

pub(crate) async fn connect_udp(
    network: Network,
    destination: SocketAddrV6,
) -> io::Result<TokioUdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.set_nonblocking(true)?;
    socket.connect(&SockAddr::from(destination))?;
    TokioUdpSocket::from_std(socket.into())
}

fn set_socket_network(network: Network, fd: c_int) -> io::Result<()> {
    if unsafe { android_setsocknetwork(network, fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
