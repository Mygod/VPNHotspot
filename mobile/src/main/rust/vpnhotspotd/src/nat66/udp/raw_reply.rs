use std::io::{self, IoSlice};
use std::mem::size_of;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::fd::AsRawFd;

use libc::{c_int, c_void, socklen_t, IPPROTO_IPV6, IPV6_CHECKSUM};
use nix::errno::Errno;
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags, SockaddrIn6};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

use vpnhotspotd::shared::udp_wire::{kernel_checksum_udp_header, UDP_CHECKSUM_OFFSET};

/// Connected peer that keeps the send-only raw socket from receiving. `raw_v6_match` drops every
/// datagram whose source differs from the connected peer
/// (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/raw.c#71),
/// and `ip6_rcv_core` drops multicast sources before raw delivery
/// (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/ip6_input.c#253).
/// RFC 4291 section 2.7.1 reserves the group-ID-zero `ff0e::` and never assigns it, and global scope
/// needs no scope ID. Connect requires a route
/// (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/datagram.c#253):
/// the kernel installs one `ff00::/8` multicast route per IPv6 interface in the `local` table
/// (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/addrconf.c#2512),
/// which rule 0 consults before any fwmark rule. A unicast discard peer such as RFC 6666 `100::/64`
/// has no route under the daemon reply mark, so connecting to it fails with `ENETUNREACH`.
const DISCARD_PEER: Ipv6Addr = Ipv6Addr::new(0xff0e, 0, 0, 0, 0, 0, 0, 0);

/// Send-only raw IPv6 UDP socket for downstream replies whose exact reply-source bind found the
/// port in use. It needs no port bind: the daemon writes the UDP header, `IPV6_PKTINFO` names the
/// non-local source, and the kernel fills the checksum, routes by the daemon reply mark, and
/// fragments oversized datagrams like a UDP socket.
pub(super) struct RawReplySocket(AsyncFd<Socket>);

impl RawReplySocket {
    pub(super) fn new(mark: u32) -> io::Result<Self> {
        let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::UDP))?;
        socket.set_mark(mark)?;
        // Lets IPV6_PKTINFO name a non-local source
        // (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/datagram.c#826).
        socket.set_ip_transparent_v6(true)?;
        // Binding the always-local loopback address fixes the connect source, so connect needs neither
        // multicast source selection on the route's interface nor kernel support for non-local raw binds.
        socket.bind(&SockAddr::from(SocketAddrV6::new(
            Ipv6Addr::LOCALHOST,
            0,
            0,
            0,
        )))?;
        socket.connect(&SockAddr::from(SocketAddrV6::new(DISCARD_PEER, 0, 0, 0)))?;
        socket.set_nonblocking(true)?;
        // Raw sockets receive from creation
        // (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/af_inet6.c#256),
        // so discard copies queued before connect. Without IPV6_CHECKSUM those copies were marked
        // checksum-verified on arrival, so EAGAIN here means empty rather than a dropped bad checksum.
        loop {
            match socket.recv(&mut []) {
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let offset = UDP_CHECKSUM_OFFSET as c_int;
        // SAFETY: the option value points to one live c_int for the duration of the call.
        if unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                IPPROTO_IPV6,
                IPV6_CHECKSUM,
                &offset as *const c_int as *const c_void,
                size_of::<c_int>() as socklen_t,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the socket owns its fd, and the inner socket is never replaced.
        Ok(Self(unsafe {
            AsyncFd::register_with_interest(socket, Interest::WRITABLE)
        }?))
    }

    pub(super) async fn send(
        &self,
        source: SocketAddrV6,
        target: SocketAddrV6,
        payload: &[u8],
    ) -> io::Result<()> {
        let header = kernel_checksum_udp_header(source.port(), target.port(), payload.len())?;
        let iov = [IoSlice::new(&header), IoSlice::new(payload)];
        let packet_info = libc::in6_pktinfo {
            ipi6_addr: libc::in6_addr {
                s6_addr: source.ip().octets(),
            },
            ipi6_ifindex: 0,
        };
        let control = [ControlMessage::Ipv6PacketInfo(&packet_info)];
        // A raw IPv6 sin6_port selects the next header; zero keeps the socket's IPPROTO_UDP
        // (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/raw.c#789).
        let target = SockaddrIn6::from(SocketAddrV6::new(
            *target.ip(),
            0,
            target.flowinfo(),
            target.scope_id(),
        ));
        loop {
            let mut ready = self.0.writable().await?;
            match sendmsg(
                self.0.as_raw_fd(),
                &iov,
                &control,
                MsgFlags::MSG_DONTWAIT,
                Some(&target),
            ) {
                Ok(written) if written == header.len() + payload.len() => return Ok(()),
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "short raw udp reply write",
                    ))
                }
                Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => ready.clear_ready(),
                // Raw sends return transmit-queue drops as ENOBUFS
                // (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/ip6_output.c#1989),
                // which a UDP socket reports as success
                // (https://android.googlesource.com/kernel/common/+/android14-6.1-2024-07_r3/net/ipv6/udp.c#1301).
                Err(Errno::ENOBUFS) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
    }
}
