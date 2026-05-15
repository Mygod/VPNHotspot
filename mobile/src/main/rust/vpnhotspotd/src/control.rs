use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use libc::EINPROGRESS;
use prost::Message;
use socket2::{Domain, SockAddr, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::select;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::neighbour::Monitor;
use crate::session::Session;
use crate::socket::await_connect;
use crate::{netlink, report, routing};
use vpnhotspotd::shared::proto::daemon;
use vpnhotspotd::shared::protocol::{
    ack_event_frame, ack_reply_frame, complete_frame, daemon_error_report_with_details,
    daemon_io_error_report, error_frame, read_session_config, traffic_counter_lines_frame,
    IoErrorReportExt, IoResultReportExt,
};

// Mirrors the app-side control frame cap, matching Android's documented Binder transaction buffer.
const MAX_CONTROL_PACKET_SIZE: usize = 1024 * 1024;

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let controller = connect_control_socket(&socket_name).await?;
    report::stderr!("connected to {socket_name}");
    let (mut controller_read, controller_write) = controller.into_split();
    let (sender, writer) = spawn_writer(controller_write);
    report::init(sender.clone())?;
    let state = Arc::new(State {
        netlink: netlink::Runtime::new()?,
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
    netlink: netlink::Runtime,
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
    async fn stop(&self, withdraw_cleanup: bool) {
        if let Some(monitor) = self.neighbour_monitor.lock().await.take() {
            monitor.monitor.stop().await;
        }
        let sessions = self.drain_sessions().await;
        stop_sessions(&sessions, withdraw_cleanup).await;
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
}

async fn stop_sessions(sessions: &[Arc<SessionState>], withdraw_cleanup: bool) {
    for session in sessions {
        if let Some(session) = session.session.lock().await.take() {
            session.stop(withdraw_cleanup).await;
        }
    }
}

struct CallState {
    cancel: CancellationToken,
}

enum CallOutput {
    Reply(Vec<u8>),
    NoFrame,
}

async fn handle_call(
    id: u64,
    command: daemon::client_envelope::Command,
    state: Arc<State>,
    sender: UnboundedSender<Vec<u8>>,
    active_calls: Arc<Mutex<HashMap<u64, Arc<CallState>>>>,
    call: Arc<CallState>,
) {
    match handle_command(
        id,
        command,
        state,
        &sender,
        active_calls.clone(),
        call.cancel.clone(),
    )
    .await
    {
        Ok(CallOutput::Reply(packet)) => {
            send_terminal_frame(id, &active_calls, &call, &sender, packet).await;
        }
        Ok(CallOutput::NoFrame) => {
            remove_call(id, &active_calls, &call).await;
        }
        Err(e) => {
            let report = daemon_io_error_report("control.handle_call", e);
            if call.cancel.is_cancelled() {
                if remove_call(id, &active_calls, &call).await {
                    report::report_for(Some(id), report);
                }
            } else {
                send_terminal_frame(id, &active_calls, &call, &sender, error_frame(id, report))
                    .await;
            }
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
            )
            .await?;
            Ok(CallOutput::Reply(ack_reply_frame(id)))
        }
        daemon::client_envelope::Command::ReadTrafficCounters(_) => {
            let lines = crate::traffic::read_counter_lines()
                .await
                .with_report_context("control.read_traffic_counters")?;
            Ok(CallOutput::Reply(traffic_counter_lines_frame(id, &lines)))
        }
        daemon::client_envelope::Command::StartNeighbourMonitor(_) => {
            start_neighbour_monitor(id, &state, sender.clone(), cancel).await?;
            Ok(CallOutput::NoFrame)
        }
        daemon::client_envelope::Command::ReplaceStaticAddresses(command) => {
            let handle = state.netlink.handle();
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
            state.stop_ipv6_nat_firewall_base().await;
            for id in complete_ids {
                send_complete(id, sender);
            }
            let handle = state.netlink.handle();
            routing::clean(&handle, &command)
                .await
                .with_report_context("control.clean_routing")?;
            Ok(CallOutput::Reply(ack_reply_frame(id)))
        }
    }
}

async fn start_session(
    id: u64,
    state: &State,
    config: vpnhotspotd::shared::model::SessionConfig,
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
    if config.ipv6_nat.is_some() {
        if let Err(e) = state
            .ensure_ipv6_nat_firewall_base()
            .await
            .with_report_context_details(
                "control.start_session.ipv6_nat_firewall_base",
                [("downstream", downstream.as_str())],
            )
        {
            remove_session_slot(state, &slot).await;
            return Err(e);
        }
    }
    let mut guard = slot.session.lock().await;
    match Session::start(id, config, &state.netlink, cancel)
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
    cancel.cancelled().await;
    if !slot.cleaning.load(Ordering::Acquire) {
        remove_session(state, &slot, false).await;
    }
    Ok(())
}

async fn replace_session(
    state: &State,
    session_id: u64,
    config: vpnhotspotd::shared::model::SessionConfig,
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
        .with_report_context("control.replace_session")
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
    *current = Some(MonitorState {
        id,
        cancel: cancel.clone(),
        monitor: Monitor::spawn(id, &state.netlink, sender)
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

async fn send_terminal_frame(
    id: u64,
    active_calls: &Mutex<HashMap<u64, Arc<CallState>>>,
    call: &Arc<CallState>,
    sender: &UnboundedSender<Vec<u8>>,
    frame: Vec<u8>,
) {
    let cancelled = call.cancel.is_cancelled();
    if remove_call(id, active_calls, call).await && !cancelled && sender.send(frame).is_err() {
        report::stderr!("controller send failed");
    }
}

async fn detach_call(id: u64, active_calls: &Mutex<HashMap<u64, Arc<CallState>>>) -> bool {
    let call = active_calls.lock().await.remove(&id);
    if let Some(call) = call {
        call.cancel.cancel();
        true
    } else {
        false
    }
}

fn send_complete(id: u64, sender: &UnboundedSender<Vec<u8>>) {
    if sender.send(complete_frame(id)).is_err() {
        report::stderr!("controller send failed");
    }
}

async fn remove_call(
    id: u64,
    active_calls: &Mutex<HashMap<u64, Arc<CallState>>>,
    call: &Arc<CallState>,
) -> bool {
    let mut active = active_calls.lock().await;
    if active
        .get(&id)
        .is_some_and(|current| Arc::ptr_eq(current, call))
    {
        active.remove(&id);
        true
    } else {
        false
    }
}

fn spawn_writer<W>(mut writer: W) -> (UnboundedSender<Vec<u8>>, JoinHandle<()>)
where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (sender, mut receiver) = unbounded_channel::<Vec<u8>>();
    let task = tokio::spawn(async move {
        while let Some(packet) = receiver.recv().await {
            if let Err(e) = send_packet(&mut writer, &packet).await {
                report::stderr!("controller send failed: {e}");
                break;
            }
        }
    });
    (sender, task)
}

async fn connect_control_socket(socket_name: &str) -> io::Result<UnixStream> {
    let address = SockAddr::unix(OsStr::from_bytes(format!("\0{socket_name}").as_bytes()))?;
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_nonblocking(true)?;
    match socket.connect(&address) {
        Ok(()) => {}
        Err(error) => {
            let raw_os_error = error.raw_os_error();
            if error.kind() == io::ErrorKind::WouldBlock || raw_os_error == Some(EINPROGRESS) {
                await_connect(&socket).await?;
            } else {
                return Err(error);
            }
        }
    }
    let stream: StdUnixStream = socket.into();
    UnixStream::from_std(stream)
}

async fn recv_packet<R>(socket: &mut R) -> io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 4];
    socket.read_exact(&mut header).await?;
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_CONTROL_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid control frame length {length}"),
        ));
    }
    let mut buffer = vec![0u8; length];
    socket.read_exact(&mut buffer).await?;
    Ok(buffer)
}

async fn send_packet<W>(socket: &mut W, packet: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if packet.is_empty() || packet.len() > MAX_CONTROL_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid control frame length {}", packet.len()),
        ));
    }
    socket
        .write_all(&(packet.len() as u32).to_be_bytes())
        .await?;
    socket.write_all(packet).await?;
    socket.flush().await
}
