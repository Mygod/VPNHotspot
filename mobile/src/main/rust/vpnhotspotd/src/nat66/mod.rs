use std::io;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use cidr::Ipv6Inet;

use crate::netlink;
use crate::report;
use vpnhotspotd::shared::model::{Ipv6NatPorts, SessionConfig};

mod icmp;
mod ra;
mod tcp;
mod tproxy;
mod udp;

pub(crate) struct Runtime {
    pub(crate) ports: Ipv6NatPorts,
    cleanup_prefixes: Vec<Ipv6Inet>,
    netlink: netlink::Handle,
    config_changed: Arc<Notify>,
    ra_task: JoinHandle<()>,
    icmp_task: Option<JoinHandle<()>>,
}

impl Runtime {
    pub(crate) fn start(
        config: &SessionConfig,
        shared: Arc<Mutex<SessionConfig>>,
        stop: CancellationToken,
        netlink: netlink::Handle,
        ipv6_address_changed: Arc<Notify>,
    ) -> io::Result<Option<Self>> {
        if config.ipv6_nat.is_none() {
            return Ok(None);
        }
        let config_changed = Arc::new(Notify::new());
        let tcp_listener = tproxy::create_tcp_listener(config.reply_mark)?;
        let tcp = tcp_listener.local_addr()?.port();
        let udp_listener = tproxy::create_udp_listener(config.reply_mark)?;
        let udp = udp_listener.local_addr()?.port();
        tcp::spawn_loop(tcp_listener, shared.clone(), stop.clone())?;
        if let Err(e) = udp::spawn_loop(udp_listener, shared.clone(), stop.clone()) {
            stop.cancel();
            return Err(e);
        }
        let ra_task = match ra::spawn_loop(
            shared.clone(),
            config_changed.clone(),
            ipv6_address_changed,
            netlink.clone(),
            stop.clone(),
            config,
        ) {
            Ok(task) => task,
            Err(e) => {
                stop.cancel();
                return Err(e);
            }
        };
        let icmp_task = match icmp::spawn_loop(config, shared.clone(), stop.clone()) {
            Ok(task) => Some(task),
            Err(e) => {
                report::io_with_details(
                    "nat66.icmp_start",
                    e,
                    [("downstream", config.downstream.clone())],
                );
                None
            }
        };
        Ok(Some(Self {
            ports: Ipv6NatPorts {
                tcp,
                udp,
                icmp_echo: icmp_task.is_some(),
            },
            cleanup_prefixes: Vec::new(),
            netlink,
            config_changed,
            ra_task,
            icmp_task,
        }))
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
        if let Err(e) = self.ra_task.await {
            report::message("nat66.ra_task_join", e.to_string(), "JoinError");
        }
        if let Some(icmp_task) = self.icmp_task {
            if let Err(e) = icmp_task.await {
                report::message("nat66.icmp_task_join", e.to_string(), "JoinError");
            }
        }
        let Some(ipv6_nat) = snapshot.ipv6_nat.as_ref() else {
            return;
        };
        let mut prefixes = if withdraw_cleanup {
            self.cleanup_prefixes
        } else {
            Vec::new()
        };
        prefixes.push(ipv6_nat.gateway);
        ra::withdraw_prefixes_once(&self.netlink, snapshot, &prefixes, false).await;
    }
}
