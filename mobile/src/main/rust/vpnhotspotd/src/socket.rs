use std::io;
use std::os::fd::{AsFd, BorrowedFd};

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
