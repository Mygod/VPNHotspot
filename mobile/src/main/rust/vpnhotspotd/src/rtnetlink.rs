use std::ffi::CString;
use std::io;
use std::mem::size_of;
use std::net::IpAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

use linux_raw_sys::netlink::{
    fib_rule_hdr, ifaddrmsg, nlmsgerr, nlmsghdr,
    rt_class_t::RT_TABLE_UNSPEC,
    rt_scope_t::{RT_SCOPE_HOST, RT_SCOPE_LINK},
    rtattr,
    rtattr_type_t::{RTA_DST, RTA_OIF, RTA_TABLE},
    rtmsg, sockaddr_nl, FRA_FWMARK, FRA_FWMASK, FRA_IIFNAME, FRA_PRIORITY, FRA_TABLE,
    FR_ACT_TO_TBL, FR_ACT_UNREACHABLE, IFA_ADDRESS, IFA_LOCAL, NETLINK_ROUTE, NLMSG_DONE,
    NLMSG_ERROR, NLM_F_ACK, NLM_F_CREATE, NLM_F_DUMP, NLM_F_EXCL, NLM_F_REPLACE, NLM_F_REQUEST,
    RTM_DELADDR, RTM_DELROUTE, RTM_DELRULE, RTM_GETROUTE, RTM_NEWADDR, RTM_NEWROUTE, RTM_NEWRULE,
    RTN_LOCAL, RTN_UNICAST, RTPROT_STATIC,
};
use tokio::io::unix::AsyncFd;

use vpnhotspotd::shared::model::{DAEMON_TABLE, LOCAL_NETWORK_TABLE};
use vpnhotspotd::shared::protocol::{
    CleanIpCommand, IpAddressCommand, IpFamily, IpOperation, IpRouteCommand, IpRuleCommand,
    RouteType, RuleAction,
};

const BUFFER_SIZE: usize = 64 * 1024;

pub(crate) async fn apply_address(command: &IpAddressCommand) -> io::Result<()> {
    let mut payload = Vec::new();
    push_struct(
        &mut payload,
        &ifaddrmsg {
            ifa_family: address_family(&command.address),
            ifa_prefixlen: command.prefix_len,
            ifa_flags: 0,
            ifa_scope: 0,
            ifa_index: interface_index(&command.interface)?,
        },
    );
    let address = address_bytes(&command.address);
    push_attr(&mut payload, IFA_LOCAL as u16, &address);
    push_attr(&mut payload, IFA_ADDRESS as u16, &address);
    let (message, flags) = match command.operation {
        IpOperation::Replace => (
            RTM_NEWADDR as u16,
            (NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE) as u16,
        ),
        IpOperation::Delete => (RTM_DELADDR as u16, (NLM_F_REQUEST | NLM_F_ACK) as u16),
    };
    NetlinkSocket::new()?
        .request(message, flags, &payload)
        .await
}

pub(crate) async fn apply_route(command: &IpRouteCommand) -> io::Result<()> {
    let mut payload = route_payload(
        command.route_type,
        &command.destination,
        command.prefix_len,
        &command.interface,
        command.table,
    )?;
    let (message, flags) = match command.operation {
        IpOperation::Replace => (
            RTM_NEWROUTE as u16,
            (NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_REPLACE) as u16,
        ),
        IpOperation::Delete => (RTM_DELROUTE as u16, (NLM_F_REQUEST | NLM_F_ACK) as u16),
    };
    NetlinkSocket::new()?
        .request(message, flags, &payload)
        .await?;
    payload.clear();
    Ok(())
}

pub(crate) async fn apply_rule(command: &IpRuleCommand) -> io::Result<()> {
    let mut payload = Vec::new();
    let action = match command.action {
        RuleAction::Lookup => FR_ACT_TO_TBL as u8,
        RuleAction::Unreachable => FR_ACT_UNREACHABLE as u8,
        RuleAction::Any => 0,
    };
    push_struct(
        &mut payload,
        &fib_rule_hdr {
            family: family_value(command.family),
            dst_len: 0,
            src_len: 0,
            tos: 0,
            table: if command.table < 256 {
                command.table as u8
            } else {
                RT_TABLE_UNSPEC as u8
            },
            res1: 0,
            res2: 0,
            action,
            flags: 0,
        },
    );
    if !command.iif.is_empty() {
        push_attr_c_string(&mut payload, FRA_IIFNAME as u16, &command.iif)?;
    }
    push_attr_u32(&mut payload, FRA_PRIORITY as u16, command.priority);
    if command.action == RuleAction::Lookup {
        push_attr_u32(&mut payload, FRA_TABLE as u16, command.table);
    }
    if let Some((mark, mask)) = command.fwmark {
        push_attr_u32(&mut payload, FRA_FWMARK as u16, mark);
        push_attr_u32(&mut payload, FRA_FWMASK as u16, mask);
    }
    let (message, flags) = match command.operation {
        IpOperation::Replace => (
            RTM_NEWRULE as u16,
            (NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL) as u16,
        ),
        IpOperation::Delete => (RTM_DELRULE as u16, (NLM_F_REQUEST | NLM_F_ACK) as u16),
    };
    NetlinkSocket::new()?
        .request(message, flags, &payload)
        .await
}

pub(crate) async fn clean_ip(command: &CleanIpCommand) -> io::Result<()> {
    let mut socket = NetlinkSocket::new()?;
    socket
        .flush_routes(libc::AF_INET6 as u8, DAEMON_TABLE)
        .await?;
    for cleanup in &command.ipv6 {
        let _ = apply_address(&IpAddressCommand {
            operation: IpOperation::Delete,
            address: IpAddr::V6(cleanup.gateway),
            prefix_len: cleanup.prefix.prefix_len,
            interface: cleanup.interface.clone(),
        })
        .await;
        let _ = apply_route(&IpRouteCommand {
            operation: IpOperation::Delete,
            route_type: RouteType::Unicast,
            destination: IpAddr::V6(cleanup.prefix_address()),
            prefix_len: cleanup.prefix.prefix_len,
            interface: cleanup.interface.clone(),
            table: LOCAL_NETWORK_TABLE,
        })
        .await;
    }
    Ok(())
}

trait Ipv6CleanupExt {
    fn prefix_address(&self) -> std::net::Ipv6Addr;
}

impl Ipv6CleanupExt for vpnhotspotd::shared::protocol::Ipv6Cleanup {
    fn prefix_address(&self) -> std::net::Ipv6Addr {
        std::net::Ipv6Addr::from(self.prefix.prefix.to_be_bytes())
    }
}

fn route_payload(
    route_type: RouteType,
    destination: &IpAddr,
    prefix_len: u8,
    interface: &str,
    table: u32,
) -> io::Result<Vec<u8>> {
    let mut payload = Vec::new();
    let route_type = match route_type {
        RouteType::Unicast => RTN_UNICAST as u8,
        RouteType::Local => RTN_LOCAL as u8,
    };
    push_struct(
        &mut payload,
        &rtmsg {
            rtm_family: address_family(destination),
            rtm_dst_len: prefix_len,
            rtm_src_len: 0,
            rtm_tos: 0,
            rtm_table: if table < 256 {
                table as u8
            } else {
                RT_TABLE_UNSPEC as u8
            },
            rtm_protocol: RTPROT_STATIC as u8,
            rtm_scope: if route_type == RTN_LOCAL as u8 {
                RT_SCOPE_HOST as u8
            } else {
                RT_SCOPE_LINK as u8
            },
            rtm_type: route_type,
            rtm_flags: 0,
        },
    );
    if prefix_len > 0 {
        let destination = address_bytes(destination);
        push_attr(&mut payload, RTA_DST as u16, &destination);
    }
    push_attr_u32(&mut payload, RTA_OIF as u16, interface_index(interface)?);
    push_attr_u32(&mut payload, RTA_TABLE as u16, table);
    Ok(payload)
}

fn address_family(address: &IpAddr) -> u8 {
    match address {
        IpAddr::V4(_) => libc::AF_INET as u8,
        IpAddr::V6(_) => libc::AF_INET6 as u8,
    }
}

fn family_value(family: IpFamily) -> u8 {
    match family {
        IpFamily::Ipv4 => libc::AF_INET as u8,
        IpFamily::Ipv6 => libc::AF_INET6 as u8,
    }
}

fn address_bytes(address: &IpAddr) -> Vec<u8> {
    match address {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    }
}

fn interface_index(name: &str) -> io::Result<u32> {
    let name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name contains NUL byte",
        )
    })?;
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    if index == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(index)
    }
}

struct NetlinkSocket {
    fd: AsyncFd<OwnedFd>,
    sequence: u32,
}

impl NetlinkSocket {
    fn new() -> io::Result<Self> {
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
            nl_groups: 0,
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
            sequence: 0,
        })
    }

    async fn request(&mut self, message: u16, flags: u16, payload: &[u8]) -> io::Result<()> {
        let sequence = self.next_sequence();
        self.send_message(message, flags, sequence, payload).await?;
        self.read_ack(sequence).await
    }

    async fn flush_routes(&mut self, family: u8, table: u32) -> io::Result<()> {
        let sequence = self.next_sequence();
        let mut payload = Vec::new();
        push_struct(
            &mut payload,
            &rtmsg {
                rtm_family: family,
                rtm_dst_len: 0,
                rtm_src_len: 0,
                rtm_tos: 0,
                rtm_table: if table < 256 {
                    table as u8
                } else {
                    RT_TABLE_UNSPEC as u8
                },
                rtm_protocol: 0,
                rtm_scope: 0,
                rtm_type: 0,
                rtm_flags: 0,
            },
        );
        push_attr_u32(&mut payload, RTA_TABLE as u16, table);
        self.send_message(
            RTM_GETROUTE as u16,
            (NLM_F_REQUEST | NLM_F_DUMP) as u16,
            sequence,
            &payload,
        )
        .await?;
        let routes = self.read_route_dump(sequence, table).await?;
        for route in routes {
            let _ = self
                .request(
                    RTM_DELROUTE as u16,
                    (NLM_F_REQUEST | NLM_F_ACK) as u16,
                    &route,
                )
                .await;
        }
        Ok(())
    }

    fn next_sequence(&mut self) -> u32 {
        self.sequence = self.sequence.wrapping_add(1);
        self.sequence
    }

    async fn send_message(
        &self,
        message: u16,
        flags: u16,
        sequence: u32,
        payload: &[u8],
    ) -> io::Result<()> {
        let mut packet = Vec::with_capacity(size_of::<nlmsghdr>() + payload.len());
        push_struct(
            &mut packet,
            &nlmsghdr {
                nlmsg_len: (size_of::<nlmsghdr>() + payload.len()) as u32,
                nlmsg_type: message,
                nlmsg_flags: flags,
                nlmsg_seq: sequence,
                nlmsg_pid: 0,
            },
        );
        packet.extend_from_slice(payload);
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

    async fn read_ack(&self, sequence: u32) -> io::Result<()> {
        let mut buffer = vec![0u8; BUFFER_SIZE];
        loop {
            let mut ready = self.fd.readable().await?;
            loop {
                match self.recv(&mut buffer)? {
                    RecvResult::Messages(bytes) => {
                        if parse_ack_messages(bytes, sequence)? {
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

    async fn read_route_dump(&self, sequence: u32, table: u32) -> io::Result<Vec<Vec<u8>>> {
        let mut buffer = vec![0u8; BUFFER_SIZE];
        let mut routes = Vec::new();
        loop {
            let mut ready = self.fd.readable().await?;
            loop {
                match self.recv(&mut buffer)? {
                    RecvResult::Messages(bytes) => {
                        if parse_route_dump_messages(bytes, sequence, table, &mut routes)? {
                            return Ok(routes);
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

fn parse_ack_messages(bytes: &[u8], sequence: u32) -> io::Result<bool> {
    parse_messages(bytes, Some(sequence), |header, payload| {
        match header.nlmsg_type as u32 {
            NLMSG_ERROR => {
                parse_error(payload)?;
                Ok(true)
            }
            NLMSG_DONE => Ok(true),
            _ => Ok(false),
        }
    })
}

fn parse_route_dump_messages(
    bytes: &[u8],
    sequence: u32,
    table: u32,
    routes: &mut Vec<Vec<u8>>,
) -> io::Result<bool> {
    parse_messages(bytes, Some(sequence), |header, payload| {
        match header.nlmsg_type as u32 {
            NLMSG_ERROR => {
                parse_error(payload)?;
                Ok(false)
            }
            NLMSG_DONE => Ok(true),
            message if message == RTM_NEWROUTE as u32 => {
                if route_table(payload)? == Some(table) {
                    routes.push(payload.to_vec());
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    })
}

fn parse_messages<F>(bytes: &[u8], sequence: Option<u32>, mut handle: F) -> io::Result<bool>
where
    F: FnMut(nlmsghdr, &[u8]) -> io::Result<bool>,
{
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
        if sequence.is_none_or(|value| value == header.nlmsg_seq) && handle(header, payload)? {
            return Ok(true);
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

fn route_table(payload: &[u8]) -> io::Result<Option<u32>> {
    if payload.len() < size_of::<rtmsg>() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "short route"));
    }
    let header = read_struct::<rtmsg>(payload);
    let mut table = if header.rtm_table == RT_TABLE_UNSPEC as u8 {
        None
    } else {
        Some(header.rtm_table as u32)
    };
    let mut offset = size_of::<rtmsg>();
    while offset + size_of::<rtattr>() <= payload.len() {
        let attr = read_struct::<rtattr>(&payload[offset..]);
        let length = attr.rta_len as usize;
        if length < size_of::<rtattr>() || offset + length > payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid route attribute length",
            ));
        }
        let value = &payload[offset + size_of::<rtattr>()..offset + length];
        if attr.rta_type == RTA_TABLE as u16 {
            if value.len() != size_of::<u32>() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid route table attribute",
                ));
            }
            table = Some(u32::from_ne_bytes(value.try_into().unwrap()));
        }
        offset += align(length);
    }
    Ok(table)
}

fn push_attr_u32(packet: &mut Vec<u8>, attr_type: u16, value: u32) {
    push_attr(packet, attr_type, &value.to_ne_bytes());
}

fn push_attr_c_string(packet: &mut Vec<u8>, attr_type: u16, value: &str) -> io::Result<()> {
    let value = CString::new(value)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "string contains NUL byte"))?;
    push_attr(packet, attr_type, value.as_bytes_with_nul());
    Ok(())
}

fn push_attr(packet: &mut Vec<u8>, attr_type: u16, value: &[u8]) {
    let length = size_of::<rtattr>() + value.len();
    push_struct(
        packet,
        &rtattr {
            rta_len: length as u16,
            rta_type: attr_type,
        },
    );
    packet.extend_from_slice(value);
    packet.resize(align(packet.len()), 0);
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
