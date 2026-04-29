use std::io;
use std::sync::Arc;

use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::model::{ipv6_to_u128, Ipv6NatPorts, Route, SessionConfig, SessionPorts};
use crate::{dns, ra, tcp, udp, upstream};

pub(crate) struct Session {
    config: Arc<Mutex<SessionConfig>>,
    cleanup_prefixes: Vec<Route>,
    pub(crate) ports: SessionPorts,
    config_changed: Arc<Notify>,
    stop: CancellationToken,
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
        let ipv6_nat = if config.ipv6_nat.is_some() {
            let tcp_listener = upstream::create_tproxy_tcp_listener(config.reply_mark)?;
            let tcp = tcp_listener.local_addr()?.port();
            let udp_listener = upstream::create_tproxy_udp_listener(config.reply_mark)?;
            let udp = udp_listener.local_addr()?.port();
            tcp::spawn_loop(tcp_listener, shared.clone(), stop.clone())?;
            udp::spawn_loop(udp_listener, shared.clone(), stop.clone())?;
            ra::spawn_loop(shared.clone(), config_changed.clone(), stop.clone())?;
            Some(Ipv6NatPorts { tcp, udp })
        } else {
            None
        };

        dns::spawn_tcp_loop(dns_tcp_listener, shared.clone(), stop.clone())?;
        dns::spawn_udp_loop(dns_udp_socket, shared.clone(), stop.clone())?;

        Ok(Self {
            config: shared,
            cleanup_prefixes,
            config_changed,
            stop,
            ports: SessionPorts {
                dns_tcp,
                dns_udp,
                ipv6_nat,
            },
        })
    }

    pub(crate) async fn session_id(&self) -> String {
        self.config.lock().await.session_id.clone()
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
        self.stop.cancel();
    }
}
