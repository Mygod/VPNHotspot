use std::collections::HashMap;
use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use tokio::io::unix::AsyncFd;
use tokio::spawn;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::{netlink, report};
use vpnhotspotd::shared::model::{Network, SessionConfig};

mod downstream;
mod probe;
mod raw_socket;
mod state;
mod upstream;

use raw_socket::{create_downstream_queue, probe_downstream_send_socket};
use state::EchoState;

pub(crate) use downstream::{send_udp_packet_too_big, send_udp_time_exceeded};
pub(crate) use state::UdpErrorRegistration;

#[derive(Clone)]
pub(crate) struct Dispatcher {
    inner: Arc<DispatcherInner>,
}

struct DispatcherInner {
    state: Arc<EchoState>,
    registrations: Arc<StdMutex<HashMap<u32, Weak<IcmpSession>>>>,
    next_session_key: AtomicU64,
    task: StdMutex<Option<JoinHandle<()>>>,
}

impl Drop for DispatcherInner {
    fn drop(&mut self) {
        if let Err(e) = self.state.cancel_all_upstream() {
            report::io("nat66.icmp_upstream_cancel", e);
        }
        let Ok(mut task) = self.task.lock() else {
            return;
        };
        if let Some(task) = task.take() {
            task.abort();
        }
    }
}

pub(crate) struct Registration {
    input_interface: u32,
    session: Arc<IcmpSession>,
    registrations: Arc<StdMutex<HashMap<u32, Weak<IcmpSession>>>>,
}

struct IcmpSession {
    session_key: u64,
    state: Arc<EchoState>,
    config: Arc<Mutex<SessionConfig>>,
}

#[derive(Clone)]
pub(crate) struct UdpErrorContext {
    pub(crate) downstream: String,
    pub(crate) reply_mark: u32,
    pub(crate) gateway: Ipv6Addr,
    pub(crate) client: SocketAddrV6,
    pub(crate) destination: SocketAddrV6,
}

impl Dispatcher {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(DispatcherInner {
                state: Arc::new(EchoState::default()),
                registrations: Arc::new(StdMutex::new(HashMap::new())),
                next_session_key: AtomicU64::new(1),
                task: StdMutex::new(None),
            }),
        }
    }

    pub(crate) async fn register(
        &self,
        initial: &SessionConfig,
        config: Arc<Mutex<SessionConfig>>,
        netlink: &netlink::Handle,
    ) -> io::Result<Registration> {
        if initial.ipv6_nat.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing ipv6 NAT config",
            ));
        }
        let input_interface = netlink::link_index(netlink, &initial.downstream).await?;
        probe_downstream_send_socket(&initial.downstream, initial.reply_mark)?;
        self.ensure_started()?;
        let session_key = self.inner.next_session_key.fetch_add(1, Ordering::Relaxed);
        let session = Arc::new(IcmpSession {
            session_key,
            state: self.inner.state.clone(),
            config,
        });
        self.inner
            .registrations
            .lock()
            .map_err(|_| io::Error::other("icmp registrations state poisoned"))?
            .insert(input_interface, Arc::downgrade(&session));
        Ok(Registration {
            input_interface,
            session,
            registrations: self.inner.registrations.clone(),
        })
    }

    pub(crate) fn register_udp_error(
        &self,
        network: Network,
        upstream: SocketAddrV6,
        destination: SocketAddrV6,
        context: UdpErrorContext,
    ) -> io::Result<UdpErrorRegistration> {
        EchoState::register_udp_error(&self.inner.state, network, upstream, destination, context)
    }

    fn ensure_started(&self) -> io::Result<()> {
        let mut task = self
            .inner
            .task
            .lock()
            .map_err(|_| io::Error::other("icmp queue task state poisoned"))?;
        if task.as_ref().is_some_and(|task| !task.is_finished()) {
            return Ok(());
        }
        let queue = AsyncFd::new(create_downstream_queue()?)?;
        *task = Some(spawn(downstream::run_queue(
            queue,
            self.inner.registrations.clone(),
            self.inner.state.clone(),
        )));
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        match self.registrations.lock() {
            Ok(mut registrations) => {
                let remove_registration = match registrations.get(&self.input_interface) {
                    Some(session) => match session.upgrade() {
                        Some(session) => Arc::ptr_eq(&session, &self.session),
                        None => true,
                    },
                    None => false,
                };
                if remove_registration {
                    registrations.remove(&self.input_interface);
                }
            }
            Err(_) => report::message(
                "nat66.icmp_remove_registration",
                "icmp registrations state poisoned",
                "PoisonError",
            ),
        }
    }
}

impl Drop for IcmpSession {
    fn drop(&mut self) {
        if let Err(e) = self.state.remove_session(self.session_key) {
            report::io("nat66.icmp_remove_session", e);
        }
    }
}
