use std::collections::HashMap;
use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Instant;

use socket2::Socket;
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::super::IDLE_TIMEOUT;
use super::probe::normalize_udp_error_addr;
use super::raw_socket::{create_upstream_socket, enable_ipv6_error_queue};
use super::UdpErrorContext;
use crate::report;
use vpnhotspotd::shared::icmp_nat::{EchoAllocation, EchoEntry, EchoMap};
use vpnhotspotd::shared::model::Network;

#[derive(Default)]
pub(super) struct EchoState {
    inner: StdMutex<EchoStateInner>,
}

#[derive(Default)]
struct EchoStateInner {
    map: EchoMap,
    upstream: HashMap<Network, UpstreamSocket>,
    udp_errors: HashMap<UdpErrorKey, UdpErrorContext>,
}

#[derive(Clone)]
pub(super) struct UpstreamSocket {
    pub(super) socket: Arc<AsyncFd<Socket>>,
    stop: CancellationToken,
    changed: Arc<Notify>,
    pub(super) error_queue: bool,
}

pub(super) enum UpstreamPrune {
    Removed,
    StillActive,
}

pub(super) enum UpstreamActivity {
    Idle,
    Active(Option<Instant>),
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct UdpErrorKey {
    network: Network,
    upstream: SocketAddrV6,
    destination: SocketAddrV6,
}

pub(crate) struct UdpErrorRegistration {
    key: UdpErrorKey,
    state: Arc<EchoState>,
}

impl Drop for UdpErrorRegistration {
    fn drop(&mut self) {
        if let Err(e) = self.state.unregister_udp_error(self.key) {
            report::io("nat66.icmp_udp_unregister", e);
        }
    }
}

impl EchoState {
    pub(super) fn allocate_echo(
        state: &Arc<Self>,
        allocation: EchoAllocation,
    ) -> io::Result<(u16, u16, UpstreamSocket)> {
        let network = allocation.network;
        let destination = allocation.destination;
        let (id, seq, socket, notify) = {
            let mut inner = state.lock_inner()?;
            let now = Instant::now();
            let old_deadline = inner.map.network_idle_deadline(now, network, IDLE_TIMEOUT);
            let (id, seq) = inner.map.allocate(now, IDLE_TIMEOUT, allocation)?;
            let socket = inner.upstream.get(&network).cloned();
            let notify = if old_deadline.is_none() {
                Self::upstream_changed_locked(&inner, network)
            } else {
                None
            };
            (id, seq, socket, notify)
        };
        Self::notify_upstream_changed(notify);
        if let Some(socket) = socket {
            return Ok((id, seq, socket));
        }
        match Self::ensure_upstream_socket(state, network) {
            Ok(socket) => Ok((id, seq, socket)),
            Err(e) => {
                if let Err(remove_error) = state.remove_allocation(network, destination, id, seq) {
                    report::io("nat66.icmp_echo_remove_failed_send", remove_error);
                }
                Err(e)
            }
        }
    }

    pub(super) fn restore(
        &self,
        network: Network,
        source: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> io::Result<Option<EchoEntry>> {
        let (entry, upstream) = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            let entry = inner
                .map
                .restore(now, IDLE_TIMEOUT, network, source, id, seq);
            let upstream = Self::remove_idle_upstream_locked(&mut inner, network, now);
            (entry, upstream)
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Ok(entry)
    }

    pub(super) fn remove_allocation(
        &self,
        network: Network,
        destination: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> io::Result<()> {
        let upstream = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            inner
                .map
                .remove(now, IDLE_TIMEOUT, network, destination, id, seq);
            Self::remove_idle_upstream_locked(&mut inner, network, now)
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Ok(())
    }

    pub(super) fn register_udp_error(
        state: &Arc<Self>,
        network: Network,
        upstream: SocketAddrV6,
        destination: SocketAddrV6,
        context: UdpErrorContext,
    ) -> io::Result<UdpErrorRegistration> {
        let key = UdpErrorKey {
            network,
            upstream: normalize_udp_error_addr(upstream),
            destination: normalize_udp_error_addr(destination),
        };
        state.lock_inner()?.udp_errors.insert(key, context);
        if let Err(e) = Self::ensure_upstream_socket(state, network) {
            state.lock_inner()?.udp_errors.remove(&key);
            return Err(e);
        }
        Ok(UdpErrorRegistration {
            key,
            state: state.clone(),
        })
    }

    fn unregister_udp_error(&self, key: UdpErrorKey) -> io::Result<()> {
        let (upstream, notify) = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            let removed = inner.udp_errors.remove(&key).is_some();
            let upstream = Self::remove_idle_upstream_locked(&mut inner, key.network, now);
            let notify = if removed
                && upstream.is_none()
                && !Self::has_udp_error_entries_locked(&inner, key.network)
                && inner
                    .map
                    .network_idle_deadline(now, key.network, IDLE_TIMEOUT)
                    .is_some()
            {
                Self::upstream_changed_locked(&inner, key.network)
            } else {
                None
            };
            (upstream, notify)
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Self::notify_upstream_changed(notify);
        Ok(())
    }

    pub(super) fn lookup_udp_error(
        &self,
        network: Network,
        upstream: SocketAddrV6,
        destination: SocketAddrV6,
    ) -> io::Result<Option<UdpErrorContext>> {
        Ok(self
            .lock_inner()?
            .udp_errors
            .get(&UdpErrorKey {
                network,
                upstream,
                destination,
            })
            .cloned())
    }

    pub(super) fn has_allocation(
        &self,
        network: Network,
        destination: Ipv6Addr,
        id: u16,
        seq: u16,
    ) -> io::Result<bool> {
        let mut inner = self.lock_inner()?;
        Ok(inner
            .map
            .contains(Instant::now(), IDLE_TIMEOUT, network, destination, id, seq))
    }

    pub(super) fn remove_session(&self, session_key: u64) -> io::Result<()> {
        let upstreams = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            inner.map.remove_session(now, IDLE_TIMEOUT, session_key);
            Self::remove_idle_upstreams_locked(&mut inner, now)
        };
        for upstream in upstreams {
            upstream.stop.cancel();
        }
        Ok(())
    }

    pub(super) fn upstream_activity(&self, network: Network) -> io::Result<UpstreamActivity> {
        let mut inner = self.lock_inner()?;
        let now = Instant::now();
        let echo_deadline = inner.map.network_idle_deadline(now, network, IDLE_TIMEOUT);
        if echo_deadline.is_some() || Self::has_udp_error_entries_locked(&inner, network) {
            Ok(UpstreamActivity::Active(echo_deadline))
        } else {
            Ok(UpstreamActivity::Idle)
        }
    }

    pub(super) fn prune_idle_upstream(&self, network: Network) -> io::Result<UpstreamPrune> {
        let (result, upstream) = {
            let mut inner = self.lock_inner()?;
            let now = Instant::now();
            if Self::has_network_entries_locked(&mut inner, network, now) {
                (UpstreamPrune::StillActive, None)
            } else {
                (UpstreamPrune::Removed, inner.upstream.remove(&network))
            }
        };
        if let Some(upstream) = upstream {
            upstream.stop.cancel();
        }
        Ok(result)
    }

    fn ensure_upstream_socket(state: &Arc<Self>, network: Network) -> io::Result<UpstreamSocket> {
        if let Some(upstream) = state.lock_inner()?.upstream.get(&network).cloned() {
            return Ok(upstream);
        }
        let socket = Arc::new(AsyncFd::new(create_upstream_socket(network)?)?);
        let stop = CancellationToken::new();
        let changed = Arc::new(Notify::new());
        let error_queue = match enable_ipv6_error_queue(socket.get_ref()) {
            Ok(()) => true,
            Err(e) => {
                report::io_with_details(
                    "nat66.icmp_error_queue",
                    e,
                    [("network", network.to_string())],
                );
                false
            }
        };
        let upstream = UpstreamSocket {
            socket: socket.clone(),
            stop: stop.clone(),
            changed: changed.clone(),
            error_queue,
        };
        let mut start = false;
        let active = {
            let mut inner = state.lock_inner()?;
            if let Some(upstream) = inner.upstream.get(&network) {
                upstream.clone()
            } else {
                if !Self::has_network_entries_locked(&mut inner, network, Instant::now()) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "upstream ICMP registration removed",
                    ));
                }
                start = true;
                inner.upstream.insert(network, upstream.clone());
                upstream
            }
        };
        if start {
            super::upstream::spawn_loop(
                network,
                active.socket.clone(),
                active.changed.clone(),
                active.error_queue,
                state.clone(),
                stop,
            );
        }
        Ok(active)
    }

    fn remove_idle_upstream_locked(
        inner: &mut EchoStateInner,
        network: Network,
        now: Instant,
    ) -> Option<UpstreamSocket> {
        if Self::has_network_entries_locked(inner, network, now) {
            None
        } else {
            inner.upstream.remove(&network)
        }
    }

    fn has_network_entries_locked(
        inner: &mut EchoStateInner,
        network: Network,
        now: Instant,
    ) -> bool {
        inner.map.has_network_entries(now, IDLE_TIMEOUT, network)
            || Self::has_udp_error_entries_locked(inner, network)
    }

    fn has_udp_error_entries_locked(inner: &EchoStateInner, network: Network) -> bool {
        inner.udp_errors.keys().any(|key| key.network == network)
    }

    fn remove_idle_upstreams_locked(
        inner: &mut EchoStateInner,
        now: Instant,
    ) -> Vec<UpstreamSocket> {
        let networks: Vec<_> = inner.upstream.keys().copied().collect();
        networks
            .into_iter()
            .filter_map(|network| Self::remove_idle_upstream_locked(inner, network, now))
            .collect()
    }

    pub(super) fn cancel_all_upstream(&self) -> io::Result<()> {
        for upstream in self
            .lock_inner()?
            .upstream
            .drain()
            .map(|(_, upstream)| upstream)
        {
            upstream.stop.cancel();
        }
        Ok(())
    }

    fn upstream_changed_locked(inner: &EchoStateInner, network: Network) -> Option<Arc<Notify>> {
        inner
            .upstream
            .get(&network)
            .map(|upstream| upstream.changed.clone())
    }

    fn notify_upstream_changed(notify: Option<Arc<Notify>>) {
        if let Some(notify) = notify {
            notify.notify_one();
        }
    }

    pub(super) fn remove_upstream_socket(
        &self,
        network: Network,
        socket: &Arc<AsyncFd<Socket>>,
    ) -> io::Result<()> {
        let mut inner = self.lock_inner()?;
        if inner
            .upstream
            .get(&network)
            .is_some_and(|upstream| Arc::ptr_eq(&upstream.socket, socket))
        {
            inner.upstream.remove(&network);
        }
        Ok(())
    }

    fn lock_inner(&self) -> io::Result<MutexGuard<'_, EchoStateInner>> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("echo state poisoned"))
    }
}
