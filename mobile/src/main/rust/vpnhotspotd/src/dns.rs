use std::io;
use std::net::{TcpListener, UdpSocket};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use libc::{c_int, fcntl, F_GETFL, F_SETFL, O_NONBLOCK};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{
    TcpListener as TokioTcpListener, TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket,
};
use tokio::sync::Mutex;
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::model::SessionConfig;
pub(crate) const DNS_PORT: u16 = 53;
// android/multinetwork.h: ResNsendFlags::ANDROID_RESOLV_NO_RETRY.
const ANDROID_RESOLV_NO_RETRY: u32 = 1 << 0;

struct ResolverQuery {
    fd: Option<RawFd>,
}

impl ResolverQuery {
    fn finish(mut self, rcode: &mut c_int, answer: &mut [u8]) -> c_int {
        unsafe {
            android_res_nresult(
                self.fd.take().unwrap(),
                rcode,
                answer.as_mut_ptr(),
                answer.len(),
            )
        }
    }
}

impl AsRawFd for ResolverQuery {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.unwrap()
    }
}

impl Drop for ResolverQuery {
    fn drop(&mut self) {
        if let Some(fd) = self.fd.take() {
            unsafe {
                android_res_cancel(fd);
            }
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
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

#[link(name = "android")]
unsafe extern "C" {
    fn android_res_nsend(network: u64, msg: *const u8, msglen: usize, flags: u32) -> c_int;
    fn android_res_nresult(fd: c_int, rcode: *mut c_int, answer: *mut u8, anslen: usize) -> c_int;
    fn android_res_cancel(nsend_fd: c_int);
}

pub(crate) fn spawn_tcp_loop(
    listener: TcpListener,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = TokioTcpListener::from_std(listener)?;
    spawn(async move {
        loop {
            select! {
                _ = stop.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((socket, _)) => {
                        let config = config.clone();
                        spawn(async move {
                            let snapshot = config.lock().await.clone();
                            if let Err(e) = handle_tcp_connection(socket, snapshot).await {
                                eprintln!("dns tcp failed: {e}");
                            }
                        });
                    }
                    Err(e) => eprintln!("dns tcp accept failed: {e}"),
                }
            }
        }
    });
    Ok(())
}

pub(crate) async fn handle_tcp_connection(
    mut socket: TokioTcpStream,
    config: SessionConfig,
) -> io::Result<()> {
    loop {
        let mut header = [0u8; 2];
        match socket.read_exact(&mut header).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        }
        let length = u16::from_be_bytes(header) as usize;
        let mut query = vec![0u8; length];
        socket.read_exact(&mut query).await?;
        let response = resolve_query(&config, &query).await?;
        socket
            .write_all(&(response.len() as u16).to_be_bytes())
            .await?;
        socket.write_all(&response).await?;
        socket.flush().await?;
    }
}

pub(crate) fn spawn_udp_loop(
    socket: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let socket = Arc::new(TokioUdpSocket::from_std(socket)?);
    spawn(async move {
        let mut buffer = [0u8; 65535];
        loop {
            select! {
                _ = stop.cancelled() => break,
                received = socket.recv_from(&mut buffer) => match received {
                    Ok((size, source)) => {
                        let snapshot = config.lock().await.clone();
                        let query = buffer[..size].to_vec();
                        let socket = socket.clone();
                        let query_stop = stop.child_token();
                        spawn(async move {
                            select! {
                                _ = query_stop.cancelled() => {}
                                result = resolve_query(&snapshot, &query) => match result {
                                    Ok(response) => {
                                        let _ = socket.send_to(&response, source).await;
                                    }
                                    Err(e) => eprintln!("dns udp resolve failed: {e}"),
                                }
                            }
                        });
                    },
                    Err(e) => eprintln!("dns udp recv failed: {e}"),
                }
            };
        }
    });
    Ok(())
}

pub(crate) async fn resolve_query(config: &SessionConfig, query: &[u8]) -> io::Result<Vec<u8>> {
    if let Some(primary) = config.primary.as_ref() {
        match query_network(primary.network_handle, query).await {
            Ok(response) => return Ok(response),
            Err(e) => eprintln!("dns primary failed: {e}"),
        }
    }
    if let Some(fallback) = config.fallback.as_ref() {
        return query_network(fallback.network_handle, query).await;
    }
    Err(io::Error::new(
        io::ErrorKind::NotConnected,
        "no DNS upstream",
    ))
}

async fn query_network(network_handle: u64, query: &[u8]) -> io::Result<Vec<u8>> {
    let fd = unsafe {
        android_res_nsend(
            network_handle,
            query.as_ptr(),
            query.len(),
            ANDROID_RESOLV_NO_RETRY,
        )
    };
    if fd < 0 {
        return Err(io::Error::from_raw_os_error(-fd));
    }
    let fd = ResolverQuery { fd: Some(fd) };
    set_nonblocking(fd.as_raw_fd())?;
    let fd = AsyncFd::new(fd)?;
    let _ = fd.readable().await?;
    let mut response = vec![0u8; 4096];
    let mut rcode = 0;
    let size = fd.into_inner().finish(&mut rcode, &mut response);
    if size < 0 {
        return Err(io::Error::from_raw_os_error(-size));
    }
    response.truncate(size as usize);
    Ok(response)
}
