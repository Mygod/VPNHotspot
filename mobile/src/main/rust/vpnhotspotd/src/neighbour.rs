use std::ffi::CStr;
use std::io;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

use linux_raw_sys::netlink::{
    ndmsg, nlmsgerr, nlmsghdr, rtattr, sockaddr_nl, NDA_DST, NDA_LLADDR, NETLINK_ROUTE, NLMSG_DONE,
    NLMSG_ERROR, NLM_F_DUMP, NLM_F_REQUEST, NUD_DELAY, NUD_FAILED, NUD_INCOMPLETE, NUD_NOARP,
    NUD_NONE, NUD_PERMANENT, NUD_PROBE, NUD_REACHABLE, NUD_STALE, RTMGRP_NEIGH, RTM_DELNEIGH,
    RTM_GETNEIGH, RTM_NEWNEIGH,
};
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc::UnboundedSender;
use tokio::{select, spawn, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use vpnhotspotd::shared::protocol::{neighbours_frame, Neighbour, NeighbourState};

const BUFFER_SIZE: usize = 64 * 1024;
const DUMP_SEQUENCE: u32 = 1;

pub(crate) async fn dump() -> io::Result<Vec<Neighbour>> {
    let socket = NetlinkSocket::new(0)?;
    socket.send_dump_request(DUMP_SEQUENCE).await?;
    let mut neighbours = Vec::new();
    socket.read_dump(DUMP_SEQUENCE, &mut neighbours).await?;
    Ok(neighbours)
}

pub(crate) struct Monitor {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

impl Monitor {
    pub(crate) fn spawn(sender: UnboundedSender<Vec<u8>>) -> io::Result<Self> {
        let socket = NetlinkSocket::new(RTMGRP_NEIGH)?;
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        Ok(Self {
            stop,
            task: spawn(async move {
                let mut buffer = vec![0u8; BUFFER_SIZE];
                loop {
                    select! {
                        _ = task_stop.cancelled() => break,
                        result = socket.read_events(&mut buffer) => match result {
                            Ok(neighbours) => {
                                if !neighbours.is_empty() &&
                                        sender.send(neighbours_frame(false, &neighbours)).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                eprintln!("neighbour monitor failed: {e}");
                                break;
                            }
                        }
                    }
                }
            }),
        })
    }

    pub(crate) async fn stop(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

struct NetlinkSocket {
    fd: AsyncFd<OwnedFd>,
}

impl NetlinkSocket {
    fn new(groups: u32) -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                NETLINK_ROUTE as libc::c_int,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = sockaddr_nl {
            nl_family: libc::AF_NETLINK as _,
            nl_pad: 0,
            nl_pid: 0,
            nl_groups: groups,
        };
        if unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &address as *const _ as *const libc::sockaddr,
                size_of::<sockaddr_nl>() as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: AsyncFd::new(fd)?,
        })
    }

    async fn send_dump_request(&self, sequence: u32) -> io::Result<()> {
        let mut packet = Vec::with_capacity(size_of::<nlmsghdr>() + size_of::<ndmsg>());
        push_struct(
            &mut packet,
            &nlmsghdr {
                nlmsg_len: (size_of::<nlmsghdr>() + size_of::<ndmsg>()) as u32,
                nlmsg_type: RTM_GETNEIGH as u16,
                nlmsg_flags: (NLM_F_REQUEST | NLM_F_DUMP) as u16,
                nlmsg_seq: sequence,
                nlmsg_pid: 0,
            },
        );
        push_struct(
            &mut packet,
            &ndmsg {
                ndm_family: libc::AF_UNSPEC as u8,
                ndm_pad1: 0,
                ndm_pad2: 0,
                ndm_ifindex: 0,
                ndm_state: 0,
                ndm_flags: 0,
                ndm_type: 0,
            },
        );
        loop {
            let result = unsafe {
                libc::send(
                    self.fd.get_ref().as_raw_fd(),
                    packet.as_ptr() as *const libc::c_void,
                    packet.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if result == packet.len() as isize {
                return Ok(());
            }
            if result >= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "short netlink write",
                ));
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
            let mut ready = self.fd.writable().await?;
            ready.clear_ready();
        }
    }

    async fn read_dump(&self, sequence: u32, neighbours: &mut Vec<Neighbour>) -> io::Result<()> {
        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            let mut ready = self.fd.readable().await?;
            loop {
                match self.recv(&mut buffer)? {
                    RecvResult::Messages(bytes) => {
                        if parse_messages(bytes, Some(sequence), neighbours)? {
                            return Ok(());
                        }
                    }
                    RecvResult::WouldBlock => {
                        ready.clear_ready();
                        break;
                    }
                }
            }
        }
    }

    async fn read_events(&self, buffer: &mut [u8]) -> io::Result<Vec<Neighbour>> {
        let mut ready = self.fd.readable().await?;
        let mut neighbours = Vec::new();
        loop {
            match self.recv(buffer)? {
                RecvResult::Messages(bytes) => {
                    parse_messages(bytes, None, &mut neighbours)?;
                }
                RecvResult::WouldBlock => {
                    ready.clear_ready();
                    return Ok(neighbours);
                }
            }
        }
    }

    fn recv<'a>(&self, buffer: &'a mut [u8]) -> io::Result<RecvResult<'a>> {
        loop {
            let result = unsafe {
                libc::recv(
                    self.fd.get_ref().as_raw_fd(),
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if result > 0 {
                return Ok(RecvResult::Messages(&buffer[..result as usize]));
            }
            if result == 0 {
                return Ok(RecvResult::WouldBlock);
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(RecvResult::WouldBlock);
            }
            return Err(error);
        }
    }
}

enum RecvResult<'a> {
    Messages(&'a [u8]),
    WouldBlock,
}

fn parse_messages(
    bytes: &[u8],
    sequence: Option<u32>,
    neighbours: &mut Vec<Neighbour>,
) -> io::Result<bool> {
    let mut offset = 0;
    while offset + size_of::<nlmsghdr>() <= bytes.len() {
        let header = read_struct::<nlmsghdr>(&bytes[offset..]);
        let length = header.nlmsg_len as usize;
        if length < size_of::<nlmsghdr>() || offset + length > bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid netlink message length",
            ));
        }
        let payload = &bytes[offset + size_of::<nlmsghdr>()..offset + length];
        if sequence.is_none_or(|value| value == header.nlmsg_seq) {
            match header.nlmsg_type as u32 {
                NLMSG_DONE => return Ok(true),
                NLMSG_ERROR => parse_error(payload)?,
                message if message == RTM_NEWNEIGH as u32 || message == RTM_DELNEIGH as u32 => {
                    if let Some(neighbour) = parse_neighbour(message, payload)? {
                        neighbours.push(neighbour);
                    }
                }
                _ => {}
            }
        }
        offset += align(length);
    }
    Ok(false)
}

fn parse_error(payload: &[u8]) -> io::Result<()> {
    if payload.len() < size_of::<nlmsgerr>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short netlink error",
        ));
    }
    let error = read_struct::<nlmsgerr>(payload).error;
    if error == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(-error))
    }
}

fn parse_neighbour(message: u32, payload: &[u8]) -> io::Result<Option<Neighbour>> {
    if payload.len() < size_of::<ndmsg>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "short neighbour message",
        ));
    }
    let header = read_struct::<ndmsg>(payload);
    if header.ndm_state & NUD_NOARP as u16 != 0 || header.ndm_ifindex <= 0 {
        return Ok(None);
    }
    let mut address = None;
    let mut lladdr = Vec::new();
    let mut offset = size_of::<ndmsg>();
    while offset + size_of::<rtattr>() <= payload.len() {
        let attr = read_struct::<rtattr>(&payload[offset..]);
        let length = attr.rta_len as usize;
        if length < size_of::<rtattr>() || offset + length > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid neighbour attribute length",
            ));
        }
        let value = &payload[offset + size_of::<rtattr>()..offset + length];
        match attr.rta_type as u32 {
            attr_type if attr_type == NDA_DST as u32 => {
                address = match header.ndm_family as i32 {
                    libc::AF_INET if value.len() == 4 => Some(IpAddr::V4(Ipv4Addr::new(
                        value[0], value[1], value[2], value[3],
                    ))),
                    libc::AF_INET6 if value.len() == 16 => {
                        let mut bytes = [0u8; 16];
                        bytes.copy_from_slice(value);
                        Some(IpAddr::V6(Ipv6Addr::from(bytes)))
                    }
                    _ => None,
                };
            }
            attr_type if attr_type == NDA_LLADDR as u32 => lladdr.extend_from_slice(value),
            _ => {}
        }
        offset += align(length);
    }
    let Some(address) = address else {
        return Ok(None);
    };
    let state = if message == RTM_DELNEIGH as u32 {
        NeighbourState::Deleting
    } else {
        match header.ndm_state as u32 {
            state
                if state & (NUD_REACHABLE | NUD_DELAY | NUD_STALE | NUD_PROBE | NUD_PERMANENT)
                    != 0 =>
            {
                NeighbourState::Valid
            }
            state if state & NUD_FAILED != 0 => NeighbourState::Failed,
            state if state & NUD_INCOMPLETE != 0 || state == NUD_NONE => NeighbourState::Incomplete,
            _ => NeighbourState::Unset,
        }
    };
    Ok(Some(Neighbour {
        address,
        interface: interface_name(header.ndm_ifindex),
        lladdr,
        state,
    }))
}

fn interface_name(index: i32) -> String {
    let mut buffer = [0 as libc::c_char; libc::IF_NAMESIZE];
    let name = unsafe { libc::if_indextoname(index as u32, buffer.as_mut_ptr()) };
    if name.is_null() {
        format!("if{index}")
    } else {
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    }
}

fn align(length: usize) -> usize {
    (length + 3) & !3
}

fn push_struct<T>(packet: &mut Vec<u8>, value: &T) {
    let bytes =
        unsafe { std::slice::from_raw_parts(value as *const T as *const u8, size_of::<T>()) };
    packet.extend_from_slice(bytes);
}

fn read_struct<T: Copy>(bytes: &[u8]) -> T {
    unsafe { ptr::read_unaligned(bytes.as_ptr() as *const T) }
}
