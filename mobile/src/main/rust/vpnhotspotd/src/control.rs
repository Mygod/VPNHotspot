use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream as StdUnixStream;

use libc::EINPROGRESS;
use socket2::{Domain, SockAddr, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::task::JoinHandle;

use crate::neighbour::Monitor;
use crate::session::Session;
use crate::socket::await_connect;
use crate::{netlink, report, routing};
use vpnhotspotd::shared::protocol::{
    error_packet, neighbours_frame, neighbours_packet, ok_packet, parse_command, ports_packet,
    reply_frame, should_suppress_static_address_error, traffic_counter_lines_packet, Command,
    DaemonErrorReport, IoErrorReportExt, IoResultReportExt, IpAddressCommand,
};

// Mirrors the app-side control frame cap, matching Android's documented Binder transaction buffer.
const MAX_CONTROL_PACKET_SIZE: usize = 1024 * 1024;

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let controller = connect_control_socket(&socket_name).await?;
    eprintln!("connected to {socket_name}");
    let (mut controller_read, controller_write) = controller.into_split();
    let (sender, writer) = spawn_writer(controller_write);
    report::init(sender.clone())?;
    let mut state = State {
        netlink: netlink::Runtime::new()?,
        sessions: HashMap::new(),
        neighbour_monitor: None,
    };
    loop {
        let packet = match recv_packet(&mut controller_read).await {
            Ok(packet) => packet,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                report::io("control.recv_packet", e);
                break;
            }
        };
        let response = match handle_packet(&packet, &mut state, &sender).await {
            Ok(HandleResult::Reply(reply)) => Some(reply),
            Ok(HandleResult::Shutdown(reply)) => {
                if sender.send(reply_frame(reply)).is_err() {
                    eprintln!("controller send failed");
                }
                break;
            }
            Err(e) => Some(error_packet(DaemonErrorReport::from_io_error(
                "control.handle_packet",
                e,
            ))),
        };
        if let Some(response) = response {
            if sender.send(reply_frame(response)).is_err() {
                eprintln!("controller send failed");
                break;
            }
        }
    }
    drop(sender);
    if let Some(monitor) = state.neighbour_monitor.take() {
        monitor.stop().await;
    }
    for (_, session) in state.sessions {
        session.stop(false).await;
    }
    if let Err(e) = writer.await {
        eprintln!("controller writer task failed: {e}");
    }
    Ok(())
}

struct State {
    netlink: netlink::Runtime,
    sessions: HashMap<String, Session>,
    neighbour_monitor: Option<Monitor>,
}

enum HandleResult {
    Reply(Vec<u8>),
    Shutdown(Vec<u8>),
}

async fn handle_packet(
    packet: &[u8],
    state: &mut State,
    sender: &UnboundedSender<Vec<u8>>,
) -> io::Result<HandleResult> {
    match parse_command(packet).with_report_context("control.parse_command")? {
        Command::StartSession(config) => {
            let downstream = config.downstream.clone();
            if state.sessions.contains_key(&downstream) {
                return Err(
                    io::Error::new(io::ErrorKind::AlreadyExists, "session already exists")
                        .with_report_context_details(
                            "control.start_session",
                            [("downstream", downstream)],
                        ),
                );
            }
            let session = Session::start(config, &state.netlink)
                .await
                .with_report_context_details(
                    "control.start_session",
                    [("downstream", downstream.as_str())],
                )?;
            let reply = ports_packet(session.ports());
            state.sessions.insert(downstream, session);
            Ok(HandleResult::Reply(reply))
        }
        Command::ReplaceSession(config) => {
            let session = state.sessions.get_mut(&config.downstream).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "session not found")
                    .with_report_context_details(
                        "control.replace_session",
                        [("downstream", config.downstream.as_str())],
                    )
            })?;
            session
                .replace_config(config)
                .await
                .with_report_context("control.replace_session")?;
            Ok(HandleResult::Reply(ok_packet()))
        }
        Command::RemoveSession {
            downstream,
            withdraw_cleanup,
        } => {
            if let Some(session) = state.sessions.remove(&downstream) {
                session.stop(withdraw_cleanup).await;
            }
            Ok(HandleResult::Reply(ok_packet()))
        }
        Command::Shutdown { withdraw_cleanup } => {
            if let Some(monitor) = state.neighbour_monitor.take() {
                monitor.stop().await;
            }
            for (_, session) in state.sessions.drain() {
                session.stop(withdraw_cleanup).await;
            }
            Ok(HandleResult::Shutdown(ok_packet()))
        }
        Command::ReadTrafficCounters => {
            let lines = crate::traffic::read_counter_lines()
                .await
                .with_report_context("control.read_traffic_counters")?;
            Ok(HandleResult::Reply(traffic_counter_lines_packet(&lines)))
        }
        Command::StartNeighbourMonitor => {
            if state.neighbour_monitor.is_none() {
                let monitor = Monitor::spawn(&state.netlink, sender.clone())?;
                let handle = state.netlink.handle();
                let neighbours = crate::neighbour::dump(&handle)
                    .await
                    .with_report_context("control.start_neighbour_monitor.dump")?;
                if sender.send(neighbours_frame(true, &neighbours)).is_err() {
                    monitor.stop().await;
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "controller disconnected",
                    )
                    .with_report_context("control.start_neighbour_monitor.send"));
                }
                state.neighbour_monitor = Some(monitor);
            }
            Ok(HandleResult::Reply(ok_packet()))
        }
        Command::StopNeighbourMonitor => {
            if let Some(monitor) = state.neighbour_monitor.take() {
                monitor.stop().await;
            }
            Ok(HandleResult::Reply(ok_packet()))
        }
        Command::DumpNeighbours => {
            let handle = state.netlink.handle();
            Ok(HandleResult::Reply(neighbours_packet(
                &crate::neighbour::dump(&handle)
                    .await
                    .with_report_context("control.dump_neighbours")?,
            )))
        }
        Command::StaticAddress(command) => {
            let handle = state.netlink.handle();
            apply_static_address(&handle, &command)
                .await
                .with_report_context_details(
                    "control.static_address",
                    [
                        ("operation", format!("{:?}", command.operation)),
                        ("address", command.address.to_string()),
                        ("interface", command.interface.clone()),
                    ],
                )?;
            Ok(HandleResult::Reply(ok_packet()))
        }
        Command::CleanRouting(command) => {
            let handle = state.netlink.handle();
            routing::clean(&handle, &command)
                .await
                .with_report_context("control.clean_routing")?;
            Ok(HandleResult::Reply(ok_packet()))
        }
    }
}

async fn apply_static_address(
    handle: &netlink::Handle,
    command: &IpAddressCommand,
) -> io::Result<()> {
    match routing::apply_static_address(handle, command).await {
        Ok(()) => Ok(()),
        Err(e) if should_suppress_static_address_error(&e, command.operation) => Ok(()),
        Err(e) => Err(e),
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
