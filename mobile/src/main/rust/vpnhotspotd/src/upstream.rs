use std::io;
use std::net::SocketAddrV6;
use std::os::fd::AsRawFd;

use libc::{c_int, EINPROGRESS};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::{TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket};

use crate::socket::await_connect;
use vpnhotspotd::shared::model::Network;

pub(crate) enum UpstreamConnectError {
    Setup(io::Error),
    Connect(io::Error),
}

#[link(name = "android")]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
}

pub(crate) async fn connect_tcp(
    network: Network,
    destination: SocketAddrV6,
) -> Result<TokioTcpStream, UpstreamConnectError> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))
        .map_err(UpstreamConnectError::Setup)?;
    set_socket_network(network, socket.as_raw_fd()).map_err(UpstreamConnectError::Setup)?;
    socket
        .set_nonblocking(true)
        .map_err(UpstreamConnectError::Setup)?;
    if let Err(error) = socket.connect(&SockAddr::from(destination)) {
        let raw_os_error = error.raw_os_error();
        if error.kind() != io::ErrorKind::WouldBlock && raw_os_error != Some(EINPROGRESS) {
            return Err(UpstreamConnectError::Connect(error));
        }
        await_connect(&socket)
            .await
            .map_err(UpstreamConnectError::Connect)?;
    }
    TokioTcpStream::from_std(socket.into()).map_err(UpstreamConnectError::Setup)
}

pub(crate) async fn connect_udp(
    network: Network,
    destination: SocketAddrV6,
) -> Result<TokioUdpSocket, UpstreamConnectError> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))
        .map_err(UpstreamConnectError::Setup)?;
    set_socket_network(network, socket.as_raw_fd()).map_err(UpstreamConnectError::Setup)?;
    socket
        .set_nonblocking(true)
        .map_err(UpstreamConnectError::Setup)?;
    socket
        .connect(&SockAddr::from(destination))
        .map_err(UpstreamConnectError::Connect)?;
    TokioUdpSocket::from_std(socket.into()).map_err(UpstreamConnectError::Setup)
}

pub(crate) fn set_socket_network(network: Network, fd: c_int) -> io::Result<()> {
    if unsafe { android_setsocknetwork(network, fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn is_selected_network_missing(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENONET)
}
