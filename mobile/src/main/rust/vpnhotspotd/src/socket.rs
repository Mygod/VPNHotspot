use std::io;
use std::mem::size_of;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::fd::{AsFd, BorrowedFd, RawFd};

use libc::{
    c_int, c_void, fcntl, setsockopt, sockaddr_in6, socklen_t, F_GETFL, F_SETFL, O_NONBLOCK,
};
use socket2::Socket;
use tokio::io::unix::AsyncFd;

pub(crate) async fn await_connect(socket: &Socket) -> io::Result<()> {
    await_writable(socket.as_fd()).await?;
    socket.take_error()?.map_or(Ok(()), Err)
}

pub(crate) async fn await_writable(fd: BorrowedFd<'_>) -> io::Result<()> {
    let fd = AsyncFd::new(fd)?;
    drop(fd.writable().await?);
    Ok(())
}

pub(crate) fn is_connection_closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
    )
}

pub(crate) fn set_nonblocking(fd: RawFd) -> io::Result<()> {
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

pub(crate) fn set_int_sockopt(
    fd: RawFd,
    level: c_int,
    name: c_int,
    value: c_int,
) -> io::Result<()> {
    let value_len = size_of::<c_int>() as socklen_t;
    if unsafe {
        setsockopt(
            fd,
            level,
            name,
            &value as *const _ as *const c_void,
            value_len,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

pub(crate) fn socket_addr_v6_from_raw(address: sockaddr_in6) -> SocketAddrV6 {
    SocketAddrV6::new(
        Ipv6Addr::from(address.sin6_addr.s6_addr),
        u16::from_be(address.sin6_port),
        address.sin6_flowinfo,
        address.sin6_scope_id,
    )
}
