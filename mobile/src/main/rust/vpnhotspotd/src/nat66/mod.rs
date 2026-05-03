use std::io;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use vpnhotspotd::shared::model::{ipv6_to_u128, Ipv6NatPorts, Route, SessionConfig};

mod ra;
mod tcp;
mod tproxy;
mod udp;

pub(crate) struct Runtime {
    pub(crate) ports: Ipv6NatPorts,
    cleanup_prefixes: Vec<Route>,
    config_changed: Arc<Notify>,
    ra_task: JoinHandle<()>,
}

impl Runtime {
    pub(crate) fn start(
        config: &SessionConfig,
        shared: Arc<Mutex<SessionConfig>>,
        stop: CancellationToken,
    ) -> io::Result<Option<Self>> {
        let Some(ipv6_nat) = config.ipv6_nat.as_ref() else {
            return Ok(None);
        };
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
        let ra_task = match ra::spawn_loop(shared, config_changed.clone(), stop.clone(), config) {
            Ok(task) => task,
            Err(e) => {
                stop.cancel();
                return Err(e);
            }
        };
        Ok(Some(Self {
            ports: Ipv6NatPorts { tcp, udp },
            cleanup_prefixes: ipv6_nat.cleanup_prefixes.clone(),
            config_changed,
            ra_task,
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
        for prefix in next.cleanup_prefixes.iter().copied() {
            if !self.cleanup_prefixes.contains(&prefix) {
                self.cleanup_prefixes.push(prefix);
            }
        }
        if let Some(previous) = previous.ipv6_nat.as_ref() {
            if previous.gateway != next.gateway || previous.prefix_len != next.prefix_len {
                let previous_prefix = Route {
                    prefix: ipv6_to_u128(previous.gateway),
                    prefix_len: previous.prefix_len,
                };
                if !self.cleanup_prefixes.contains(&previous_prefix) {
                    self.cleanup_prefixes.push(previous_prefix);
                }
            }
        }
    }

    pub(crate) fn notify_config_changed(&self) {
        self.config_changed.notify_one();
    }

    pub(crate) async fn stop(self, snapshot: &SessionConfig, withdraw_cleanup: bool) {
        if let Err(e) = self.ra_task.await {
            eprintln!("ra task join failed: {e}");
        }
        let Some(ipv6_nat) = snapshot.ipv6_nat.as_ref() else {
            return;
        };
        let mut prefixes = if withdraw_cleanup {
            self.cleanup_prefixes
        } else {
            Vec::new()
        };
        prefixes.push(Route {
            prefix: ipv6_to_u128(ipv6_nat.gateway),
            prefix_len: ipv6_nat.prefix_len,
        });
        ra::withdraw_prefixes_once(snapshot, &prefixes, false).await;
    }
}
