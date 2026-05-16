use std::io;
use std::net::{Ipv6Addr, SocketAddrV6, TcpListener, UdpSocket};

use nix::sys::socket::{setsockopt, sockopt};
use socket2::{Domain, SockAddr, Socket, Type};
use vpnhotspotd::shared::model::DAEMON_UDP_TPROXY_ADDRESS;

pub(crate) fn create_tcp_listener(mark: u32) -> io::Result<TcpListener> {
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

pub(crate) fn create_udp_listener(mark: u32) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_mark(mark)?;
    socket.set_only_v6(true)?;
    socket.set_ip_transparent_v6(true)?;
    setsockopt(&socket, sockopt::Ipv6OrigDstAddr, &true).map_err(io::Error::from)?;
    // UDP TPROXY rules land on ::1 so cached transparent reply sockets bound to
    // original destinations do not compete with the listener in UDP socket lookup.
    socket.bind(&SockAddr::from(SocketAddrV6::new(
        DAEMON_UDP_TPROXY_ADDRESS,
        0,
        0,
        0,
    )))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}
