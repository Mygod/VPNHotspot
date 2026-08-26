//! Selected-network egress sockets for the Shizuku dataplane.
//!
//! Every socket here follows the same order: create, bind to the selected `Network`, verify options,
//! then connect or send. A failure at any of those steps affects only that operation - there is no
//! process-default fallback and no alternate network, because a packet leaving on the wrong network is
//! the one outcome this mode exists to prevent.
//!
//! `IP_MTU_DISCOVER` is the one load-bearing option below that neither `socket2` nor `nix` exposes, so its
//! raw `libc` call stays at this socket-owner boundary.

use std::io;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use libc::{c_int, c_void};
use nix::sys::socket::{
    recvmsg, sendmsg, setsockopt, sockopt, ControlMessage, ControlMessageOwned, MsgFlags,
    SockaddrIn, SockaddrIn6, SockaddrStorage,
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::unix::AsyncFd;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::icmp_translate::{Quote, Reported, QUOTE_BYTES};
use vpnhotspotd::shared::model::Network;

use crate::android_network::set_socket_network;

/// `IP_PMTUDISC_OMIT`, which the `libc` crate does not export for Android. Clears DF and skips the
/// path-MTU cache, which is what lets Android's downstream fragment an oversized relayed datagram.
const IP_PMTUDISC_OMIT: c_int = 5;

/// What a relayed packet's DF bit was, which the mapping owner reapplies immediately before each send
/// because one socket carries datagrams for both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fragmentation {
    /// DF set: discover the path MTU and fail oversized sends with `EMSGSIZE`.
    Prohibited,
    /// DF clear: let the downstream fragment.
    Permitted,
}

/// Hop metadata is required in both directions: sends carry the client's remaining TTL or hop limit
/// rather than a local default, and a reply without it is dropped rather than guessed at.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Received {
    pub(crate) bytes: usize,
    pub(crate) source: SocketAddr,
    pub(crate) hop_limit: u8,
    pub(crate) interface: u32,
}

fn set_ipv4_mtu_discover(fd: BorrowedFd<'_>, value: c_int) -> io::Result<()> {
    // SAFETY: value outlives the call and its length is exactly what the kernel reads for an int option.
    if unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            &value as *const c_int as *const c_void,
            size_of::<c_int>() as libc::socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Enables the metadata every relayed datagram needs before it can be trusted: the error queue, the
/// received hop limit, and the receiving interface index.
///
/// The interface index is not diagnostic. Inbound UDP and ICMP demultiplex on local address and port
/// alone, so a late reply to a retired mapping can be delivered to whatever socket now holds that
/// identity; requiring the arrival interface to match the current generation's is what rejects it.
pub(crate) fn configure_metadata(socket: &Socket, ipv6: bool) -> io::Result<()> {
    if ipv6 {
        setsockopt(socket, sockopt::Ipv6RecvErr, &true).map_err(io::Error::from)?;
        setsockopt(socket, sockopt::Ipv6RecvHopLimit, &true).map_err(io::Error::from)?;
        setsockopt(socket, sockopt::Ipv6RecvPacketInfo, &true).map_err(io::Error::from)?;
    } else {
        setsockopt(socket, sockopt::Ipv4RecvErr, &true).map_err(io::Error::from)?;
        setsockopt(socket, sockopt::Ipv4RecvTtl, &true).map_err(io::Error::from)?;
        setsockopt(socket, sockopt::Ipv4PacketInfo, &true).map_err(io::Error::from)?;
    }
    Ok(())
}

/// Applied immediately before each IPv4 send and never left to another task to interleave, because one
/// unconnected socket carries datagrams whose DF bits differ.
pub(crate) fn set_fragmentation(socket: &Socket, fragmentation: Fragmentation) -> io::Result<()> {
    set_ipv4_mtu_discover(
        socket.as_fd(),
        match fragmentation {
            Fragmentation::Prohibited => libc::IP_PMTUDISC_DO,
            Fragmentation::Permitted => IP_PMTUDISC_OMIT,
        },
    )
}

/// One unconnected socket per UDP mapping, so a client can reach many destinations through one pinned
/// local identity, which is what makes the outer mapping endpoint-independent.
pub(crate) fn open_udp(network: Network, ipv6: bool) -> io::Result<Socket> {
    let socket = Socket::new(
        if ipv6 { Domain::IPV6 } else { Domain::IPV4 },
        Type::DGRAM,
        Some(Protocol::UDP),
    )?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.set_nonblocking(true)?;
    configure_metadata(&socket, ipv6)?;
    Ok(socket)
}

/// One ping socket per family and generation. Unprivileged ping sockets are ordinary datagram sockets
/// whose identifier the kernel assigns, so Echo identifiers seen on the wire are the kernel's and must be
/// translated back to the client's rather than passed through.
pub(crate) fn open_ping(network: Network, ipv6: bool) -> io::Result<Socket> {
    let socket = Socket::new(
        if ipv6 { Domain::IPV6 } else { Domain::IPV4 },
        Type::DGRAM,
        Some(if ipv6 {
            Protocol::ICMPV6
        } else {
            Protocol::ICMPV4
        }),
    )?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.set_nonblocking(true)?;
    configure_metadata(&socket, ipv6)?;
    Ok(socket)
}

/// Sends one datagram carrying the client's remaining hop limit as ancillary data rather than as a
/// socket-wide default, because one socket relays for many clients.
pub(crate) fn send_to(
    socket: &Socket,
    destination: SocketAddr,
    payload: &[u8],
    hop_limit: u8,
) -> io::Result<usize> {
    let fd = socket.as_raw_fd();
    let slices = [io::IoSlice::new(payload)];
    let hops = hop_limit as c_int;
    let control = [if destination.is_ipv6() {
        ControlMessage::Ipv6HopLimit(&hops)
    } else {
        ControlMessage::Ipv4Ttl(&hops)
    }];
    match destination {
        SocketAddr::V4(address) => sendmsg(
            fd,
            &slices,
            &control,
            MsgFlags::empty(),
            Some(&SockaddrIn::from(address)),
        ),
        SocketAddr::V6(address) => sendmsg(
            fd,
            &slices,
            &control,
            MsgFlags::empty(),
            Some(&SockaddrIn6::from(address)),
        ),
    }
    .map_err(io::Error::from)
}

/// The real length of the datagram at the head of the queue, without consuming it, so the caller can
/// allocate exactly what it will forward.
///
/// `MSG_TRUNC` reports the whole length even though only one byte is copied, and `MSG_PEEK` leaves the
/// datagram queued for the [receive] that follows. The alternative is a fixed buffer per mapping, which
/// would have to hold a whole 64 KiB datagram to be correct - and any local app can multiply that by the
/// mapping ceiling, per the security posture. One extra syscall per reply is the cheaper side of that trade.
pub(crate) fn peek_length(socket: &Socket) -> io::Result<usize> {
    let mut probe = [0u8; 1];
    let mut slices = [io::IoSliceMut::new(&mut probe)];
    recvmsg::<SockaddrStorage>(
        socket.as_raw_fd(),
        &mut slices,
        None,
        MsgFlags::MSG_PEEK | MsgFlags::MSG_TRUNC,
    )
    .map(|message| message.bytes)
    .map_err(io::Error::from)
}

/// Receives one datagram and its hop metadata. Missing metadata is an error rather than a default: a
/// relayed reply whose hop limit is unknown cannot be re-originated truthfully, and one whose arrival
/// interface is unknown cannot be attributed to the current generation.
pub(crate) fn receive(socket: &Socket, buffer: &mut [u8]) -> io::Result<Received> {
    let mut slices = [io::IoSliceMut::new(buffer)];
    let mut space = nix::cmsg_space!(libc::in6_pktinfo, c_int);
    let message = recvmsg::<SockaddrStorage>(
        socket.as_raw_fd(),
        &mut slices,
        Some(&mut space),
        MsgFlags::empty(),
    )
    .map_err(io::Error::from)?;
    let mut hop_limit = None;
    let mut interface = None;
    for control in message
        .cmsgs()
        .map_err(|e| io::Error::other(format!("failed to read ancillary data: {e}")))?
    {
        match control {
            ControlMessageOwned::Ipv4Ttl(value) => hop_limit = u8::try_from(value).ok(),
            ControlMessageOwned::Ipv6HopLimit(value) => hop_limit = u8::try_from(value).ok(),
            ControlMessageOwned::Ipv4PacketInfo(info) => {
                interface = u32::try_from(info.ipi_ifindex).ok()
            }
            // ipi6_ifindex is signed on bionic and unsigned on glibc, so it is widened to a type both
            // agree on and then narrowed: a direct try_from is required on one and a lint error on the
            // other
            ControlMessageOwned::Ipv6PacketInfo(info) => {
                interface = u32::try_from(i64::from(info.ipi6_ifindex)).ok()
            }
            _ => {}
        }
    }
    let source = message
        .address
        .as_ref()
        .and_then(socket_address)
        .ok_or_else(|| io::Error::other("reply carried no source address"))?;
    Ok(Received {
        bytes: message.bytes,
        source,
        hop_limit: hop_limit.ok_or_else(|| io::Error::other("reply carried no hop metadata"))?,
        interface: interface.ok_or_else(|| io::Error::other("reply carried no interface index"))?,
    })
}

/// The steps of a TCP connect that are this process's own, named so a failure at one of them is reported as
/// itself. Everything else this function can return is what the path answered.
const CONNECT_SOCKET: &str = "shizuku.tcp_connect_socket";
const CONNECT_BIND: &str = "shizuku.tcp_connect_bind";
const CONNECT_NONBLOCK: &str = "shizuku.tcp_connect_nonblock";
const CONNECT_REGISTER: &str = "shizuku.tcp_connect_register";

/// Connects one TCP socket on the selected network. Dual-family, unlike the NAT66 path's IPv6-only
/// connect, because terminated TCP has to reach both families.
///
/// The failure is classified rather than merely returned, and the reason is flood resistance: a refused or
/// unreachable destination is the ordinary outcome of a client opening a connection, and a client chooses how
/// many of those it opens - so collapsing it with "this daemon could not create a socket" would mean either a
/// structured report per connection attempt or a real local failure lost among them. See
/// [vpnhotspotd::shared::failure].
pub(crate) async fn connect_tcp(
    network: Network,
    destination: SocketAddr,
) -> Result<Socket, Failure> {
    let socket = Socket::new(
        if destination.is_ipv6() {
            Domain::IPV6
        } else {
            Domain::IPV4
        },
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(Failure::local(CONNECT_SOCKET))?;
    // A selected network that has gone away is `ENONET` here, which is an ordinary consequence of the upstream
    // changing under a flow rather than a local fault - but it arrives from a call only this process makes, so
    // it is named as one and stays reportable. The generation the flow belonged to is being retired anyway.
    set_socket_network(network, socket.as_raw_fd()).map_err(Failure::local(CONNECT_BIND))?;
    socket
        .set_nonblocking(true)
        .map_err(Failure::local(CONNECT_NONBLOCK))?;
    if let Err(error) = socket.connect(&destination.into()) {
        if error.kind() != io::ErrorKind::WouldBlock
            && error.raw_os_error() != Some(libc::EINPROGRESS)
        {
            return Err(Failure::Expected(error));
        }
        settle(&socket).await?;
    }
    Ok(socket)
}

/// Waits for one connect to settle, keeping the local readiness registration apart from what the path
/// answered.
///
/// `AsyncFd::new` and the wait are this process's own doing - a reactor that refused the descriptor, a runtime
/// shutting down - while `SO_ERROR` is the answer. [crate::socket::await_connect] returns both as one
/// `io::Error`, which is exactly the collapse this function exists to avoid.
async fn settle(socket: &Socket) -> Result<(), Failure> {
    let ready = AsyncFd::new(socket.as_fd()).map_err(Failure::local(CONNECT_REGISTER))?;
    drop(
        ready
            .writable()
            .await
            .map_err(Failure::local(CONNECT_REGISTER))?,
    );
    match socket
        .take_error()
        .map_err(Failure::local(CONNECT_REGISTER))?
    {
        Some(error) => Err(Failure::Expected(error)),
        None => Ok(()),
    }
}

/// Reads an unconnected socket's error queue, one message at a time.
///
/// `MSG_ERRQUEUE` is the only place an ICMP error for an unconnected socket surfaces, and it is also where a
/// *local* refusal lands: a DF-set send larger than the cached path MTU calls `ip_local_error` with that MTU,
/// which arrives here as `ee_info`. That is the only way to recover the number on an unconnected socket -
/// `IP_MTU` needs a destination, and one socket here serves many - so this is what makes an honest
/// Fragmentation Needed toward the client possible rather than a guessed one.
///
/// A caller after the local refusal has to drain the whole queue rather than read its head, and that is
/// load-bearing. It is FIFO and holds both kinds: errors *routers* sent about earlier packets, and the local
/// refusal of the send that just failed. Reading only the head hides the refusal behind any router error that
/// arrived first, which is not rare - one traceroute through the relay leaves several. Draining past them
/// costs a syscall each and is what makes the local refusal reachable at all. [drain_local_error] is that
/// caller.
///
/// A router's error cannot stand in for the local one: it is about whichever destination *its* packet was
/// aimed at, which this socket no longer knows, so it cannot be attributed to the send that just failed.
/// Attribution for the local one rests on the drain being done by the code path that saw the send fail,
/// because the kernel reports no address for it either: `ip_local_error` is passed `inet_dport`, which is zero
/// on an unconnected socket, and `ip_recv_error` fills the message address only when that port is non-zero.
///
/// https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ip_output.c#998
/// https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ip_sockglue.c#469
pub(crate) struct ErrorQueue {
    /// Where the offending bytes land. Exactly the prefix a correlation reads, because the rest was never
    /// looked at: the kernel copies what fits and truncates the remainder, which is the intent.
    quote: [u8; QUOTE_BYTES],
    /// Room for the RECVERR message and for the received hop limit and packet info beside it. Those two are
    /// not hypothetical: a router-origin error carries them, because ip_recv_error and ipv6_recv_error append
    /// the socket's normal receive metadata for anything they can name a sender for, while a local refusal has
    /// no sender and carries none. Sizing for the local case alone truncates the other and loses the whole
    /// read.
    space: Vec<u8>,
}

/// One message taken off the error queue.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Drained {
    /// The kernel's own refusal of a send this daemon made.
    Local(QueuedError),
    /// An ICMP error a router sent about traffic this socket relayed.
    Remote(Reported),
    /// A message that was neither - no error entry at all, or a router error missing the metadata a repeat
    /// needs. Answered rather than skipped over internally so that the caller stays the one deciding how long
    /// to keep draining.
    Neither,
}

impl ErrorQueue {
    /// Built once per worker and kept for its life, rather than once per readiness.
    ///
    /// The ancillary buffer is heap-backed - `recvmsg` wants a `Vec` for it - so a per-readiness one would be
    /// an allocation whose count is a remote's choice, and one held across a handover would be an allocation
    /// nobody charged waiting on a channel. One per worker is a fixed cost, charged with the worker's own
    /// record through [ErrorQueue::footprint], and it never crosses an await.
    pub(crate) fn new() -> Self {
        ErrorQueue {
            quote: [0u8; QUOTE_BYTES],
            space: nix::cmsg_space!(
                libc::sock_extended_err,
                libc::sockaddr_in6,
                libc::in6_pktinfo,
                c_int
            ),
        }
    }

    /// Takes the next message off the queue, or answers `None` once it is empty.
    ///
    /// One at a time rather than a batch, and that is the bound: how many errors are queued is a remote's
    /// choice, so collecting them would let a sender pick the size of an allocation. Handing each over as it
    /// is read means exactly one exists at a time, and the caller's own bounded path is what it has to fit
    /// through.
    /// What one of these owns, whatever is in it: its ancillary buffer's capacity and its own inline state.
    ///
    /// The ancillary size is `cmsg_space!` of exactly the messages below, so this is the real figure rather
    /// than an estimate - and it is fixed, so an owner charges it once.
    pub(crate) fn footprint() -> u64 {
        (nix::cmsg_space!(
            libc::sock_extended_err,
            libc::sockaddr_in6,
            libc::in6_pktinfo,
            c_int
        )
        .capacity()
            + std::mem::size_of::<Self>()) as u64
    }

    pub(crate) fn next(&mut self, socket: &Socket) -> io::Result<Option<Drained>> {
        // Scoped so the iovec's borrow of the buffer ends before the offending bytes are copied out of it: the
        // read and the copy both want that buffer, and only one of them can hold it at a time.
        let (bytes, destination, hop_limit, entry) = {
            let mut slices = [io::IoSliceMut::new(&mut self.quote)];
            let message = match recvmsg::<SockaddrStorage>(
                socket.as_raw_fd(),
                &mut slices,
                Some(&mut self.space),
                MsgFlags::MSG_ERRQUEUE,
            ) {
                Ok(message) => message,
                // the queue is empty, which is the only way out
                Err(nix::errno::Errno::EAGAIN) => return Ok(None),
                Err(e) => return Err(io::Error::from(e)),
            };
            let destination = message.address.as_ref().and_then(socket_address);
            let mut hop_limit = None;
            let mut entry = None;
            for control in message
                .cmsgs()
                .map_err(|e| io::Error::other(format!("failed to read ancillary data: {e}")))?
            {
                match control {
                    ControlMessageOwned::Ipv4Ttl(value) => hop_limit = u8::try_from(value).ok(),
                    ControlMessageOwned::Ipv6HopLimit(value) => {
                        hop_limit = u8::try_from(value).ok()
                    }
                    ControlMessageOwned::Ipv4RecvErr(error, offender) => {
                        let remote = offender.map(|address| {
                            IpAddr::V4(Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()))
                        });
                        entry = Some((error, remote));
                    }
                    ControlMessageOwned::Ipv6RecvErr(error, offender) => {
                        let remote = offender
                            .map(|address| IpAddr::V6(Ipv6Addr::from(address.sin6_addr.s6_addr)));
                        entry = Some((error, remote));
                    }
                    _ => {}
                }
            }
            (message.bytes, destination, hop_limit, entry)
        };
        let Some((error, offender)) = entry else {
            return Ok(Some(Drained::Neither));
        };
        if error.ee_origin == libc::SO_EE_ORIGIN_LOCAL {
            return Ok(Some(Drained::Local(QueuedError {
                errno: error.ee_errno as i32,
                info: error.ee_info,
            })));
        }
        // Reported rather than discarded, so the owner - which is the only thing that knows which client this
        // socket serves - can decide whether it has state the error describes. Every one of the three pieces
        // metadata that is required rather than defaulted: without the offender there is no address to speak
        // from, and without the hop limit a repeated error would have to invent one. The destination is left
        // optional because a ping socket genuinely has none to report.
        let (Some(remote), Some(hop_limit)) = (offender, hop_limit) else {
            return Ok(Some(Drained::Neither));
        };
        Ok(Some(Drained::Remote(Reported {
            remote,
            destination,
            hop_limit,
            icmp_type: error.ee_type,
            code: error.ee_code,
            info: error.ee_info,
            quoted: Quote::new(&self.quote[..bytes.min(QUOTE_BYTES)]),
        })))
    }
}

/// Empties the error queue and answers the kernel's own refusal of a send, if one was in it.
///
/// For the send path, which has a failed send in hand and needs the number that explains it. Router errors are
/// read past and dropped here rather than reported, because each is about whichever destination *its* packet
/// was aimed at and one socket serves many - see the module note above [ErrorQueue].
///
/// The queue is the owner's own scratch rather than one built here. Building one per failed send would be a
/// second heap ancillary buffer alive beside the receive worker's, uncharged and as frequent as a client
/// chooses to make sends fail. This path runs inside the ingress owner, which is single-threaded and holds
/// exactly one of these, so lending it is both exact and free.
pub(crate) fn drain_local_error(
    socket: &Socket,
    queue: &mut ErrorQueue,
) -> io::Result<Option<QueuedError>> {
    let mut local = None;
    loop {
        match queue.next(socket)? {
            None => return Ok(local),
            // The first is kept rather than the last: it is the oldest refusal in a FIFO queue, and the send
            // that just failed is what this is called about.
            Some(Drained::Local(queued)) => local = local.or(Some(queued)),
            Some(_) => {}
        }
    }
}

/// The kernel's own refusal of a send, as taken off the error queue.
#[derive(Clone, Copy, Debug)]
pub(crate) struct QueuedError {
    pub(crate) errno: i32,
    /// Protocol-specific. For `EMSGSIZE` it is the path MTU, which is the whole reason this is read.
    pub(crate) info: u32,
}

fn socket_address(storage: &SockaddrStorage) -> Option<SocketAddr> {
    if let Some(address) = storage.as_sockaddr_in() {
        Some(SocketAddr::new(IpAddr::V4(address.ip()), address.port()))
    } else {
        storage
            .as_sockaddr_in6()
            .map(|address| SocketAddr::new(IpAddr::V6(address.ip()), address.port()))
    }
}
