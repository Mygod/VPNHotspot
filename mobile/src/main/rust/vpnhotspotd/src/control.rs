use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream as StdUnixStream;

use libc::EINPROGRESS;
use socket2::{Domain, SockAddr, Socket, Type};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::session::Session;
use crate::socket::await_connect;
use vpnhotspotd::shared::protocol::{
    error_packet, ok_packet, parse_command, ports_packet, Command,
};

// Mirrors the app-side control frame cap, matching Android's documented Binder transaction buffer.
const MAX_CONTROL_PACKET_SIZE: usize = 1024 * 1024;

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let mut controller = connect_control_socket(&socket_name).await?;
    eprintln!("connected to {socket_name}");
    let mut sessions = HashMap::<String, Session>::new();
    loop {
        let packet = match recv_packet(&mut controller).await {
            Ok(packet) => packet,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                eprintln!("controller recv failed: {e}");
                break;
            }
        };
        let response = match handle_packet(&packet, &mut sessions).await {
            Ok(HandleResult::Reply(reply)) => Some(reply),
            Ok(HandleResult::Shutdown(reply)) => {
                if let Err(e) = send_packet(&mut controller, &reply).await {
                    eprintln!("controller send failed: {e}");
                }
                break;
            }
            Err(e) => Some(error_packet(e)),
        };
        if let Some(response) = response {
            if let Err(e) = send_packet(&mut controller, &response).await {
                eprintln!("controller send failed: {e}");
                break;
            }
        }
    }
    for (_, session) in sessions {
        session.stop(false).await;
    }
    Ok(())
}

enum HandleResult {
    Reply(Vec<u8>),
    Shutdown(Vec<u8>),
}

async fn handle_packet(
    packet: &[u8],
    sessions: &mut HashMap<String, Session>,
) -> io::Result<HandleResult> {
    match parse_command(packet)? {
        Command::StartSession(config) => {
            let downstream = config.downstream.clone();
            if sessions.contains_key(&downstream) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "session already exists",
                ));
            }
            let session = Session::start(config).await?;
            let reply = ports_packet(session.ports());
            sessions.insert(downstream, session);
            Ok(HandleResult::Reply(reply))
        }
        Command::ReplaceSession(config) => {
            let session = sessions
                .get_mut(&config.downstream)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "session not found"))?;
            session.replace_config(config).await;
            Ok(HandleResult::Reply(ok_packet()))
        }
        Command::RemoveSession {
            downstream,
            withdraw_cleanup,
        } => {
            if let Some(session) = sessions.remove(&downstream) {
                session.stop(withdraw_cleanup).await;
            }
            Ok(HandleResult::Reply(ok_packet()))
        }
        Command::Shutdown { withdraw_cleanup } => {
            for (_, session) in sessions.drain() {
                session.stop(withdraw_cleanup).await;
            }
            Ok(HandleResult::Shutdown(ok_packet()))
        }
    }
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

async fn recv_packet(socket: &mut UnixStream) -> io::Result<Vec<u8>> {
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

async fn send_packet(socket: &mut UnixStream, packet: &[u8]) -> io::Result<()> {
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
