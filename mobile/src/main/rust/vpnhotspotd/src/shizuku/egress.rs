use std::io;
use std::mem::size_of;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use libc::{c_int, c_void};
use nix::sys::socket::{
    recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, SockaddrIn, SockaddrIn6,
    SockaddrStorage,
};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::unix::AsyncFd;
use vpnhotspotd::shared::failure::Failure;
use vpnhotspotd::shared::icmp_translate::{Quote, Reported, QUOTE_BYTES};

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

/// Applies each packet's IPv4 DF policy before sending. IPv6 source-fragmentation policy is fixed at open.
pub(crate) fn set_fragmentation(socket: &Socket, fragmentation: Fragmentation) -> io::Result<()> {
    set_ipv4_mtu_discover(
        socket.as_fd(),
        match fragmentation {
            Fragmentation::Prohibited => libc::IP_PMTUDISC_DO,
            Fragmentation::Permitted => IP_PMTUDISC_OMIT,
        },
    )
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
/// relayed reply whose hop limit is unknown cannot be re-originated truthfully.
pub(crate) fn receive(socket: &Socket, buffer: &mut [u8]) -> io::Result<Received> {
    let mut slices = [io::IoSliceMut::new(buffer)];
    let mut space = nix::cmsg_space!(c_int);
    let message = recvmsg::<SockaddrStorage>(
        socket.as_raw_fd(),
        &mut slices,
        Some(&mut space),
        MsgFlags::empty(),
    )
    .map_err(io::Error::from)?;
    let mut hop_limit = None;
    for control in message
        .cmsgs()
        .map_err(|e| io::Error::other(format!("failed to read ancillary data: {e}")))?
    {
        match control {
            ControlMessageOwned::Ipv4Ttl(value) => hop_limit = u8::try_from(value).ok(),
            ControlMessageOwned::Ipv6HopLimit(value) => hop_limit = u8::try_from(value).ok(),
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
    })
}

/// The steps of a TCP connect that are this process's own, named so a failure at one of them is reported as
/// itself. Everything else this function can return is what the path answered.
const CONNECT_SOCKET: &str = "shizuku.tcp_connect_socket";
const CONNECT_NONBLOCK: &str = "shizuku.tcp_connect_nonblock";
const CONNECT_REGISTER: &str = "shizuku.tcp_connect_register";

/// Opens one nonblocking TCP socket. Kept synchronous so descriptor admission immediately follows a
/// successful open, before any other owner turn can retain or admit another descriptor.
pub(crate) fn open_tcp(destination: SocketAddr) -> Result<Socket, Failure> {
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
    socket
        .set_nonblocking(true)
        .map_err(Failure::local(CONNECT_NONBLOCK))?;
    Ok(socket)
}

/// Connects an already-open TCP socket through the app UID's routing policy. Dual-family, unlike the NAT66
/// path's IPv6-only connect, because terminated TCP has to reach both families.
pub(crate) async fn connect_tcp(
    socket: Socket,
    destination: SocketAddr,
) -> Result<Socket, Failure> {
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
pub(crate) struct ErrorQueue {
    /// Where the offending bytes land. One complete Echo header is all Echo correlation reads; UDP uses the
    /// socket and destination metadata instead. The kernel copies what fits and truncates the remainder.
    quote: [u8; QUOTE_BYTES],
    /// Ancillary room for RECVERR and hop limit.
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
    pub(crate) fn new() -> Self {
        ErrorQueue {
            quote: [0u8; QUOTE_BYTES],
            space: nix::cmsg_space!(libc::sock_extended_err, libc::sockaddr_in6, c_int),
        }
    }

    /// Takes the next message off the queue, or answers `None` once it is empty.
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
