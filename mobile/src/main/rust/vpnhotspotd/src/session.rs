use std::io;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{dns, nat66};
use vpnhotspotd::shared::model::{SessionConfig, SessionPorts};

pub(crate) struct Session {
    config: Arc<Mutex<SessionConfig>>,
    dns: dns::Runtime,
    nat66: Option<nat66::Runtime>,
    stop: CancellationToken,
}

impl Session {
    pub(crate) async fn start(config: SessionConfig) -> io::Result<Self> {
        let stop = CancellationToken::new();
        let shared = Arc::new(Mutex::new(config.clone()));
        let dns = match dns::Runtime::start(config.dns_bind_address, shared.clone(), stop.clone()) {
            Ok(runtime) => runtime,
            Err(e) => {
                stop.cancel();
                return Err(e);
            }
        };
        let nat66 = match nat66::Runtime::start(&config, shared.clone(), stop.clone()) {
            Ok(runtime) => runtime,
            Err(e) => {
                stop.cancel();
                return Err(e);
            }
        };
        Ok(Self {
            config: shared,
            dns,
            nat66,
            stop,
        })
    }

    pub(crate) fn ports(&self) -> SessionPorts {
        SessionPorts {
            dns_tcp: self.dns.tcp_port,
            dns_udp: self.dns.udp_port,
            ipv6_nat: self.nat66.as_ref().map(|runtime| runtime.ports),
        }
    }

    pub(crate) async fn replace_config(&mut self, config: SessionConfig) {
        {
            let mut current = self.config.lock().await;
            if let Some(nat66) = self.nat66.as_mut() {
                nat66.record_config_replacement(&current, &config);
            }
            *current = config;
        }
        if let Some(nat66) = self.nat66.as_ref() {
            nat66.notify_config_changed();
        }
    }

    pub(crate) async fn stop(self, withdraw_cleanup: bool) {
        self.stop.cancel();
        let snapshot = self.config.lock().await.clone();
        if let Some(nat66) = self.nat66 {
            nat66.stop(&snapshot, withdraw_cleanup).await;
        }
    }
}
