use std::collections::{hash_map::Entry, HashMap};
use std::io;
use std::net::{SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, OnceLock, Weak};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket as TokioUdpSocket;

use super::raw_reply::RawReplySocket;
use crate::report;
use crate::socket::is_udp_reply_unreachable;
use vpnhotspotd::shared::model::mac_string;

struct ReplySocketEntry {
    source: SocketAddrV6,
    socket: OnceLock<ReplySocket>,
}

enum ReplySocket {
    Bound(TokioUdpSocket),
    /// The exact bind found the source port in use, so replies from this source use the shared raw
    /// socket.
    Raw(Arc<RawReplySocket>),
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

    pub(super) fn prepare(&self) -> Result<(), SendResponseError> {
        self.socket().map(|_| ())
    }

    fn entry(&self) -> &Arc<ReplySocketEntry> {
        self.entry
            .as_ref()
            .expect("reply socket lease entry missing")
    }

    fn socket(&self) -> Result<&ReplySocket, SendResponseError> {
        let entry = self.entry();
        if let Some(socket) = entry.socket.get() {
            return Ok(socket);
        }
        let mut state = self.pool.lock_state()?;
        state
            .sockets
            .get(&entry.source)
            .filter(|current| Arc::ptr_eq(&current.entry, entry))
            .ok_or_else(|| io::Error::other("reply socket lease missing"))?;
        if let Some(socket) = entry.socket.get() {
            return Ok(socket);
        }
        let in_use = match create_reply_socket(entry.source, self.pool.mark) {
            Ok(socket) => {
                entry
                    .socket
                    .set(ReplySocket::Bound(socket))
                    .map_err(|_| io::Error::other("reply socket already initialized"))?;
                return Ok(entry
                    .socket
                    .get()
                    .expect("reply socket initialization failed"));
            }
            // Another socket owns this port on the source address or the wildcard, and SO_REUSEADDR
            // shares a port only when both sockets set it
            // (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv4/udp.c#142).
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => e,
            Err(e) => return Err(e.into()),
        };
        let raw = match state.raw.upgrade() {
            Some(raw) => raw,
            None => {
                // Creation connects through Bionic's blocking fwmarkd request, so it runs unlocked.
                drop(state);
                let created = Arc::new(
                    RawReplySocket::new(self.pool.mark).map_err(SendResponseError::RawSocket)?,
                );
                state = self.pool.lock_state()?;
                // A concurrent acquirer may have initialized this entry or the shared socket meanwhile;
                // an unused new socket closes when dropped.
                if let Some(socket) = entry.socket.get() {
                    return Ok(socket);
                }
                match state.raw.upgrade() {
                    Some(raw) => raw,
                    None => {
                        state.raw = Arc::downgrade(&created);
                        created
                    }
                }
            }
        };
        entry
            .socket
            .set(ReplySocket::Raw(raw))
            .map_err(|_| io::Error::other("reply socket already initialized"))?;
        drop(state);
        report::stdout!(
            "nat66 udp reply source {} in use, replying through raw socket: {in_use}",
            entry.source
        );
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
    /// Shared by every entry that fell back to raw replies; it closes with the last such entry.
    raw: Weak<RawReplySocket>,
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
    match reply_socket.socket()? {
        ReplySocket::Bound(socket) => {
            if let Err(initial) = socket.send_to(payload, SocketAddr::V6(target)).await {
                socket
                    .send_to(payload, SocketAddr::V6(target))
                    .await
                    .map_err(|error| SendResponseError::Retry {
                        stage: "retry_send",
                        initial,
                        error,
                    })?;
            }
        }
        ReplySocket::Raw(socket) => {
            let source = reply_socket.source();
            if let Err(initial) = socket.send(source, target, payload) {
                socket
                    .send(source, target, payload)
                    .map_err(|error| SendResponseError::Retry {
                        stage: "raw_retry_send",
                        initial,
                        error,
                    })?;
            }
        }
    }
    Ok(())
}

pub(super) enum SendResponseError {
    Acquire(io::Error),
    RawSocket(io::Error),
    Retry {
        stage: &'static str,
        initial: io::Error,
        error: io::Error,
    },
}

impl From<io::Error> for SendResponseError {
    fn from(error: io::Error) -> Self {
        Self::Acquire(error)
    }
}

impl SendResponseError {
    fn into_report_parts(self) -> (io::Error, Option<io::Error>, &'static str) {
        match self {
            Self::Acquire(error) => (error, None, "acquire_socket"),
            Self::RawSocket(error) => (error, None, "raw_socket"),
            Self::Retry {
                stage,
                initial,
                error,
            } => (error, Some(initial), stage),
        }
    }
}

#[track_caller]
pub(super) fn report_send_response_error(
    context: &'static str,
    error: SendResponseError,
    mac: [u8; 6],
    client: SocketAddrV6,
    destination: SocketAddrV6,
    downstream: &str,
) {
    if let SendResponseError::Retry { initial, error, .. } = &error {
        if is_udp_reply_unreachable(initial) && is_udp_reply_unreachable(error) {
            report::stderr!(
                "{context} dropped: client={client} destination={destination} \
                 initial_send={initial} retry_send={error}"
            );
            return;
        }
    }
    let (error, initial, stage) = error.into_report_parts();
    let mut details = vec![
        ("mac", mac_string(&mac)),
        ("client", client.to_string()),
        ("destination", destination.to_string()),
        ("downstream", downstream.to_owned()),
        ("reply_socket_stage", stage.to_owned()),
    ];
    if let Some(initial) = initial {
        let initial_errno = initial.raw_os_error();
        details.extend([
            ("initial_send_kind", format!("{:?}", initial.kind())),
            ("initial_send_error", initial.to_string()),
        ]);
        if let Some(errno) = initial_errno {
            details.push(("initial_send_errno", errno.to_string()));
        }
    }
    report::io_with_details(context, error, details);
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
