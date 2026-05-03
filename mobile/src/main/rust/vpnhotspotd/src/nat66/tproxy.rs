use std::io;
use std::mem::size_of_val;
use std::net::{Ipv6Addr, SocketAddrV6, TcpListener, UdpSocket};
use std::os::fd::AsRawFd;

use libc::{c_void, setsockopt, socklen_t, IPPROTO_IPV6, IPV6_RECVORIGDSTADDR};
use socket2::{Domain, SockAddr, Socket, Type};

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
