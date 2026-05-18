use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use prost::Message;
use tokio::select;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::neighbour::Monitor;
use crate::session::Session;
use crate::{ipsec, nat66, netlink, report, routing};
use vpnhotspotd::shared::ipsec::{IpSecForwardPolicyTarget, UpstreamTracker};
use vpnhotspotd::shared::proto::daemon;
use vpnhotspotd::shared::protocol::{
    ack_event_frame, ack_reply_frame, daemon_error_report_with_details, daemon_io_error_report,
    daemon_io_error_report_with_details, error_frame, ipsec_forward_policy_frame,
    read_session_config, traffic_counters_frame, IoErrorReportExt, IoResultReportExt,
};

mod calls;
mod wire;

use calls::{detach_call, handle_call, send_complete, CallOutput, CallState};
use wire::{connect_control_socket, recv_packet, spawn_writer};

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let controller = connect_control_socket(&socket_name).await?;
    report::stderr!("connected to {socket_name}");
    let (mut controller_read, controller_write) = controller.into_split();
    let (sender, writer) = spawn_writer(controller_write);
    report::init(sender.clone())?;
    let state = Arc::new(State {
        netlink: Mutex::new(None),
        ipsec: Mutex::new(UpstreamTracker::default()),
        icmp: nat66::IcmpDispatcher::new(),
        sessions: Mutex::new(HashMap::new()),
        ipv6_nat_firewall_base: Mutex::new(false),
        neighbour_monitor: Mutex::new(None),
    });
    let active_calls: Arc<Mutex<HashMap<u64, Arc<CallState>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut tasks = JoinSet::new();
    loop {
        let packet = select! {
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(e)) = result {
                    report::message("control.call_join", e.to_string(), "JoinError");
                }
                continue;
            }
            packet = recv_packet(&mut controller_read) => match packet {
                Ok(packet) => packet,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => {
                    report::io("control.recv_packet", e);
                    break;
                }
            },
        };
        let envelope = match daemon::ClientEnvelope::decode(packet.as_slice()) {
            Ok(envelope) => envelope,
            Err(e) => {
                report::io(
                    "control.parse_frame",
                    io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
                );
                break;
            }
        };
        let id = if envelope.call_id == 0 {
            report::io(
                "control.parse_frame",
                io::Error::new(io::ErrorKind::InvalidData, "invalid daemon call id 0"),
            );
            break;
        } else {
            envelope.call_id
        };
        let command = match envelope.command {
            Some(daemon::client_envelope::Command::Cancel(_)) => {
                if let Some(call) = active_calls.lock().await.get(&id) {
                    call.cancel.cancel();
                }
                continue;
            }
            Some(command) => command,
            None => {
                let report = daemon_io_error_report(
                    "control.parse_command",
                    io::Error::new(io::ErrorKind::InvalidData, "missing command"),
                );
                if sender.send(error_frame(id, report)).is_err() {
                    report::stderr!("controller send failed");
                    break;
                }
                continue;
            }
        };
        let call = Arc::new(CallState {
            cancel: CancellationToken::new(),
        });
        {
            let mut active = active_calls.lock().await;
            if active.contains_key(&id) {
                let report = daemon_error_report_with_details(
                    "control.call",
                    "call already active",
                    "AlreadyExists",
                    [("id", id.to_string())],
                );
                if sender.send(error_frame(id, report)).is_err() {
                    report::stderr!("controller send failed");
                    break;
                }
                continue;
            }
            active.insert(id, call.clone());
        }
        tasks.spawn(handle_call(
            id,
            command,
            state.clone(),
            sender.clone(),
            active_calls.clone(),
            call,
        ));
    }
    for call in active_calls.lock().await.values() {
        call.cancel.cancel();
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(e) = result {
            report::message("control.call_join", e.to_string(), "JoinError");
        }
    }
    state.stop(false).await;
    drop(sender);
    if let Err(e) = writer.await {
        report::stderr!("controller writer task failed: {e}");
    }
    Ok(())
}

struct State {
    netlink: Mutex<Option<Arc<netlink::Runtime>>>,
    ipsec: Mutex<UpstreamTracker>,
    icmp: nat66::IcmpDispatcher,
    sessions: Mutex<HashMap<u64, Arc<SessionState>>>,
    ipv6_nat_firewall_base: Mutex<bool>,
    neighbour_monitor: Mutex<Option<MonitorState>>,
}

struct SessionState {
    id: u64,
    downstream: String,
    cleaning: AtomicBool,
    session: Mutex<Option<Session>>,
}

struct MonitorState {
    id: u64,
    cancel: CancellationToken,
    monitor: Monitor,
}

impl State {
    async fn netlink(&self) -> io::Result<Arc<netlink::Runtime>> {
        let mut current = self.netlink.lock().await;
        if let Some(netlink) = current.as_ref() {
            return Ok(netlink.clone());
        }
        let netlink = Arc::new(netlink::Runtime::new()?);
        *current = Some(netlink.clone());
        Ok(netlink)
    }

    async fn stop(&self, withdraw_cleanup: bool) {
        if let Some(monitor) = self.neighbour_monitor.lock().await.take() {
            monitor.monitor.stop().await;
        }
        let sessions = self.drain_sessions().await;
        stop_sessions(&sessions, withdraw_cleanup).await;
        self.ipsec.lock().await.clear();
        self.stop_ipv6_nat_firewall_base().await;
    }

    async fn drain_sessions(&self) -> Vec<Arc<SessionState>> {
        self.sessions
            .lock()
            .await
            .drain()
            .map(|(_, session)| session)
            .collect()
    }

    async fn ensure_ipv6_nat_firewall_base(&self) -> io::Result<()> {
        let mut installed = self.ipv6_nat_firewall_base.lock().await;
        if !*installed {
            routing::ensure_ipv6_nat_firewall_base().await?;
            *installed = true;
        }
        Ok(())
    }

    async fn stop_ipv6_nat_firewall_base(&self) {
        let mut installed = self.ipv6_nat_firewall_base.lock().await;
        if *installed {
            routing::delete_ipv6_nat_firewall_base().await;
            *installed = false;
        }
    }

    async fn session_traffic_snapshots(
        &self,
    ) -> (
        Vec<vpnhotspotd::shared::model::SessionConfig>,
        Vec<daemon::TrafficCounter>,
    ) {
        let sessions = self
            .sessions
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut configs = Vec::with_capacity(sessions.len());
        let mut counters = Vec::new();
        for slot in sessions {
            let guard = slot.session.lock().await;
            if let Some(session) = guard.as_ref() {
                configs.push(session.config_snapshot().await);
                counters.extend(session.traffic_counters().await);
            }
        }
        (configs, counters)
    }

    async fn update_ipsec_session(
        self: &Arc<Self>,
        slot: &Arc<SessionState>,
        config: &vpnhotspotd::shared::model::SessionConfig,
        sender: &UnboundedSender<Vec<u8>>,
    ) {
        let entered = {
            let sessions = self.sessions.lock().await;
            if slot.cleaning.load(Ordering::Acquire)
                || !sessions
                    .get(&slot.id)
                    .is_some_and(|current| Arc::ptr_eq(current, slot))
            {
                return;
            }
            self.ipsec.lock().await.update_session(slot.id, config)
        };
        if !entered.is_empty() {
            let state = self.clone();
            let sender = sender.clone();
            tokio::spawn(async move {
                let result = ipsec::scan(&entered).await;
                state.finish_ipsec_probe(&entered, result, &sender).await;
            });
        }
    }

    async fn finish_ipsec_probe(
        &self,
        interfaces: &[String],
        result: io::Result<Vec<IpSecForwardPolicyTarget>>,
        sender: &UnboundedSender<Vec<u8>>,
    ) {
        match result {
            Ok(targets) => {
                let frames = {
                    let ipsec = self.ipsec.lock().await;
                    targets
                        .into_iter()
                        .filter_map(|target| {
                            let id = ipsec.session_for_interface(&target.interface)?;
                            Some(ipsec_forward_policy_frame(id, &target))
                        })
                        .collect::<Vec<_>>()
                };
                for frame in frames {
                    if sender.send(frame).is_err() {
                        report::stderr!("controller send failed");
                        return;
                    }
                }
            }
            Err(e) => {
                let call_id = {
                    let ipsec = self.ipsec.lock().await;
                    interfaces
                        .iter()
                        .find_map(|interface| ipsec.session_for_interface(interface))
                };
                if let Some(call_id) = call_id {
                    report::report_for(Some(call_id), daemon_io_error_report("ipsec.scan", e));
                }
            }
        }
    }
}

async fn stop_sessions(sessions: &[Arc<SessionState>], withdraw_cleanup: bool) {
    for session in sessions {
        if let Some(session) = session.session.lock().await.take() {
            session.stop(withdraw_cleanup).await;
        }
    }
}

async fn handle_command(
    id: u64,
    command: daemon::client_envelope::Command,
    state: Arc<State>,
    sender: &UnboundedSender<Vec<u8>>,
    active_calls: Arc<Mutex<HashMap<u64, Arc<CallState>>>>,
    cancel: CancellationToken,
) -> io::Result<CallOutput> {
    match command {
        daemon::client_envelope::Command::Cancel(_) => {
            unreachable!("cancel commands are handled before call dispatch")
        }
        daemon::client_envelope::Command::StartSession(command) => {
            let config = read_session_config(command.config.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing start session config")
            })?)?;
            match start_session(id, &state, config, sender, &cancel).await {
                Ok(()) => Ok(CallOutput::NoFrame),
                Err(e) if cancel.is_cancelled() && e.kind() == io::ErrorKind::Interrupted => {
                    Ok(CallOutput::NoFrame)
                }
                Err(e) => Err(e),
            }
        }
        daemon::client_envelope::Command::ReplaceSession(command) => {
            replace_session(
                &state,
                command.session_id,
                read_session_config(command.config.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing replace session config")
                })?)?,
                sender,
            )
            .await?;
            Ok(CallOutput::Reply(ack_reply_frame(id)))
        }
        daemon::client_envelope::Command::ReadTrafficCounters(_) => {
            let (configs, mut counters) = state.session_traffic_snapshots().await;
            counters.extend(
                crate::traffic::read_counters(&configs)
                    .await
                    .with_report_context("control.read_traffic_counters")?,
            );
            Ok(CallOutput::Reply(traffic_counters_frame(id, counters)))
        }
        daemon::client_envelope::Command::StartNeighbourMonitor(_) => {
            start_neighbour_monitor(id, &state, sender.clone(), cancel).await?;
            Ok(CallOutput::NoFrame)
        }
        daemon::client_envelope::Command::ReplaceStaticAddresses(command) => {
            let netlink = state
                .netlink()
                .await
                .with_report_context("control.replace_static_addresses.netlink")?;
            let handle = netlink.handle();
            routing::replace_static_addresses(&handle, &command)
                .await
                .with_report_context_details(
                    "control.replace_static_addresses",
                    [
                        ("dev", command.dev.clone()),
                        ("count", command.addresses.len().to_string()),
                    ],
                )?;
            Ok(CallOutput::Reply(ack_reply_frame(id)))
        }
        daemon::client_envelope::Command::CleanRouting(command) => {
            let sessions = state.drain_sessions().await;
            let mut complete_ids = Vec::new();
            for session in &sessions {
                session.cleaning.store(true, Ordering::Release);
            }
            for session in &sessions {
                if detach_call(session.id, &active_calls).await {
                    complete_ids.push(session.id);
                }
            }
            stop_sessions(&sessions, true).await;
            state.ipsec.lock().await.clear();
            state.stop_ipv6_nat_firewall_base().await;
            for id in complete_ids {
                send_complete(id, sender);
            }
            let netlink = state
                .netlink()
                .await
                .with_report_context("control.clean_routing.netlink")?;
            let handle = netlink.handle();
            routing::clean(&handle, &command)
                .await
                .with_report_context("control.clean_routing")?;
            Ok(CallOutput::Reply(ack_reply_frame(id)))
        }
    }
}

async fn start_session(
    id: u64,
    state: &Arc<State>,
    mut config: vpnhotspotd::shared::model::SessionConfig,
    sender: &UnboundedSender<Vec<u8>>,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let downstream = config.downstream.clone();
    let slot = Arc::new(SessionState {
        id,
        downstream: downstream.clone(),
        cleaning: AtomicBool::new(false),
        session: Mutex::new(None),
    });
    {
        let mut sessions = state.sessions.lock().await;
        if sessions
            .values()
            .any(|session| session.downstream == downstream)
        {
            return Err(
                io::Error::new(io::ErrorKind::AlreadyExists, "session already exists")
                    .with_report_context_details(
                        "control.start_session",
                        [("downstream", downstream)],
                    ),
            );
        }
        sessions.insert(id, slot.clone());
    }
    let netlink = match state
        .netlink()
        .await
        .with_report_context("control.start_session.netlink")
    {
        Ok(netlink) => netlink,
        Err(e) => {
            remove_session_slot(state, &slot).await;
            return Err(e);
        }
    };
    if config.ipv6_nat.is_some() {
        if let Err(e) = state
            .ensure_ipv6_nat_firewall_base()
            .await
            .with_report_context_details(
                "control.start_session.ipv6_nat_firewall_base",
                [("downstream", downstream.as_str())],
            )
        {
            report::report_for(
                Some(id),
                daemon_io_error_report_with_details(
                    "control.start_session.ipv6_nat_firewall_base",
                    e,
                    [("downstream", downstream.as_str())],
                ),
            );
            config.ipv6_nat = None;
        }
    }
    let mut guard = slot.session.lock().await;
    let ipsec_config = config.clone();
    match Session::start(id, config, netlink, &state.icmp, cancel)
        .await
        .with_report_context_details(
            "control.start_session",
            [("downstream", downstream.as_str())],
        ) {
        Ok(session) => {
            *guard = Some(session);
        }
        Err(e) => {
            drop(guard);
            remove_session_slot(state, &slot).await;
            return Err(e);
        }
    }
    drop(guard);
    if sender.send(ack_event_frame(id)).is_err() {
        remove_session(state, &slot, false).await;
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "controller send failed",
        ));
    }
    state
        .update_ipsec_session(&slot, &ipsec_config, sender)
        .await;
    cancel.cancelled().await;
    if !slot.cleaning.load(Ordering::Acquire) {
        remove_session(state, &slot, false).await;
    }
    Ok(())
}

async fn replace_session(
    state: &Arc<State>,
    session_id: u64,
    config: vpnhotspotd::shared::model::SessionConfig,
    sender: &UnboundedSender<Vec<u8>>,
) -> io::Result<()> {
    let slot = state
        .sessions
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "session not found")
                .with_report_context_details(
                    "control.replace_session",
                    [("session_id", session_id.to_string())],
                )
        })?;
    if slot.downstream != config.downstream {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session downstream cannot change",
        )
        .with_report_context_details(
            "control.replace_session",
            [
                ("session_id", session_id.to_string()),
                ("session_downstream", slot.downstream.clone()),
                ("downstream", config.downstream.clone()),
            ],
        ));
    }
    let mut guard = slot.session.lock().await;
    let ipsec_config = config.clone();
    let session = guard.as_mut().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "session not established")
            .with_report_context_details(
                "control.replace_session",
                [
                    ("session_id", session_id.to_string()),
                    ("downstream", config.downstream.clone()),
                ],
            )
    })?;
    session
        .replace_config(config)
        .await
        .with_report_context("control.replace_session")?;
    drop(guard);
    state
        .update_ipsec_session(&slot, &ipsec_config, sender)
        .await;
    Ok(())
}

async fn remove_session(state: &State, slot: &Arc<SessionState>, withdraw_cleanup: bool) {
    let mut guard = slot.session.lock().await;
    if let Some(session) = guard.take() {
        session.stop(withdraw_cleanup).await;
    }
    drop(guard);
    remove_session_slot(state, slot).await;
}

async fn remove_session_slot(state: &State, slot: &Arc<SessionState>) {
    let mut sessions = state.sessions.lock().await;
    if sessions
        .get(&slot.id)
        .is_some_and(|current| Arc::ptr_eq(current, slot))
    {
        sessions.remove(&slot.id);
        drop(sessions);
        state.ipsec.lock().await.remove_session(slot.id);
    }
}

async fn start_neighbour_monitor(
    id: u64,
    state: &State,
    sender: UnboundedSender<Vec<u8>>,
    cancel: CancellationToken,
) -> io::Result<()> {
    let mut current = state.neighbour_monitor.lock().await;
    if current
        .as_ref()
        .is_some_and(|current| !current.cancel.is_cancelled())
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "neighbour monitor already active",
        )
        .with_report_context("control.start_neighbour_monitor"));
    }
    if let Some(monitor) = current.take() {
        monitor.monitor.stop().await;
    }
    let netlink = state
        .netlink()
        .await
        .with_report_context("control.start_neighbour_monitor.netlink")?;
    *current = Some(MonitorState {
        id,
        cancel: cancel.clone(),
        monitor: Monitor::spawn(id, &netlink, sender)
            .await
            .with_report_context("control.start_neighbour_monitor")?,
    });
    drop(current);
    cancel.cancelled().await;
    let mut current = state.neighbour_monitor.lock().await;
    let monitor = if current.as_ref().is_some_and(|current| current.id == id) {
        current.take()
    } else {
        None
    };
    drop(current);
    if let Some(monitor) = monitor {
        monitor.monitor.stop().await;
    }
    Ok(())
}
