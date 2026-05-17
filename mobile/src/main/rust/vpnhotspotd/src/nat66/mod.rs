use std::future::pending;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio_util::sync::CancellationToken;

use cidr::Ipv6Inet;

use crate::netlink;
use crate::report;
use vpnhotspotd::shared::model::{Ipv6NatPorts, SessionConfig};
use vpnhotspotd::shared::protocol::daemon_io_error_report_with_details;

mod icmp;
mod ra;
mod tcp;
mod tproxy;
mod udp;

pub(crate) use icmp::Dispatcher as IcmpDispatcher;

const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

async fn sleep_until_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        sleep_until(TokioInstant::from_std(deadline)).await;
    } else {
        pending::<()>().await;
    }
}

pub(crate) struct Runtime {
    pub(crate) ports: Ipv6NatPorts,
    cleanup_prefixes: Vec<Ipv6Inet>,
    netlink: netlink::Handle,
    config_changed: Arc<Notify>,
    ra_task: Option<JoinHandle<()>>,
    _icmp_registration: Option<icmp::Registration>,
}

impl Runtime {
    pub(crate) async fn start(
        call_id: u64,
        config: &SessionConfig,
        shared: Arc<Mutex<SessionConfig>>,
        stop: CancellationToken,
        netlink: netlink::Handle,
        ipv6_address_changed: Arc<Notify>,
        icmp: &IcmpDispatcher,
    ) -> Option<Self> {
        config.ipv6_nat.as_ref()?;
        let config_changed = Arc::new(Notify::new());
        let tcp = match tproxy::create_tcp_listener(config.reply_mark).and_then(|listener| {
            let port = listener.local_addr()?.port();
            tcp::spawn_loop(listener, shared.clone(), stop.clone())?;
            Ok(port)
        }) {
            Ok(port) => Some(port),
            Err(e) => {
                report_start_error(call_id, "nat66.tcp_start", e, &config.downstream);
                None
            }
        };
        let udp = match tproxy::create_udp_listener(config.reply_mark).and_then(|listener| {
            let port = listener.local_addr()?.port();
            udp::spawn_loop(listener, shared.clone(), stop.clone(), icmp.clone())?;
            Ok(port)
        }) {
            Ok(port) => Some(port),
            Err(e) => {
                report_start_error(call_id, "nat66.udp_start", e, &config.downstream);
                None
            }
        };
        if tcp.is_none() && udp.is_none() {
            return None;
        }
        let icmp_registration = match icmp.register(config, shared.clone(), &netlink).await {
            Ok(registration) => Some(registration),
            Err(e) => {
                report::report_for(
                    Some(call_id),
                    daemon_io_error_report_with_details(
                        "nat66.icmp_start",
                        e,
                        [("downstream", config.downstream.clone())],
                    ),
                );
                None
            }
        };
        let ra_task = match ra::spawn_loop(
            shared.clone(),
            config_changed.clone(),
            ipv6_address_changed,
            netlink.clone(),
            stop.clone(),
            config,
        ) {
            Ok(task) => Some(task),
            Err(e) => {
                report_start_error(call_id, "nat66.ra_start", e, &config.downstream);
                None
            }
        };
        Some(Self {
            ports: Ipv6NatPorts {
                tcp,
                udp,
                icmp_echo: icmp_registration.is_some(),
            },
            cleanup_prefixes: Vec::new(),
            netlink,
            config_changed,
            ra_task,
            _icmp_registration: icmp_registration,
        })
    }

    pub(crate) fn record_config_replacement(
        &mut self,
        previous: &SessionConfig,
        next: &SessionConfig,
    ) {
        let Some(next) = next.ipv6_nat.as_ref() else {
            return;
        };
        if let Some(previous) = previous.ipv6_nat.as_ref() {
            if previous.gateway != next.gateway
                && !self.cleanup_prefixes.contains(&previous.gateway)
            {
                self.cleanup_prefixes.push(previous.gateway);
            }
        }
    }

    pub(crate) fn notify_config_changed(&self) {
        self.config_changed.notify_one();
    }

    pub(crate) async fn stop(self, snapshot: &SessionConfig, withdraw_cleanup: bool) {
        let Self {
            cleanup_prefixes,
            netlink,
            ra_task,
            _icmp_registration,
            ..
        } = self;
        drop(_icmp_registration);
        if let Some(ra_task) = ra_task {
            if let Err(e) = ra_task.await {
                report::message("nat66.ra_task_join", e.to_string(), "JoinError");
            }
        }
        let Some(ipv6_nat) = snapshot.ipv6_nat.as_ref() else {
            return;
        };
        let mut prefixes = if withdraw_cleanup {
            cleanup_prefixes
        } else {
            Vec::new()
        };
        prefixes.push(ipv6_nat.gateway);
        ra::withdraw_prefixes_once(&netlink, snapshot, &prefixes, false).await;
    }
}

fn report_start_error(call_id: u64, context: &str, error: io::Error, downstream: &str) {
    report::report_for(
        Some(call_id),
        daemon_io_error_report_with_details(
            context,
            error,
            [("downstream", downstream.to_owned())],
        ),
    );
}
