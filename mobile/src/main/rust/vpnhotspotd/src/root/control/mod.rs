use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use prost::Message;
use tokio::select;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::report::{ControllerSender, ControllerSenderExt};
use crate::root::{ipsec, nat66, neighbour, netlink, routing, session::Session};
use crate::{control_wire, report};
use vpnhotspotd::shared::ipsec::{Finished, IpSecForwardPolicyTarget, Probe, UpstreamTracker};
use vpnhotspotd::shared::model::DAEMON_REPLY_MARK;
use vpnhotspotd::shared::proto::daemon;
use vpnhotspotd::shared::protocol::{
    ack_event_frame, ack_reply_frame, daemon_error_report_with_details, daemon_io_error_report,
    daemon_io_error_report_with_details, error_frame, ipsec_forward_policy_frame,
    read_session_config, traffic_counters_frame, IoErrorReportExt, IoResultReportExt,
};
use vpnhotspotd::shared::tasks::{combine, Background};

mod calls;
mod session_control;

use calls::{detach_call, handle_call, send_complete, CallOutput, CallState};
use control_wire::{connect_control_socket, recv_packet, spawn_writer};
use session_control::{read_session_counters, run_session, stop_sessions, SessionControl};

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let controller = connect_control_socket(&socket_name).await?;
    report::stderr!("connected to {socket_name}");
    let (mut controller_read, controller_write) = controller.into_split();
    // Cancelled by the writer when the control socket can no longer carry a frame, which is the only way the
    // read loop below learns about a peer that closed just its read half.
    let cancel = CancellationToken::new();
    let (sender, writer) = spawn_writer(controller_write, cancel.clone());
    let reporter = report::init(sender.clone())?;
    // Reporting stays open until every detached report-capable task exits.
    let detached = TaskTracker::new();
    let state = Arc::new(State {
        ipsec: Mutex::new(UpstreamTracker::default()),
        nat66: nat66::ProcessResources::new(DAEMON_REPLY_MARK, detached.clone()),
        sessions: Mutex::new(HashMap::new()),
        probes: Background::new("control.ipsec_probe"),
        ipv6_nat_firewall_base: Mutex::new(false),
        neighbour_monitor: Mutex::new(None),
        detached: detached.clone(),
    });
    let active_calls: Arc<Mutex<HashMap<u64, Arc<CallState>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut tasks = JoinSet::new();
    loop {
        let packet = select! {
            // Biased, and this arm first, because "first" has to be real rather than documented: an
            // unbiased select picks at random among ready arms, so a writer that has just failed could lose
            // to a buffered command and this loop would dispatch a new call - starting side effects - after
            // output was already terminal. A dead control socket ends the conversation instead, rather than
            // leaving it parked on a read that will never return while root routing and IPsec probes stay
            // live behind it.
            biased;
            () = cancel.cancelled() => break,
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
                if !sender.send_frame(error_frame(id, report)) {
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
                if !sender.send_frame(error_frame(id, report)) {
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
    if let Err(e) = state.probes.close().await {
        report::message("control.ipsec_probe_join", e.to_string(), "JoinError");
    }
    state.stop(false).await;
    // Drop root's ICMP-dispatcher owner so its final clone drops before or inside the tracker wait.
    drop(state);
    detached.close();
    detached.wait().await;
    // Task destruction can report, so finish only after the tracker is empty.
    let reported = reporter.finish().await;
    drop(sender);
    let writing = match writer.await {
        Ok(result) => result,
        Err(e) => Err(io::Error::other(format!(
            "controller writer task failed: {e}"
        ))),
    };
    // Reporter and writer failures are independent.
    combine(reported, writing)
}

struct State {
    ipsec: Mutex<UpstreamTracker>,
    nat66: nat66::ProcessResources,
    sessions: Mutex<HashMap<u64, Arc<SessionState>>>,
    /// The IPsec probes calls have started. Owned by the process rather than detached, and joined by [run]
    /// before anything a probe touches is torn down - see [Background].
    probes: Background,
    ipv6_nat_firewall_base: Mutex<bool>,
    neighbour_monitor: Mutex<Option<MonitorState>>,
    /// Detached report-capable work for this conversation.
    detached: TaskTracker,
}

struct SessionState {
    id: u64,
    downstream: String,
    cancel: CancellationToken,
    teardown_complete: CancellationToken,
    cleaning: AtomicBool,
    control: Mutex<Option<SessionControl>>,
}

struct MonitorState {
    id: u64,
    cancel: CancellationToken,
}

impl State {
    async fn stop(&self, withdraw_cleanup: bool) {
        if let Some(monitor) = self.neighbour_monitor.lock().await.take() {
            monitor.cancel.cancel();
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

    async fn update_ipsec_session(
        self: &Arc<Self>,
        slot: &Arc<SessionState>,
        config: &vpnhotspotd::shared::model::SessionConfig,
        sender: &ControllerSender,
    ) {
        let probe = {
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
        let Some(probe) = probe else {
            // Either nothing changed that a scan could answer, or one is already running and this update is
            // recorded as the rescan it owes - which the probe below picks up rather than losing.
            return;
        };
        let state = self.clone();
        let sender = sender.clone();
        // Admitted rather than spawned, so the handle is kept: this task mutates the tracker and sends frames
        // on the control writer, both of which the process tears down on its way out. A refused admission
        // means the process is already stopping, and the tracker's probing flag goes with the [State::stop]
        // that refused it.
        let admitted = self
            .probes
            .admit(|cancel| async move {
                // One flight, however many updates it has to answer for. Looping here rather than admitting a
                // second task keeps "one scan at a time" true and keeps the update that arrived mid-scan from
                // being answered by a scan that predates it.
                let mut probe = probe;
                loop {
                    let result = select! {
                        biased;
                        // The one expected ending, and a quiet one: the process is stopping, so there is no
                        // tracker left to update and no conversation left to tell.
                        () = cancel.cancelled() => return,
                        result = ipsec::scan() => result,
                    };
                    match state.finish_ipsec_probe(probe, result, &sender).await {
                        Some(rescan) => probe = rescan,
                        None => return,
                    }
                }
            })
            .await;
        if let Err(e) = admitted.reaped {
            report::message("control.ipsec_probe_join", e.to_string(), "JoinError");
        }
        if !admitted.started {
            report::stderr!("ipsec probe skipped: the daemon is stopping");
        }
    }

    /// Commits one probe's answer, if it still speaks for the tracker's sessions, and hands back the rescan it
    /// owes either way.
    ///
    /// The rescan is not optional for the caller: the tracker has handed it this flight, so a `Some` that is
    /// not run leaves the tracker believing a scan is in progress and nothing would ever start another. The
    /// loop below runs it; the two paths that return `None` with one outstanding are both the conversation
    /// itself ending, where [State::stop] clears the tracker.
    async fn finish_ipsec_probe(
        &self,
        probe: Probe,
        result: io::Result<Vec<IpSecForwardPolicyTarget>>,
        sender: &ControllerSender,
    ) -> Option<Probe> {
        match result {
            Ok(targets) => {
                let (frames, rescan) = {
                    let mut ipsec = self.ipsec.lock().await;
                    let Finished { current, rescan } = ipsec.finish_probe(probe);
                    // A probe the sessions moved out from under - a clean, or a replacement this scan predates
                    // - speaks for a session set that no longer exists. Committing it would forget the targets
                    // a newer scan has already sent, and forgetting those is what makes them be sent again;
                    // publishing it would attribute what it did see from the session set it no longer speaks
                    // for. So nothing it saw is used, and the rescan below is what answers for the change.
                    if current {
                        ipsec.retain_observed_targets(&targets);
                        let frames = targets
                            .into_iter()
                            .filter_map(|target| {
                                let id = ipsec.session_for_new_target(&target)?;
                                Some(ipsec_forward_policy_frame(id, &target))
                            })
                            .collect::<Vec<_>>();
                        (frames, rescan)
                    } else {
                        (Vec::new(), rescan)
                    }
                };
                for frame in frames {
                    if !sender.send_frame(frame) {
                        report::stderr!("controller send failed");
                        return None;
                    }
                }
                rescan
            }
            Err(e) => {
                // A scan that failed is reported whoever it belonged to - the failure is the daemon's own
                // either way.
                let rescan = self.ipsec.lock().await.finish_probe(probe).rescan;
                report::report_for(None, daemon_io_error_report("ipsec.scan", e));
                rescan
            }
        }
    }
}

async fn handle_command(
    id: u64,
    command: daemon::client_envelope::Command,
    state: Arc<State>,
    sender: &ControllerSender,
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
            let sessions = state
                .sessions
                .lock()
                .await
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut configs = Vec::with_capacity(sessions.len());
            let mut counters = Vec::new();
            for slot in sessions {
                if let Some(snapshot) = read_session_counters(&slot).await {
                    configs.push(snapshot.config);
                    counters.extend(snapshot.counters);
                }
            }
            match crate::root::traffic::read_counters(&configs)
                .await
                .with_report_context("control.read_traffic_counters")
            {
                Ok(ipv4_counters) => counters.extend(ipv4_counters),
                Err(e) => {
                    report::report_for(
                        Some(id),
                        daemon_io_error_report("control.read_traffic_counters", e),
                    );
                }
            }
            Ok(CallOutput::Reply(traffic_counters_frame(id, counters)))
        }
        daemon::client_envelope::Command::StartNeighbourMonitor(_) => {
            start_neighbour_monitor(id, &state, sender.clone(), cancel).await?;
            Ok(CallOutput::NoFrame)
        }
        daemon::client_envelope::Command::ReplaceStaticAddresses(command) => {
            let mut handle = netlink::RequestConnection::new(&state.detached)
                .with_report_context("control.replace_static_addresses.netlink")?;
            routing::replace_static_addresses(&mut handle, &command)
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
        // The two command families are never served by the same process. Refused on the call rather than
        // ignored, so an app that sent one to the wrong daemon learns which boundary it crossed.
        daemon::client_envelope::Command::StartShizukuSession(_)
        | daemon::client_envelope::Command::ApplyShizukuConfig(_) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "app-UID command sent to the root daemon",
        )
        .with_report_context("control.handle_command")),
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
            let mut handle = netlink::RequestConnection::new(&state.detached)
                .with_report_context("control.clean_routing.netlink")?;
            routing::clean(&mut handle, &command)
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
    sender: &ControllerSender,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let downstream = config.downstream.clone();
    let slot = Arc::new(SessionState {
        id,
        downstream: downstream.clone(),
        cancel: cancel.clone(),
        teardown_complete: CancellationToken::new(),
        cleaning: AtomicBool::new(false),
        control: Mutex::new(None),
    });
    loop {
        let existing = {
            let mut sessions = state.sessions.lock().await;
            if let Some(existing) = sessions
                .values()
                .find(|session| session.downstream == downstream)
                .cloned()
            {
                existing
            } else {
                sessions.insert(id, slot.clone());
                break;
            }
        };
        if existing.cancel.is_cancelled() {
            select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "start session cancelled"));
                }
                _ = existing.teardown_complete.cancelled() => {}
            }
        } else {
            return Err(
                io::Error::new(io::ErrorKind::AlreadyExists, "session already exists")
                    .with_report_context_details(
                        "control.start_session",
                        [("downstream", downstream)],
                    ),
            );
        }
    }
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
    let mut guard = slot.control.lock().await;
    let ipsec_config = config.clone();
    // After downstream discovery, startup must produce an owned Session so applied state can be rolled back.
    let session = match Session::start(
        id,
        config,
        state.nat66.clone(),
        state.detached.clone(),
        cancel,
    )
    .await
    .with_report_context_details(
        "control.start_session",
        [("downstream", downstream.as_str())],
    ) {
        Ok(session) => session,
        Err(e) => {
            drop(guard);
            remove_session_slot(state, &slot).await;
            return Err(e);
        }
    };
    let (control, command_receiver) = session_control::channel();
    *guard = Some(control);
    drop(guard);
    if !sender.send_frame(ack_event_frame(id)) {
        *slot.control.lock().await = None;
        session.stop(false).await;
        remove_session_slot(state, &slot).await;
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "controller send failed",
        ));
    }
    state
        .update_ipsec_session(&slot, &ipsec_config, sender)
        .await;
    run_session(state, slot, session, command_receiver, cancel).await;
    Ok(())
}

async fn replace_session(
    state: &Arc<State>,
    session_id: u64,
    config: vpnhotspotd::shared::model::SessionConfig,
    sender: &ControllerSender,
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
    let ipsec_config = config.clone();
    let pending = {
        let guard = slot.control.lock().await;
        let control = guard.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "session not established")
                .with_report_context_details(
                    "control.replace_session",
                    [
                        ("session_id", session_id.to_string()),
                        ("downstream", config.downstream.clone()),
                    ],
                )
        })?;
        control.replace_config(config)?
    };
    pending
        .receive()
        .await
        .with_report_context("control.replace_session")?
        .with_report_context("control.replace_session")?;
    state
        .update_ipsec_session(&slot, &ipsec_config, sender)
        .await;
    Ok(())
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
    slot.teardown_complete.cancel();
}

async fn start_neighbour_monitor(
    id: u64,
    state: &State,
    sender: ControllerSender,
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
    if let Some(previous) = current.take() {
        previous.cancel.cancel();
    }
    *current = Some(MonitorState {
        id,
        cancel: cancel.clone(),
    });
    drop(current);
    let result = neighbour::run(id, sender, &cancel, &state.detached)
        .await
        .with_report_context("control.start_neighbour_monitor");
    let mut current = state.neighbour_monitor.lock().await;
    if current.as_ref().is_some_and(|current| current.id == id) {
        current.take();
    }
    result
}
