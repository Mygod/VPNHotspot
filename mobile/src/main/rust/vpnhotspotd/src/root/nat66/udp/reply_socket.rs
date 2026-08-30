use std::collections::{hash_map::Entry, HashMap};
use std::io;
use std::net::{SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, OnceLock};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket as TokioUdpSocket;

use crate::report;
use crate::socket::is_udp_reply_unreachable;

struct ReplySocketEntry {
    source: SocketAddrV6,
    socket: OnceLock<TokioUdpSocket>,
}

struct ReplySocketEntryState {
    entry: Arc<ReplySocketEntry>,
    users: usize,
}

pub(super) struct ReplySocketLease {
    pool: Arc<ReplySocketPool>,
    entry: Option<Arc<ReplySocketEntry>>,
}

impl ReplySocketLease {
    pub(super) fn source(&self) -> SocketAddrV6 {
        self.entry().source
    }

    pub(super) fn try_clone(&self) -> io::Result<Self> {
        let entry = self.entry();
        let mut state = self.pool.lock_state()?;
        let current = state
            .sockets
            .get_mut(&entry.source)
            .filter(|current| Arc::ptr_eq(&current.entry, entry))
            .ok_or_else(|| io::Error::other("reply socket lease missing"))?;
        current.users = current
            .users
            .checked_add(1)
            .ok_or_else(|| io::Error::other("reply socket users overflow"))?;
        Ok(Self {
            pool: self.pool.clone(),
            entry: Some(entry.clone()),
        })
    }

    pub(super) fn prepare(&self) -> io::Result<()> {
        self.socket().map(|_| ())
    }

    fn entry(&self) -> &Arc<ReplySocketEntry> {
        self.entry
            .as_ref()
            .expect("reply socket lease entry missing")
    }

    fn socket(&self) -> io::Result<&TokioUdpSocket> {
        let entry = self.entry();
        if let Some(socket) = entry.socket.get() {
            return Ok(socket);
        }
        let state = self.pool.lock_state()?;
        state
            .sockets
            .get(&entry.source)
            .filter(|current| Arc::ptr_eq(&current.entry, entry))
            .ok_or_else(|| io::Error::other("reply socket lease missing"))?;
        if let Some(socket) = entry.socket.get() {
            return Ok(socket);
        }
        entry
            .socket
            .set(create_reply_socket(entry.source, self.pool.mark)?)
            .map_err(|_| io::Error::other("reply socket already initialized"))?;
        Ok(entry
            .socket
            .get()
            .expect("reply socket initialization failed"))
    }
}

impl Drop for ReplySocketLease {
    fn drop(&mut self) {
        let Some(entry) = self.entry.take() else {
            return;
        };
        let mut state = match self.pool.lock_state() {
            Ok(state) => state,
            Err(e) => {
                report::io("nat66.udp_reply_release", e);
                return;
            }
        };
        let mut release_error = None;
        let remove = match state.sockets.get_mut(&entry.source) {
            Some(current) if Arc::ptr_eq(&current.entry, &entry) => {
                if current.users == 0 {
                    release_error = Some("reply socket users underflow");
                    false
                } else {
                    current.users -= 1;
                    current.users == 0
                }
            }
            _ => {
                release_error = Some("reply socket lease missing");
                false
            }
        };
        if remove {
            let stored = state
                .sockets
                .remove(&entry.source)
                .expect("reply socket entry disappeared");
            // Close the socket before making the missing entry visible to another acquirer.
            drop(stored);
            drop(entry);
        }
        drop(state);
        if let Some(error) = release_error {
            report::message("nat66.udp_reply_release", error, "InvalidData");
        }
    }
}

#[derive(Default)]
struct ReplySocketState {
    sockets: HashMap<SocketAddrV6, ReplySocketEntryState>,
}

pub(crate) struct ReplySocketPool {
    mark: u32,
    state: StdMutex<ReplySocketState>,
}

impl ReplySocketPool {
    pub(crate) fn new(mark: u32) -> Self {
        Self {
            mark,
            state: StdMutex::default(),
        }
    }

    pub(super) fn lease(self: &Arc<Self>, source: SocketAddrV6) -> io::Result<ReplySocketLease> {
        let mut state = self.lock_state()?;
        let entry = match state.sockets.entry(source) {
            Entry::Occupied(mut current) => {
                let current = current.get_mut();
                current.users = current
                    .users
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("reply socket users overflow"))?;
                current.entry.clone()
            }
            Entry::Vacant(current) => {
                let entry = Arc::new(ReplySocketEntry {
                    source,
                    socket: OnceLock::new(),
                });
                current.insert(ReplySocketEntryState {
                    entry: entry.clone(),
                    users: 1,
                });
                entry
            }
        };
        Ok(ReplySocketLease {
            pool: self.clone(),
            entry: Some(entry),
        })
    }

    fn lock_state(&self) -> io::Result<MutexGuard<'_, ReplySocketState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("reply socket state poisoned"))
    }
}

pub(super) async fn send_response(
    reply_socket: &ReplySocketLease,
    target: SocketAddrV6,
    payload: &[u8],
) -> Result<(), SendResponseError> {
    let socket = reply_socket.socket().map_err(SendResponseError::Acquire)?;
    socket
        .send_to(payload, SocketAddr::V6(target))
        .await
        .map(|_| ())
        .map_err(SendResponseError::Send)
}

pub(super) enum SendResponseError {
    Acquire(io::Error),
    Send(io::Error),
}

impl SendResponseError {
    fn into_report_parts(self) -> (io::Error, &'static str) {
        match self {
            Self::Acquire(error) => (error, "acquire_socket"),
            Self::Send(error) => (error, "send"),
        }
    }
}

#[track_caller]
pub(super) fn report_send_response_error(
    context: &'static str,
    error: SendResponseError,
    client: SocketAddrV6,
    destination: SocketAddrV6,
) {
    if let SendResponseError::Send(error) = &error {
        if is_udp_reply_unreachable(error) {
            report::stderr!(
                "{context} dropped: client={client} destination={destination} send={error}"
            );
            return;
        }
    }
    let (error, stage) = error.into_report_parts();
    report::io_with_details(
        context,
        error,
        [
            ("client", client.to_string()),
            ("destination", destination.to_string()),
            ("reply_socket_stage", stage.to_owned()),
        ],
    );
}

fn create_reply_socket(source: SocketAddrV6, mark: u32) -> io::Result<TokioUdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_mark(mark)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(source))?;
    socket.set_nonblocking(true)?;
    TokioUdpSocket::from_std(socket.into())
}
