use std::io;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::nat66::{ra, tcp, udp};
use crate::{dns, upstream};
use vpnhotspotd::shared::model::{ipv6_to_u128, Ipv6NatPorts, Route, SessionConfig, SessionPorts};

pub(crate) struct Session {
    config: Arc<Mutex<SessionConfig>>,
    cleanup_prefixes: Vec<Route>,
    pub(crate) ports: SessionPorts,
    config_changed: Arc<Notify>,
    stop: CancellationToken,
    ra_task: Option<JoinHandle<()>>,
}

impl Session {
    pub(crate) async fn start(config: SessionConfig) -> io::Result<Self> {
        let stop = CancellationToken::new();
        let shared = Arc::new(Mutex::new(config.clone()));
        let config_changed = Arc::new(Notify::new());
        let cleanup_prefixes = config
            .ipv6_nat
            .as_ref()
            .map(|ipv6_nat| ipv6_nat.cleanup_prefixes.clone())
            .unwrap_or_default();

        let dns_tcp_listener = dns::create_tcp_listener(config.dns_bind_address)?;
        let dns_tcp = dns_tcp_listener.local_addr()?.port();
        let dns_udp_socket = dns::create_udp_listener(config.dns_bind_address)?;
        let dns_udp = dns_udp_socket.local_addr()?.port();
        let (ipv6_nat, ra_task) = if config.ipv6_nat.is_some() {
            let tcp_listener = upstream::create_tproxy_tcp_listener(config.reply_mark)?;
            let tcp = tcp_listener.local_addr()?.port();
            let udp_listener = upstream::create_tproxy_udp_listener(config.reply_mark)?;
            let udp = udp_listener.local_addr()?.port();
            tcp::spawn_loop(tcp_listener, shared.clone(), stop.clone())?;
            if let Err(e) = udp::spawn_loop(udp_listener, shared.clone(), stop.clone()) {
                stop.cancel();
                return Err(e);
            }
            let ra_task = match ra::spawn_loop(
                shared.clone(),
                config_changed.clone(),
                stop.clone(),
                &config,
            ) {
                Ok(task) => task,
                Err(e) => {
                    stop.cancel();
                    return Err(e);
                }
            };
            (Some(Ipv6NatPorts { tcp, udp }), Some(ra_task))
        } else {
            (None, None)
        };

        if let Err(e) = dns::spawn_tcp_loop(dns_tcp_listener, shared.clone(), stop.clone()) {
            stop.cancel();
            return Err(e);
        }
        if let Err(e) = dns::spawn_udp_loop(dns_udp_socket, shared.clone(), stop.clone()) {
            stop.cancel();
            return Err(e);
        }

        Ok(Self {
            config: shared,
            cleanup_prefixes,
            config_changed,
            stop,
            ra_task,
            ports: SessionPorts {
                dns_tcp,
                dns_udp,
                ipv6_nat,
            },
        })
    }

    pub(crate) async fn replace_config(&mut self, config: SessionConfig) {
        if let Some(ipv6_nat) = config.ipv6_nat.as_ref() {
            for prefix in ipv6_nat.cleanup_prefixes.iter().copied() {
                if !self.cleanup_prefixes.contains(&prefix) {
                    self.cleanup_prefixes.push(prefix);
                }
            }
            let current = self.config.lock().await;
            if let Some(current) = current.ipv6_nat.as_ref() {
                if current.gateway != ipv6_nat.gateway || current.prefix_len != ipv6_nat.prefix_len
                {
                    let previous_prefix = Route {
                        prefix: ipv6_to_u128(current.gateway),
                        prefix_len: current.prefix_len,
                    };
                    if !self.cleanup_prefixes.contains(&previous_prefix) {
                        self.cleanup_prefixes.push(previous_prefix);
                    }
                }
            }
        }
        let mut current = self.config.lock().await;
        *current = config;
        self.config_changed.notify_one();
    }

    pub(crate) async fn stop(self, withdraw_cleanup: bool) {
        self.stop.cancel();
        if let Some(ra_task) = self.ra_task {
            if let Err(e) = ra_task.await {
                eprintln!("ra task join failed: {e}");
            }
        }
        let snapshot = self.config.lock().await.clone();
        if let Some(ipv6_nat) = snapshot.ipv6_nat.as_ref() {
            let mut prefixes = if withdraw_cleanup {
                self.cleanup_prefixes.clone()
            } else {
                Vec::new()
            };
            prefixes.push(Route {
                prefix: ipv6_to_u128(ipv6_nat.gateway),
                prefix_len: ipv6_nat.prefix_len,
            });
            ra::withdraw_prefixes_once(&snapshot, &prefixes, false).await;
        }
    }
}
