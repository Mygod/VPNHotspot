use std::io;
use std::os::fd::{AsFd, BorrowedFd, RawFd};

use libc::{fcntl, F_GETFL, F_SETFL, O_NONBLOCK};
use socket2::{SockAddr, Socket};
use tokio::io::unix::AsyncFd;
use vpnhotspotd::shared::protocol::error_errno;

pub(crate) async fn await_connect(socket: &Socket) -> io::Result<()> {
    await_writable(socket.as_fd()).await?;
    socket.take_error()?.map_or(Ok(()), Err)
}

pub(crate) async fn await_writable(fd: BorrowedFd<'_>) -> io::Result<()> {
    let fd = AsyncFd::new(fd)?;
    drop(fd.writable().await?);
    Ok(())
}

pub(crate) async fn send_packet_to(
    socket: &Socket,
    packet: &[u8],
    address: SockAddr,
) -> io::Result<()> {
    loop {
        let written = match socket.send_to(packet, &address) {
            Ok(written) => written,
            Err(error) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    await_writable(socket.as_fd()).await?;
                    continue;
                }
                return Err(error);
            }
        };
        return packet_write_result(written, packet.len());
    }
}

pub(crate) async fn send_packet_to_async(
    socket: &AsyncFd<Socket>,
    packet: &[u8],
    address: SockAddr,
) -> io::Result<()> {
    loop {
        let mut ready = socket.writable().await?;
        let written = match socket.get_ref().send_to(packet, &address) {
            Ok(written) => written,
            Err(error) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    ready.clear_ready();
                    continue;
                }
                return Err(error);
            }
        };
        return packet_write_result(written, packet.len());
    }
}

fn packet_write_result(written: usize, expected: usize) -> io::Result<()> {
    if written == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short packet write",
        ))
    }
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

pub(crate) fn is_route_unreachable(error: &io::Error) -> bool {
    matches!(
        error_errno(error),
        Some(libc::EHOSTUNREACH | libc::ENETUNREACH)
    )
}

pub(crate) fn is_udp_reply_unreachable(error: &io::Error) -> bool {
    is_route_unreachable(error)
}

pub(crate) fn is_kernel_icmp_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::EACCES
                | libc::ECONNREFUSED
                | libc::EHOSTUNREACH
                | libc::EMSGSIZE
                | libc::ENETUNREACH
                | libc::EPROTO
                | libc::ETIMEDOUT
        )
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
