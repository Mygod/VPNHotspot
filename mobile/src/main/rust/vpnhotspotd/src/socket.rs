use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::time::Duration;

use socket2::Socket;
use tokio::io::unix::AsyncFd;
use tokio::time::timeout;

pub(crate) async fn await_connect(socket: &Socket, duration: Duration) -> io::Result<()> {
    if timeout(duration, await_writable(socket.as_fd()))
        .await
        .is_err()
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "tcp connect timed out",
        ));
    }
    socket.take_error()?.map_or(Ok(()), Err)
}

pub(crate) async fn await_writable(fd: BorrowedFd<'_>) -> io::Result<()> {
    let fd = AsyncFd::new(fd)?;
    let _ = fd.writable().await?;
    Ok(())
}
