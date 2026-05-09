use std::io;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{dns, downstream, nat66, netlink, report, routing};
use vpnhotspotd::shared::downstream::DownstreamIpv4;
use vpnhotspotd::shared::model::{SessionConfig, SessionPorts};
use vpnhotspotd::shared::protocol::DaemonErrorReport;

pub(crate) struct Session {
    config: Arc<Mutex<SessionConfig>>,
    _dns: dns::Runtime,
    nat66: Option<nat66::Runtime>,
    routing: routing::Runtime,
    downstream_ipv4: DownstreamIpv4,
    stop: CancellationToken,
}

impl Session {
    pub(crate) async fn start(
        call_id: u64,
        mut config: SessionConfig,
        netlink: &netlink::Runtime,
        cancel: &CancellationToken,
    ) -> io::Result<Self> {
        let stop = CancellationToken::new();
        let downstream_ipv4 = downstream::wait_ipv4(
            &netlink.handle(),
            netlink.ipv4_address_changed(),
            &config.downstream,
            cancel,
        )
        .await?;
        let shared = Arc::new(Mutex::new(config.clone()));
        let dns = match dns::Runtime::start(
            downstream_ipv4.address,
            config.reply_mark,
            shared.clone(),
            stop.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(e) => {
                stop.cancel();
                return Err(e);
            }
        };
        let nat66 = match nat66::Runtime::start(
            &config,
            shared.clone(),
            stop.child_token(),
            netlink.handle(),
            netlink.ipv6_address_changed(),
        ) {
            Ok(runtime) => runtime,
            Err(e) => {
                report::report_for(
                    Some(call_id),
                    DaemonErrorReport::from_io_error_with_details(
                        "session.start_ipv6_nat",
                        e,
                        [("downstream", config.downstream.as_str())],
                    ),
                );
                config.ipv6_nat = None;
                shared.lock().await.ipv6_nat = None;
                None
            }
        };
        let ports = SessionPorts {
            dns_tcp: dns.tcp_port,
            dns_udp: dns.udp_port,
            ipv6_nat: nat66.as_ref().map(|runtime| runtime.ports),
        };
        let routing =
            routing::Runtime::start(call_id, &config, downstream_ipv4, ports, netlink.handle())
                .await;
        Ok(Self {
            config: shared,
            _dns: dns,
            nat66,
            routing,
            downstream_ipv4,
            stop,
        })
    }

    pub(crate) async fn replace_config(&mut self, config: SessionConfig) -> io::Result<()> {
        let mut config = config;
        if self.nat66.is_none() {
            config.ipv6_nat = None;
        }
        {
            let mut current = self.config.lock().await;
            self.routing
                .replace(&current, &config, self.downstream_ipv4)
                .await?;
            if let Some(nat66) = self.nat66.as_mut() {
                nat66.record_config_replacement(&current, &config);
            }
            *current = config;
        }
        if let Some(nat66) = self.nat66.as_ref() {
            nat66.notify_config_changed();
        }
        Ok(())
    }

    pub(crate) async fn stop(self, withdraw_cleanup: bool) {
        let snapshot = self.config.lock().await.clone();
        self.routing.stop().await;
        self.stop.cancel();
        if let Some(nat66) = self.nat66 {
            nat66.stop(&snapshot, withdraw_cleanup).await;
        }
    }
}
