use std::collections::{hash_map::Entry, HashMap};
use std::io;
use std::net::{SocketAddr, SocketAddrV6};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::UdpSocket as TokioUdpSocket;

use crate::report;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct ReplySocketKey {
    pub(super) source: SocketAddrV6,
    pub(super) mark: u32,
}

struct ReplySocketEntry {
    socket: Option<Arc<TokioUdpSocket>>,
    users: usize,
}

struct RetainedDnsSocket {
    key: ReplySocketKey,
    socket: Arc<TokioUdpSocket>,
}

pub(super) struct ReplySocketReservation {
    pub(super) pool: Arc<ReplySocketPool>,
    pub(super) key: ReplySocketKey,
}

impl Drop for ReplySocketReservation {
    fn drop(&mut self) {
        if let Err(e) = self.pool.release_user(self.key) {
            report::io("nat66.udp_reply_release", e);
        }
    }
}

#[derive(Default)]
struct ReplySocketState {
    retained_dns: Option<RetainedDnsSocket>,
    sockets: HashMap<ReplySocketKey, ReplySocketEntry>,
}

#[derive(Default)]
pub(super) struct ReplySocketPool {
    state: StdMutex<ReplySocketState>,
}

impl ReplySocketPool {
    pub(super) fn reserve_user(
        self: &Arc<Self>,
        key: ReplySocketKey,
    ) -> io::Result<ReplySocketReservation> {
        self.with_state(|state| {
            let retained = state.retained_dns_socket(key);
            match state.sockets.entry(key) {
                Entry::Occupied(mut entry) => {
                    let entry = entry.get_mut();
                    entry.users += 1;
                    if entry.socket.is_none() {
                        entry.socket = retained;
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(ReplySocketEntry {
                        socket: retained,
                        users: 1,
                    });
                }
            }
        })?;
        Ok(ReplySocketReservation {
            pool: self.clone(),
            key,
        })
    }

    fn acquire_reserved(&self, key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.with_state(|state| state.reserved_socket(key))? {
            return Ok(socket);
        }
        let socket = create_reply_socket(key)?;
        self.with_state(|state| {
            if let Some(existing) = state.reserved_socket(key) {
                return existing;
            }
            if let Some(entry) = state.sockets.get_mut(&key) {
                entry.socket = Some(socket.clone());
            }
            socket
        })
    }

    pub(super) fn retain_dns(&self, key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.with_state(|state| state.retain_existing_dns_socket(key))? {
            return Ok(socket);
        }
        let socket = create_reply_socket(key)?;
        self.with_state(|state| {
            if let Some(existing) = state.retain_existing_dns_socket(key) {
                return existing;
            }
            if let Some(entry) = state.sockets.get_mut(&key) {
                entry.socket = Some(socket.clone());
            }
            state.retain_dns_socket(key, socket.clone());
            socket
        })
    }

    fn release_user(&self, key: ReplySocketKey) -> io::Result<()> {
        self.with_state(|state| -> io::Result<()> {
            let remove = {
                let entry = state
                    .sockets
                    .get_mut(&key)
                    .ok_or_else(|| io::Error::other("reply socket reservation missing"))?;
                if entry.users == 0 {
                    return Err(io::Error::other("reply socket users underflow"));
                }
                entry.users -= 1;
                entry.users == 0
            };
            if remove {
                state.sockets.remove(&key);
            }
            Ok(())
        })?
    }

    fn replace_socket(
        &self,
        key: ReplySocketKey,
        previous: &Arc<TokioUdpSocket>,
    ) -> io::Result<Arc<TokioUdpSocket>> {
        let socket = create_reply_socket(key)?;
        self.with_state(|state| state.replace_socket(key, previous, socket))
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut ReplySocketState) -> T) -> io::Result<T> {
        let mut state = self.lock_state()?;
        Ok(f(&mut state))
    }

    fn lock_state(&self) -> io::Result<MutexGuard<'_, ReplySocketState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("reply socket state poisoned"))
    }
}

impl ReplySocketState {
    fn retained_dns_socket(&self, key: ReplySocketKey) -> Option<Arc<TokioUdpSocket>> {
        self.retained_dns
            .as_ref()
            .filter(|retained| retained.key == key)
            .map(|retained| retained.socket.clone())
    }

    fn retain_dns_socket(&mut self, key: ReplySocketKey, socket: Arc<TokioUdpSocket>) {
        self.retained_dns = Some(RetainedDnsSocket { key, socket });
    }

    fn reserved_socket(&mut self, key: ReplySocketKey) -> Option<Arc<TokioUdpSocket>> {
        if let Some(socket) = self
            .sockets
            .get(&key)
            .and_then(|entry| entry.socket.clone())
        {
            return Some(socket);
        }
        let socket = self.retained_dns_socket(key)?;
        if let Some(entry) = self.sockets.get_mut(&key) {
            entry.socket = Some(socket.clone());
        }
        Some(socket)
    }

    fn retain_existing_dns_socket(&mut self, key: ReplySocketKey) -> Option<Arc<TokioUdpSocket>> {
        if let Some(socket) = self.retained_dns_socket(key) {
            return Some(socket);
        }
        let socket = self
            .sockets
            .get(&key)
            .and_then(|entry| entry.socket.clone())?;
        self.retain_dns_socket(key, socket.clone());
        Some(socket)
    }

    fn replace_socket(
        &mut self,
        key: ReplySocketKey,
        previous: &Arc<TokioUdpSocket>,
        socket: Arc<TokioUdpSocket>,
    ) -> Arc<TokioUdpSocket> {
        let mut current = None;
        let mut replaced = false;
        if let Some(entry) = self.sockets.get_mut(&key) {
            if entry
                .socket
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, previous))
            {
                entry.socket = Some(socket.clone());
                replaced = true;
            }
            current = entry.socket.clone();
        }
        if let Some(retained) = self
            .retained_dns
            .as_mut()
            .filter(|retained| retained.key == key)
        {
            if Arc::ptr_eq(&retained.socket, previous) {
                retained.socket = socket.clone();
                replaced = true;
            }
            if current.is_none() {
                current = Some(retained.socket.clone());
            }
        }
        if replaced {
            socket
        } else {
            current.unwrap_or(socket)
        }
    }
}

pub(super) async fn send_response(
    reply_sockets: &ReplySocketPool,
    key: ReplySocketKey,
    socket: &mut Option<Arc<TokioUdpSocket>>,
    target: SocketAddrV6,
    payload: &[u8],
) -> Result<(), SendResponseError> {
    let current = match socket {
        Some(socket) => socket.clone(),
        None => {
            let acquired = reply_sockets
                .acquire_reserved(key)
                .map_err(SendResponseError::Acquire)?;
            *socket = Some(acquired.clone());
            acquired
        }
    };
    if let Err(initial) = current.send_to(payload, SocketAddr::V6(target)).await {
        let retry = match reply_sockets.replace_socket(key, &current) {
            Ok(socket) => socket,
            Err(error) => return Err(SendResponseError::Replace { initial, error }),
        };
        *socket = Some(retry.clone());
        retry
            .send_to(payload, SocketAddr::V6(target))
            .await
            .map_err(|error| SendResponseError::Retry { initial, error })?;
    }
    Ok(())
}

pub(super) enum SendResponseError {
    Acquire(io::Error),
    Replace {
        initial: io::Error,
        error: io::Error,
    },
    Retry {
        initial: io::Error,
        error: io::Error,
    },
}

impl SendResponseError {
    fn into_report_parts(self) -> (io::Error, Option<io::Error>, &'static str) {
        match self {
            Self::Acquire(error) => (error, None, "acquire_socket"),
            Self::Replace { initial, error } => (error, Some(initial), "replace_socket"),
            Self::Retry { initial, error } => (error, Some(initial), "retry_send"),
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
    let (error, initial, stage) = error.into_report_parts();
    let mut details = vec![
        ("client", client.to_string()),
        ("destination", destination.to_string()),
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

fn create_reply_socket(key: ReplySocketKey) -> io::Result<Arc<TokioUdpSocket>> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_mark(key.mark)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(key.source))?;
    socket.set_nonblocking(true)?;
    Ok(Arc::new(TokioUdpSocket::from_std(socket.into())?))
}
