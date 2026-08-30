use std::io::{self, IoSliceMut};
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::fd::AsRawFd;

use libc::c_int;
use nfq::Queue;
use nix::cmsg_space;
use nix::sys::socket::{recvmsg, setsockopt, sockopt, ControlMessageOwned, MsgFlags, SockaddrIn6};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;

use crate::android_network::set_socket_network;
use crate::socket::{send_packet_to, send_packet_to_async, set_nonblocking};
use vpnhotspotd::shared::model::{Network, DAEMON_ICMP_NFQUEUE_NUM};

const ICMPV6_MINIMUM_MTU: usize = 1280;
const IPV6_HEADER_LEN: usize = 40;
const ICMPV6_ERROR_HEADER_LEN: usize = 8;
/// Retains the largest invoking-packet prefix that can appear in an ICMPv6 error constrained to
/// IPv6's 1280-byte minimum MTU. `recvmsg(MSG_TRUNC)` still returns the full queued payload length
/// for counters; bytes beyond this prefix cannot be emitted in the translated downstream error and
/// are discarded. See RFC 4443 section 2.4(c): https://www.rfc-editor.org/rfc/rfc4443.html#section-2.4.
const ERROR_QUEUE_QUOTE_LIMIT: usize =
    ICMPV6_MINIMUM_MTU - IPV6_HEADER_LEN - ICMPV6_ERROR_HEADER_LEN;

/// Queued packets retained by Linux while userspace owes verdicts. This deliberately matches the directly
/// analogous `NFQNL_QMAX_DEFAULT`; once all 1,024 entries are occupied, fail-closed NFQUEUE drops each new
/// packet before Rust receives it and increments the kernel queue-drop counter. Existing entries are unchanged.
/// <https://android.googlesource.com/kernel/common/+/afea13f9ff7137797a2858fc973c226ec93866aa/net/netfilter/nfnetlink_queue.c#51>
const NFQUEUE_MAX_LEN: u32 = 1024;
/// Requests the largest packet copy that nfnetlink can structurally carry: the 16-bit netlink-attribute
/// length minus its four-byte header, or 65,531 bytes. Larger queued packets arrive with `NFQA_CAP_LEN`; the
/// consumer detects that condition and drops the packet rather than parsing an incomplete IPv6 datagram.
/// <https://android.googlesource.com/kernel/common/+/afea13f9ff7137797a2858fc973c226ec93866aa/net/netfilter/nfnetlink_queue.c#55>
const NFQUEUE_COPY_RANGE: u16 = u16::MAX - std::mem::size_of::<libc::nlattr>() as u16;

pub(super) const SO_EE_ORIGIN_LOCAL: u8 = libc::SO_EE_ORIGIN_LOCAL;
pub(super) const SO_EE_ORIGIN_ICMP6: u8 = libc::SO_EE_ORIGIN_ICMP6;
pub(super) const EMSGSIZE: u32 = libc::EMSGSIZE as u32;
// Documentation prefix source used only to probe transparent nonlocal bind support.
const NONLOCAL_PROBE_SOURCE: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);

pub(super) struct ReceivedIcmpPacket<'a> {
    pub(super) source: SocketAddrV6,
    pub(super) payload: &'a [u8],
}

pub(super) struct ErrorQueueMessage {
    pub(super) error: Ipv6RecvError,
    pub(super) offender: Option<SocketAddrV6>,
    pub(super) destination: Option<SocketAddrV6>,
    pub(super) message_len: usize,
    pub(super) payload: Vec<u8>,
}

pub(super) struct Ipv6RecvError {
    pub(super) origin: u8,
    pub(super) errno: u32,
    pub(super) icmp_type: u8,
    pub(super) code: u8,
    pub(super) info: u32,
}

impl From<libc::sock_extended_err> for Ipv6RecvError {
    fn from(error: libc::sock_extended_err) -> Self {
        Self {
            origin: error.ee_origin,
            errno: error.ee_errno,
            icmp_type: error.ee_type,
            code: error.ee_code,
            info: error.ee_info,
        }
    }
}

pub(super) fn recv_raw_icmp_packet<'a>(
    socket: &AsyncFd<Socket>,
    buffer: &'a mut [u8],
) -> io::Result<Option<ReceivedIcmpPacket<'a>>> {
    let (size, source) = {
        let mut iov = [IoSliceMut::new(buffer)];
        let message = recvmsg::<SockaddrIn6>(
            socket.get_ref().as_raw_fd(),
            &mut iov,
            None,
            MsgFlags::MSG_DONTWAIT,
        )
        .map_err(io::Error::from)?;
        if message.bytes == 0 {
            return Ok(None);
        }
        let Some(source) = message.address.map(SocketAddrV6::from) else {
            return Ok(None);
        };
        (message.bytes, source)
    };
    Ok(Some(ReceivedIcmpPacket {
        source,
        payload: &buffer[..size],
    }))
}

pub(super) fn recv_error_queue(socket: &AsyncFd<Socket>) -> io::Result<ErrorQueueMessage> {
    let mut buffer = vec![0u8; ERROR_QUEUE_QUOTE_LIMIT];
    let (size, destination, error, offender) = {
        let mut control = cmsg_space!(libc::sock_extended_err, libc::sockaddr_in6);
        let mut iov = [IoSliceMut::new(&mut buffer)];
        let message = recvmsg::<SockaddrIn6>(
            socket.get_ref().as_raw_fd(),
            &mut iov,
            Some(&mut control),
            MsgFlags::MSG_DONTWAIT | MsgFlags::MSG_ERRQUEUE | MsgFlags::MSG_TRUNC,
        )
        .map_err(io::Error::from)?;
        let destination = message.address.map(SocketAddrV6::from);
        let mut error = None;
        let mut offender = None;
        for cmsg in message.cmsgs().map_err(io::Error::from)? {
            if let ControlMessageOwned::Ipv6RecvErr(value, raw_offender) = cmsg {
                error = Some(value);
                offender = raw_offender
                    .filter(|raw| raw.sin6_family == libc::AF_INET6 as libc::sa_family_t)
                    .map(|raw| SocketAddrV6::from(SockaddrIn6::from(raw)));
            }
        }
        (message.bytes, destination, error, offender)
    };
    let error =
        error.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 error"))?;
    let retained = size.min(buffer.len());
    buffer.truncate(retained);
    Ok(ErrorQueueMessage {
        error: error.into(),
        offender,
        destination,
        message_len: size,
        payload: buffer,
    })
}

pub(super) fn create_downstream_queue() -> io::Result<Queue> {
    let mut queue = Queue::open()?;
    queue.bind(DAEMON_ICMP_NFQUEUE_NUM)?;
    queue.set_copy_range(DAEMON_ICMP_NFQUEUE_NUM, NFQUEUE_COPY_RANGE)?;
    queue.set_queue_max_len(DAEMON_ICMP_NFQUEUE_NUM, NFQUEUE_MAX_LEN)?;
    queue.set_fail_open(DAEMON_ICMP_NFQUEUE_NUM, false)?;
    // `Queue::open` normally suppresses netlink ENOBUFS notifications. Receiving them lets the existing
    // structured receive-error path report userspace socket overruns; kernel NFQUEUE-full drops remain visible
    // only through the kernel queue counters/log because no userspace message exists for those packets.
    queue.set_recv_enobufs(true)?;
    queue.set_nonblocking(true);
    set_nonblocking(queue.as_raw_fd())?;
    Ok(queue)
}

pub(super) fn probe_downstream_send_socket(interface: &str, mark: u32) -> io::Result<()> {
    drop(create_downstream_send_socket(
        interface,
        mark,
        NONLOCAL_PROBE_SOURCE,
    )?);
    Ok(())
}

fn create_downstream_send_socket(
    interface: &str,
    mark: u32,
    source: Ipv6Addr,
) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    socket.bind_device(Some(interface.as_bytes()))?;
    socket.set_mark(mark)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(SocketAddrV6::new(source, 0, 0, 0)))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

pub(super) fn create_upstream_socket(network: Network) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

pub(super) fn enable_ipv6_error_queue(socket: &Socket) -> io::Result<()> {
    setsockopt(socket, sockopt::Ipv6RecvErr, &true).map_err(io::Error::from)
}

pub(super) fn set_upstream_echo_hop_limit(
    socket: &AsyncFd<Socket>,
    hop_limit: u8,
) -> io::Result<()> {
    setsockopt(socket.get_ref(), sockopt::Ipv6Ttl, &c_int::from(hop_limit)).map_err(io::Error::from)
}

pub(super) async fn send_upstream_echo(
    socket: &AsyncFd<Socket>,
    destination: SocketAddrV6,
    packet: &[u8],
) -> io::Result<()> {
    send_packet_to_async(socket, packet, SockAddr::from(destination)).await
}

pub(super) async fn send_downstream_icmp(
    interface: &str,
    mark: u32,
    source: Ipv6Addr,
    target: SocketAddrV6,
    packet: &[u8],
) -> io::Result<()> {
    let socket = create_downstream_send_socket(interface, mark, source)?;
    send_packet_to(
        &socket,
        packet,
        SockAddr::from(SocketAddrV6::new(
            *target.ip(),
            0,
            target.flowinfo(),
            target.scope_id(),
        )),
    )
    .await
}
