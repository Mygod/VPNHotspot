use std::io;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{dns, downstream, nat66, netlink, routing};
use vpnhotspotd::shared::downstream::DownstreamIpv4;
use vpnhotspotd::shared::model::{
    has_client_scoped_ipv6_nat_demand, should_disable_uncommitted_ipv6_nat, Ipv6NatPorts,
    SessionConfig, SessionPorts,
};

pub(crate) struct Session {
    call_id: u64,
    config: Arc<Mutex<SessionConfig>>,
    dns: dns::Runtime,
    nat66: Option<nat66::Runtime>,
    icmp: nat66::IcmpDispatcher,
    netlink: Arc<netlink::Runtime>,
    routing: routing::Runtime,
    downstream_ipv4: DownstreamIpv4,
    stop: CancellationToken,
}

impl Session {
    pub(crate) async fn start(
        call_id: u64,
        mut config: SessionConfig,
        netlink: Arc<netlink::Runtime>,
        icmp: &nat66::IcmpDispatcher,
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
        let mut dns = dns::Runtime::start(
            call_id,
            &config.downstream,
            downstream_ipv4.address,
            config.reply_mark,
            shared.clone(),
            stop.clone(),
            &config,
        );
        let mut nat66 = nat66::Runtime::start(
            call_id,
            &config,
            shared.clone(),
            stop.child_token(),
            dns.counter_sink(),
            &netlink,
            icmp,
        )
        .await;
        if nat66.is_none() && has_client_scoped_ipv6_nat_demand(&config) {
            config.ipv6_nat = None;
            shared.lock().await.ipv6_nat = None;
        }
        let candidate_ports = SessionPorts {
            dns: dns.ports(),
            ipv6_nat: candidate_ipv6_nat_ports(&config, nat66.as_ref()),
        };
        let (mut routing, mut committed_ports) = routing::Runtime::start(
            call_id,
            &config,
            downstream_ipv4,
            candidate_ports,
            netlink.handle(),
        )
        .await;
        dns.retain_ports(&committed_ports.dns);
        if let Some(nat66) = nat66.as_mut() {
            nat66.retain_ports(committed_ports.ipv6_nat.as_ref());
        }
        if should_disable_uncommitted_ipv6_nat(&config, &committed_ports) {
            let previous = config.clone();
            config.ipv6_nat = None;
            shared.lock().await.ipv6_nat = None;
            committed_ports.ipv6_nat = None;
            committed_ports = routing
                .replace(&previous, &config, downstream_ipv4, committed_ports)
                .await?;
            dns.retain_ports(&committed_ports.dns);
            if let Some(nat66) = nat66.take() {
                nat66.stop(&previous, true).await;
            }
        }
        Ok(Self {
            call_id,
            config: shared,
            dns,
            nat66,
            icmp: icmp.clone(),
            netlink,
            routing,
            downstream_ipv4,
            stop,
        })
    }

    pub(crate) async fn replace_config(&mut self, config: SessionConfig) -> io::Result<()> {
        let mut config = config;
        let mut nat66_to_stop = None;
        let mut nat66_stop_snapshot = None;
        if self.nat66.is_none() {
            let current = self.config.lock().await.clone();
            if current.ipv6_nat.is_some() && has_client_scoped_ipv6_nat_demand(&config) {
                self.nat66 = nat66::Runtime::start(
                    self.call_id,
                    &config,
                    self.config.clone(),
                    self.stop.child_token(),
                    self.dns.counter_sink(),
                    &self.netlink,
                    &self.icmp,
                )
                .await;
            }
            if self.nat66.is_none()
                && (current.ipv6_nat.is_none() || has_client_scoped_ipv6_nat_demand(&config))
            {
                config.ipv6_nat = None;
            }
        }
        {
            let mut current = self.config.lock().await;
            self.dns.replace_clients(&config);
            if let Some(nat66) = self.nat66.as_mut() {
                nat66.replace_clients(&config, &self.icmp);
            }
            let candidate_ports = SessionPorts {
                dns: self.dns.ports(),
                ipv6_nat: candidate_ipv6_nat_ports(&config, self.nat66.as_ref()),
            };
            let committed_ports = match self
                .routing
                .replace(&current, &config, self.downstream_ipv4, candidate_ports)
                .await
            {
                Ok(ports) => ports,
                Err(e) => {
                    self.dns.replace_clients(&current);
                    if let Some(nat66) = self.nat66.as_mut() {
                        nat66.replace_clients(&current, &self.icmp);
                    }
                    return Err(e);
                }
            };
            self.dns.retain_ports(&committed_ports.dns);
            if config.ipv6_nat.is_none() || has_client_scoped_ipv6_nat_demand(&config) {
                if let Some(nat66) = self.nat66.as_mut() {
                    nat66.retain_ports(committed_ports.ipv6_nat.as_ref());
                }
            }
            if config.ipv6_nat.is_none() {
                nat66_to_stop = self.nat66.take();
                nat66_stop_snapshot = Some(current.clone());
            } else if should_disable_uncommitted_ipv6_nat(&config, &committed_ports) {
                let previous = config.clone();
                config.ipv6_nat = None;
                let ports = SessionPorts {
                    dns: self.dns.ports(),
                    ipv6_nat: None,
                };
                let committed_ports = self
                    .routing
                    .replace(&previous, &config, self.downstream_ipv4, ports)
                    .await?;
                self.dns.retain_ports(&committed_ports.dns);
                nat66_to_stop = self.nat66.take();
                nat66_stop_snapshot = Some(previous);
            }
            if let Some(nat66) = self.nat66.as_mut() {
                nat66.record_config_replacement(&current, &config);
            }
            *current = config;
        }
        if let (Some(nat66), Some(snapshot)) = (nat66_to_stop, nat66_stop_snapshot) {
            nat66.stop(&snapshot, true).await;
        }
        if let Some(nat66) = self.nat66.as_ref() {
            nat66.notify_config_changed();
        }
        Ok(())
    }

    pub(crate) async fn config_snapshot(&self) -> SessionConfig {
        self.config.lock().await.clone()
    }

    pub(crate) async fn traffic_counters(
        &self,
    ) -> Vec<vpnhotspotd::shared::proto::daemon::TrafficCounter> {
        let mut counters = self.dns.counters().await;
        if let Some(nat66) = self.nat66.as_ref() {
            counters.extend(nat66.traffic_counters().await);
        }
        counters
    }

    pub(crate) async fn stop(self, withdraw_cleanup: bool) {
        let snapshot = self.config.lock().await.clone();
        self.stop.cancel();
        self.routing.stop().await;
        if let Some(nat66) = self.nat66 {
            nat66.stop(&snapshot, withdraw_cleanup).await;
        }
    }
}

fn candidate_ipv6_nat_ports(
    config: &SessionConfig,
    nat66: Option<&nat66::Runtime>,
) -> Option<Ipv6NatPorts> {
    if has_client_scoped_ipv6_nat_demand(config) {
        nat66.map(|runtime| runtime.ports.clone())
    } else {
        None
    }
}
