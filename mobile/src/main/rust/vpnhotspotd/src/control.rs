use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;

use libc::EINPROGRESS;
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
use vpnhotspotd::shared::protocol::{
    ok_packet, parse_command, ports_packet, traffic_counter_lines_packet, Command,
    DaemonErrorReport, IoErrorReportExt, IoResultReportExt,
};
use vpnhotspotd::shared::transport::{error_frame, parse_client_frame, reply_frame, ClientFrame};

// Mirrors the app-side control frame cap, matching Android's documented Binder transaction buffer.
const MAX_CONTROL_PACKET_SIZE: usize = 1024 * 1024;

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let controller = connect_control_socket(&socket_name).await?;
    eprintln!("connected to {socket_name}");
    let (mut controller_read, controller_write) = controller.into_split();
    let (sender, writer) = spawn_writer(controller_write);
    report::init(sender.clone())?;
    let state = Arc::new(State {
        netlink: netlink::Runtime::new()?,
        sessions: Mutex::new(HashMap::new()),
        neighbour_monitor: Mutex::new(None),
    });
    let active_calls = Arc::new(Mutex::new(HashMap::new()));
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
        match parse_client_frame(&packet) {
            Ok(ClientFrame::Call { id, packet }) => {
                let call = Arc::new(CallState {
                    cancel: CancellationToken::new(),
                });
                {
                    let mut active = active_calls.lock().await;
                    if active.contains_key(&id) {
                        let report = DaemonErrorReport::from_message_with_details(
                            "control.call",
                            "call already active",
                            "AlreadyExists",
                            [("id", id.to_string())],
                        );
                        if sender.send(error_frame(id, report)).is_err() {
                            eprintln!("controller send failed");
                            break;
                        }
                        continue;
                    }
                    active.insert(id, call.clone());
                }
                tasks.spawn(handle_call(
                    id,
                    packet,
                    state.clone(),
                    sender.clone(),
                    active_calls.clone(),
                    call,
                ));
            }
            Ok(ClientFrame::Cancel { id }) => {
                if let Some(call) = active_calls.lock().await.get(&id) {
                    call.cancel.cancel();
                }
            }
            Err(e) => {
                report::io("control.parse_frame", e);
                break;
            }
        }
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
        eprintln!("controller writer task failed: {e}");
    }
    Ok(())
}

struct State {
    netlink: netlink::Runtime,
    sessions: Mutex<HashMap<String, Arc<Mutex<Option<Session>>>>>,
    neighbour_monitor: Mutex<Option<MonitorState>>,
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
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .await
            .drain()
            .map(|(_, session)| session)
            .collect();
        for session in sessions {
            if let Some(session) = session.lock().await.take() {
                session.stop(withdraw_cleanup).await;
            }
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
    packet: Vec<u8>,
    state: Arc<State>,
    sender: UnboundedSender<Vec<u8>>,
    active_calls: Arc<Mutex<HashMap<u64, Arc<CallState>>>>,
    call: Arc<CallState>,
) {
    match handle_command(id, &packet, state, &sender, call.cancel.clone()).await {
        Ok(CallOutput::Reply(packet)) => {
            send_terminal_frame(id, &active_calls, &call, &sender, reply_frame(id, packet)).await;
        }
        Ok(CallOutput::NoFrame) => {
            remove_call(id, &active_calls, &call).await;
        }
        Err(e) => {
            let report = DaemonErrorReport::from_io_error("control.handle_call", e);
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
    packet: &[u8],
    state: Arc<State>,
    sender: &UnboundedSender<Vec<u8>>,
    cancel: CancellationToken,
) -> io::Result<CallOutput> {
    match parse_command(packet).with_report_context("control.parse_command")? {
        Command::StartSession(config) => match start_session(&state, config, &cancel).await {
            Ok(reply) => Ok(CallOutput::Reply(reply)),
            Err(e) if cancel.is_cancelled() && e.kind() == io::ErrorKind::Interrupted => {
                Ok(CallOutput::NoFrame)
            }
            Err(e) => Err(e),
        },
        Command::ReplaceSession(config) => {
            replace_session(&state, config).await?;
            Ok(CallOutput::Reply(ok_packet()))
        }
        Command::RemoveSession {
            downstream,
            withdraw_cleanup,
        } => {
            remove_session(&state, downstream, withdraw_cleanup).await;
            Ok(CallOutput::Reply(ok_packet()))
        }
        Command::ReadTrafficCounters => {
            let lines = crate::traffic::read_counter_lines()
                .await
                .with_report_context("control.read_traffic_counters")?;
            Ok(CallOutput::Reply(traffic_counter_lines_packet(&lines)))
        }
        Command::StartNeighbourMonitor => {
            start_neighbour_monitor(id, &state, sender.clone(), cancel).await?;
            Ok(CallOutput::NoFrame)
        }
        Command::ReplaceStaticAddresses(command) => {
            let handle = state.netlink.handle();
            routing::replace_static_addresses(&handle, &command)
                .await
                .with_report_context_details(
                    "control.replace_static_addresses",
                    [
                        ("interface", command.interface.clone()),
                        ("count", command.addresses.len().to_string()),
                    ],
                )?;
            Ok(CallOutput::Reply(ok_packet()))
        }
        Command::DeleteStaticAddresses { interface } => {
            let handle = state.netlink.handle();
            routing::delete_static_addresses(&handle, &interface)
                .await
                .with_report_context_details(
                    "control.delete_static_addresses",
                    [("interface", interface)],
                )?;
            Ok(CallOutput::Reply(ok_packet()))
        }
        Command::CleanRouting(command) => {
            let handle = state.netlink.handle();
            routing::clean(&handle, &command)
                .await
                .with_report_context("control.clean_routing")?;
            Ok(CallOutput::Reply(ok_packet()))
        }
    }
}

async fn start_session(
    state: &State,
    config: vpnhotspotd::shared::model::SessionConfig,
    cancel: &CancellationToken,
) -> io::Result<Vec<u8>> {
    let downstream = config.downstream.clone();
    let slot = Arc::new(Mutex::new(None));
    {
        let mut sessions = state.sessions.lock().await;
        if sessions.contains_key(&downstream) {
            return Err(
                io::Error::new(io::ErrorKind::AlreadyExists, "session already exists")
                    .with_report_context_details(
                        "control.start_session",
                        [("downstream", downstream)],
                    ),
            );
        }
        sessions.insert(downstream.clone(), slot.clone());
    }
    let mut guard = slot.lock().await;
    match Session::start(config, &state.netlink, cancel)
        .await
        .with_report_context_details(
            "control.start_session",
            [("downstream", downstream.as_str())],
        ) {
        Ok(session) => {
            let reply = ports_packet(session.ports());
            *guard = Some(session);
            Ok(reply)
        }
        Err(e) => {
            drop(guard);
            remove_session_slot(state, &downstream, &slot).await;
            Err(e)
        }
    }
}

async fn replace_session(
    state: &State,
    config: vpnhotspotd::shared::model::SessionConfig,
) -> io::Result<()> {
    let slot = state
        .sessions
        .lock()
        .await
        .get(&config.downstream)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "session not found")
                .with_report_context_details(
                    "control.replace_session",
                    [("downstream", config.downstream.as_str())],
                )
        })?;
    let mut guard = slot.lock().await;
    let session = guard.as_mut().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "session not ready").with_report_context_details(
            "control.replace_session",
            [("downstream", config.downstream.as_str())],
        )
    })?;
    session
        .replace_config(config)
        .await
        .with_report_context("control.replace_session")
}

async fn remove_session(state: &State, downstream: String, withdraw_cleanup: bool) {
    let slot = state.sessions.lock().await.get(&downstream).cloned();
    if let Some(slot) = slot {
        let mut guard = slot.lock().await;
        if let Some(session) = guard.take() {
            session.stop(withdraw_cleanup).await;
        }
        drop(guard);
        remove_session_slot(state, &downstream, &slot).await;
    }
}

async fn remove_session_slot(state: &State, downstream: &str, slot: &Arc<Mutex<Option<Session>>>) {
    let mut sessions = state.sessions.lock().await;
    if sessions
        .get(downstream)
        .is_some_and(|current| Arc::ptr_eq(current, slot))
    {
        sessions.remove(downstream);
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
        eprintln!("controller send failed");
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
                eprintln!("controller send failed: {e}");
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
