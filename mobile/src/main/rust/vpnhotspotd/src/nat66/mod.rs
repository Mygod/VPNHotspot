use std::collections::HashMap;
use std::future::pending;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio_util::sync::CancellationToken;

use cidr::Ipv6Inet;

use crate::report;
use crate::{dns, netlink};
use vpnhotspotd::shared::model::{mac_string, ClientIpv6NatPorts, Ipv6NatPorts, SessionConfig};
use vpnhotspotd::shared::nat66_counter::{Nat66CounterKey, Nat66CounterSource, Nat66Counters};
use vpnhotspotd::shared::proto::daemon;
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
    call_id: u64,
    pub(crate) ports: Ipv6NatPorts,
    shared: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    dns: dns::CounterSink,
    clients: HashMap<[u8; 6], ClientRuntime>,
    counters: Nat66Counters,
    cleanup_prefixes: Vec<Ipv6Inet>,
    netlink: netlink::Handle,
    config_changed: Arc<Notify>,
    ra_task: Option<JoinHandle<()>>,
    _icmp_registration: Option<icmp::Registration>,
}

struct ClientRuntime {
    tcp: Option<ClientListenerRuntime>,
    udp: Option<ClientListenerRuntime>,
}

struct ClientListenerRuntime {
    port: u16,
    stop: CancellationToken,
}

impl Runtime {
    pub(crate) async fn start(
        call_id: u64,
        config: &SessionConfig,
        shared: Arc<Mutex<SessionConfig>>,
        stop: CancellationToken,
        dns: dns::CounterSink,
        netlink: &netlink::Runtime,
        icmp: &IcmpDispatcher,
    ) -> Option<Self> {
        config.ipv6_nat.as_ref()?;
        let config_changed = Arc::new(Notify::new());
        let netlink_handle = netlink.handle();
        let mut runtime = Self {
            call_id,
            ports: Ipv6NatPorts {
                clients: Vec::new(),
                icmp_echo: false,
            },
            shared: shared.clone(),
            stop,
            dns,
            clients: HashMap::new(),
            counters: Nat66Counters::default(),
            cleanup_prefixes: Vec::new(),
            netlink: netlink_handle,
            config_changed: config_changed.clone(),
            ra_task: None,
            _icmp_registration: None,
        };
        runtime.replace_clients(config, icmp);
        if !runtime
            .ports
            .clients
            .iter()
            .any(|client| client.tcp.is_some() || client.udp.is_some())
        {
            return None;
        }
        let icmp_registration = match icmp
            .register(
                config,
                shared.clone(),
                runtime.counters.clone(),
                &runtime.netlink,
            )
            .await
        {
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
        runtime.ports.icmp_echo = icmp_registration.is_some();
        runtime._icmp_registration = icmp_registration;
        runtime.ra_task = match ra::spawn_loop(
            shared.clone(),
            config_changed.clone(),
            netlink.ipv6_address_changed(),
            runtime.netlink.clone(),
            runtime.stop.clone(),
            config,
        ) {
            Ok(task) => Some(task),
            Err(e) => {
                report::report_for(
                    Some(call_id),
                    daemon_io_error_report_with_details(
                        "nat66.ra_start",
                        e,
                        [("downstream", config.downstream.clone())],
                    ),
                );
                None
            }
        };
        Some(runtime)
    }

    pub(crate) fn replace_clients(&mut self, config: &SessionConfig, icmp: &IcmpDispatcher) {
        let mut next_macs = Vec::new();
        if config.ipv6_nat.is_some() {
            for client in &config.clients {
                if !next_macs.contains(&client.mac) {
                    next_macs.push(client.mac);
                }
            }
        }
        self.clients.retain(|mac, client| {
            if next_macs.contains(mac) {
                true
            } else {
                client.cancel();
                false
            }
        });
        for mac in next_macs {
            let needs_tcp = self
                .clients
                .get(&mac)
                .is_none_or(|client| client.tcp.is_none());
            let needs_udp = self
                .clients
                .get(&mac)
                .is_none_or(|client| client.udp.is_none());
            let tcp = if needs_tcp {
                self.start_tcp(config, mac)
            } else {
                None
            };
            let udp = if needs_udp {
                self.start_udp(config, mac, icmp)
            } else {
                None
            };
            let client = self.clients.entry(mac).or_insert(ClientRuntime {
                tcp: None,
                udp: None,
            });
            if needs_tcp {
                client.tcp = tcp;
            }
            if needs_udp {
                client.udp = udp;
            }
        }
        self.clients
            .retain(|_, client| client.tcp.is_some() || client.udp.is_some());
        let mut client_macs = Vec::new();
        self.ports.clients = config
            .clients
            .iter()
            .filter_map(|client| {
                if client_macs.contains(&client.mac) {
                    None
                } else {
                    client_macs.push(client.mac);
                    self.clients
                        .get(&client.mac)
                        .and_then(|runtime| runtime.ports(client.mac))
                }
            })
            .collect();
    }

    pub(crate) fn retain_ports(&mut self, committed: Option<&Ipv6NatPorts>) {
        let icmp_echo = committed.is_some_and(|ports| ports.icmp_echo);
        if !icmp_echo {
            self._icmp_registration = None;
        }
        self.clients.retain(|mac, client| {
            let committed =
                committed.and_then(|ports| ports.clients.iter().find(|ports| ports.mac == *mac));
            if committed.and_then(|ports| ports.tcp).is_none() {
                if let Some(runtime) = client.tcp.take() {
                    runtime.stop.cancel();
                }
            }
            if committed.and_then(|ports| ports.udp).is_none() {
                if let Some(runtime) = client.udp.take() {
                    runtime.stop.cancel();
                }
            }
            if client.tcp.is_some() || client.udp.is_some() {
                true
            } else {
                client.cancel();
                false
            }
        });
        self.ports = committed.cloned().unwrap_or(Ipv6NatPorts {
            clients: Vec::new(),
            icmp_echo: false,
        });
    }

    fn start_tcp(&self, config: &SessionConfig, mac: [u8; 6]) -> Option<ClientListenerRuntime> {
        let stop = self.stop.child_token();
        match tproxy::create_tcp_listener(config.reply_mark).and_then(|listener| {
            let port = listener.local_addr()?.port();
            tcp::spawn_loop(
                listener,
                self.shared.clone(),
                stop.clone(),
                self.counters.clone(),
                self.dns.clone(),
                mac,
            )?;
            Ok(port)
        }) {
            Ok(port) => Some(ClientListenerRuntime { port, stop }),
            Err(e) => {
                stop.cancel();
                report_client_start_error(
                    self.call_id,
                    "nat66.tcp_start",
                    e,
                    &config.downstream,
                    mac,
                );
                None
            }
        }
    }

    fn start_udp(
        &self,
        config: &SessionConfig,
        mac: [u8; 6],
        icmp: &IcmpDispatcher,
    ) -> Option<ClientListenerRuntime> {
        let stop = self.stop.child_token();
        match tproxy::create_udp_listener(config.reply_mark).and_then(|listener| {
            let port = listener.local_addr()?.port();
            udp::spawn_loop(
                listener,
                self.shared.clone(),
                stop.clone(),
                icmp.clone(),
                self.counters.clone(),
                self.dns.clone(),
                mac,
            )?;
            Ok(port)
        }) {
            Ok(port) => Some(ClientListenerRuntime { port, stop }),
            Err(e) => {
                stop.cancel();
                report_client_start_error(
                    self.call_id,
                    "nat66.udp_start",
                    e,
                    &config.downstream,
                    mac,
                );
                None
            }
        }
    }

    pub(crate) fn record_config_replacement(
        &mut self,
        previous: &SessionConfig,
        next: &SessionConfig,
    ) {
        let mut previous_macs = Vec::new();
        for client in &previous.clients {
            if !previous_macs.contains(&client.mac) {
                previous_macs.push(client.mac);
            }
        }
        for mac in previous_macs {
            if next.clients.iter().any(|client| client.mac == mac) {
                continue;
            }
            if let Some(registration) = self._icmp_registration.as_ref() {
                if let Err(e) = registration.remove_client(mac) {
                    report::io_with_details(
                        "nat66.icmp_remove_client",
                        e,
                        [
                            ("downstream", previous.downstream.clone()),
                            ("mac", mac_string(&mac)),
                        ],
                    );
                }
            }
        }
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

    pub(crate) async fn traffic_counters(&self) -> Vec<daemon::TrafficCounter> {
        let config = self.shared.lock().await;
        let mut active = Vec::new();
        for ports in &self.ports.clients {
            if ports.tcp.is_some() {
                active.push(Nat66CounterKey {
                    mac: ports.mac,
                    source: Nat66CounterSource::Tcp,
                });
            }
            if ports.udp.is_some() {
                active.push(Nat66CounterKey {
                    mac: ports.mac,
                    source: Nat66CounterSource::Udp,
                });
            }
        }
        if self.ports.icmp_echo && config.ipv6_nat.is_some() {
            for client in &config.clients {
                if !active
                    .iter()
                    .any(|key| key.mac == client.mac && key.source == Nat66CounterSource::Icmpv6)
                {
                    active.push(Nat66CounterKey {
                        mac: client.mac,
                        source: Nat66CounterSource::Icmpv6,
                    });
                }
            }
        }
        match self.counters.counters(&config.downstream, active) {
            Ok(counters) => counters,
            Err(e) => {
                report::io("nat66.counter", e);
                Vec::new()
            }
        }
    }

    pub(crate) async fn stop(self, snapshot: &SessionConfig, withdraw_cleanup: bool) {
        let Self {
            stop,
            clients,
            cleanup_prefixes,
            netlink,
            ra_task,
            _icmp_registration,
            ..
        } = self;
        stop.cancel();
        for client in clients.into_values() {
            client.cancel();
        }
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

impl ClientRuntime {
    fn ports(&self, mac: [u8; 6]) -> Option<ClientIpv6NatPorts> {
        let tcp = self.tcp.as_ref().map(|runtime| runtime.port);
        let udp = self.udp.as_ref().map(|runtime| runtime.port);
        if tcp.is_some() || udp.is_some() {
            Some(ClientIpv6NatPorts { mac, tcp, udp })
        } else {
            None
        }
    }

    fn cancel(&self) {
        if let Some(runtime) = self.tcp.as_ref() {
            runtime.stop.cancel();
        }
        if let Some(runtime) = self.udp.as_ref() {
            runtime.stop.cancel();
        }
    }
}

fn report_client_start_error(
    call_id: u64,
    context: &str,
    error: io::Error,
    downstream: &str,
    mac: [u8; 6],
) {
    report::report_for(
        Some(call_id),
        daemon_io_error_report_with_details(
            context,
            error,
            [
                ("downstream", downstream.to_owned()),
                ("mac", mac_string(&mac)),
            ],
        ),
    );
}
