//! Shizuku-mode bootstrap.
//!
//! The app UID launches this binary directly, so there is no root shell and no privileged dataplane.
//! Before anything else happens the daemon receives the TUN it will own over `SCM_RIGHTS`.
//!
//! The app cannot prove what arrived on its side of that transfer: it only sets the descriptor
//! nonblocking before duplicating it and keeps the original. Everything the app asserts about the
//! descriptor is therefore re-checked here against the descriptor itself.

use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use libc::{c_int, c_short, ioctl, F_GETFL, IFF_NO_PI, IFF_TUN, IFNAMSIZ, O_NONBLOCK};
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use prost::Message;
use tokio::net::UnixStream;
use vpnhotspotd::shared::proto::daemon::{BootstrapConfig, BootstrapReady};

use crate::control::wire::{connect_control_socket, send_packet, MAX_CONTROL_PACKET_SIZE};

/// Keeps the failing syscall's errno while naming what was being attempted, since a bare errno on a
/// descriptor check says nothing about which check failed.
fn context(message: &str) -> io::Error {
    let error = io::Error::last_os_error();
    io::Error::new(error.kind(), format!("{message}: {error}"))
}

/// `TUNGETIFF`, which is the only way to learn which interface a descriptor belongs to. Not exported
/// by the `libc` crate for Android, so it is spelled out here: `_IOR('T', 210, unsigned int)`.
///
/// Kept as `u32` and cast at the call site because bionic's `ioctl` takes an `int` request while other libc
/// signatures use an `unsigned long`.
const TUNGETIFF: u32 = 0x8004_54d2;

/// `SIOCGIFMTU`, cast at the call site for the same reason as [TUNGETIFF].
const SIOCGIFMTU: u32 = 0x8921;

/// `struct ifreq` as `TUNGETIFF` fills it in: the name, then the flags the interface was created with.
/// The trailing union is 24 bytes on every supported ABI, so each view below pads to that size and reads
/// only the member that request actually writes.
#[repr(C)]
struct IfReqFlags {
    name: [u8; IFNAMSIZ],
    flags: c_short,
    padding: [u8; 22],
}

/// `SIOCGIFADDR`, cast at the call site for the same reason as [TUNGETIFF].
const SIOCGIFADDR: u32 = 0x8915;

/// The same `struct ifreq` as `SIOCGIFADDR` fills it in: a `sockaddr_in` in the trailing union, of which only
/// the family and the address are read. The rest of the union is padding here, exactly as above.
#[repr(C)]
struct IfReqAddr {
    name: [u8; IFNAMSIZ],
    family: u16,
    port: u16,
    address: [u8; 4],
    padding: [u8; 16],
}

/// The same `struct ifreq` as `SIOCGIFMTU` fills it in.
#[repr(C)]
struct IfReqMtu {
    name: [u8; IFNAMSIZ],
    mtu: c_int,
    padding: [u8; 20],
}

pub(crate) async fn run(socket_name: String) -> io::Result<()> {
    let mut stream = connect_control_socket(&socket_name).await?;
    let (payload, tun) = recv_frame_with_descriptor(&stream).await?;
    let config = BootstrapConfig::decode(payload.as_slice())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let (interface_name, gateway) = verify_tun(&tun, &config)?;
    send_packet(
        &mut stream,
        &BootstrapReady {
            interface_name: interface_name.clone(),
        }
        .encode_to_vec(),
    )
    .await?;
    // the bootstrap's job ends here; the session owns the descriptor and the control socket from now on
    //
    // Nothing here coordinates with root mode, and nothing needs to: this daemon relays a TUN that Android's
    // tethering may or may not have selected as its upstream, and root mode's own per-interface routing is
    // installed independently of it. When both are running, root's routing takes precedence over whatever
    // upstream Android picked, by the ordinary root design and without either side being told.
    crate::app_session::run(stream, tun, interface_name, gateway, config.mtu as usize).await
}

/// Reads one frame entirely through `recvmsg`, because a plain `read` that consumes the bytes the
/// descriptor is attached to would discard it. Ancillary data is collected across every call, since the
/// sender's write boundaries are not something this side can rely on.
async fn recv_frame_with_descriptor(stream: &UnixStream) -> io::Result<(Vec<u8>, OwnedFd)> {
    let mut received = Vec::new();
    let mut header = [0u8; 4];
    let mut filled = 0;
    while filled < header.len() {
        filled += recv_into(stream, &mut header[filled..], &mut received).await?;
    }
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 || length > MAX_CONTROL_PACKET_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid bootstrap frame length {length}"),
        ));
    }
    let mut payload = vec![0u8; length];
    let mut filled = 0;
    while filled < length {
        filled += recv_into(stream, &mut payload[filled..], &mut received).await?;
    }
    let mut descriptors = received.into_iter();
    let descriptor = descriptors.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "no descriptor was transferred")
    })?;
    // extra descriptors are closed rather than leaked, then the handshake fails
    let extra = descriptors.count();
    if extra > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} extra descriptors were transferred", extra),
        ));
    }
    Ok((payload, descriptor))
}

async fn recv_into(
    stream: &UnixStream,
    buffer: &mut [u8],
    received: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    loop {
        stream.readable().await?;
        let attempt = stream.try_io(tokio::io::Interest::READABLE, || {
            // room for two descriptors although exactly one is required: the surplus is what lets an
            // over-sending peer be detected and rejected rather than silently truncated to one
            let mut space = nix::cmsg_space!([RawFd; 2]);
            let mut slices = [io::IoSliceMut::new(buffer)];
            let message = recvmsg::<()>(
                stream.as_raw_fd(),
                &mut slices,
                Some(&mut space),
                MsgFlags::empty(),
            )
            .map_err(io::Error::from)?;
            let mut fds = Vec::new();
            for control in message
                .cmsgs()
                .map_err(|e| io::Error::other(format!("failed to read ancillary data: {e}")))?
            {
                if let ControlMessageOwned::ScmRights(raw) = control {
                    // SAFETY: the kernel just installed these descriptors in this process, and nothing
                    // else owns them.
                    fds.extend(
                        raw.into_iter()
                            .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                    );
                }
            }
            Ok((message.bytes, fds))
        });
        match attempt {
            Ok((bytes, fds)) => {
                received.extend(fds);
                if bytes == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "control socket closed during bootstrap",
                    ));
                }
                return Ok(bytes);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Requires a nonblocking TUN naming the expected interface. The flags matter as much as the name: a
/// TAP descriptor, or one created without `IFF_NO_PI`, would carry a different framing than every
/// parser above assumes.
fn verify_tun(descriptor: &OwnedFd, config: &BootstrapConfig) -> io::Result<(String, Ipv4Addr)> {
    let fd = descriptor.as_raw_fd();
    // SAFETY: fd is owned and open for the duration of this call.
    let flags = unsafe { libc::fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(context("failed to read descriptor flags"));
    }
    if flags & O_NONBLOCK == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "transferred descriptor is blocking",
        ));
    }
    let mut request = IfReqFlags {
        name: [0; IFNAMSIZ],
        flags: 0,
        padding: [0; 22],
    };
    // SAFETY: TUNGETIFF writes an ifreq, and request is exactly that layout.
    if unsafe { ioctl(fd, TUNGETIFF as _, &mut request as *mut IfReqFlags) } < 0 {
        return Err(context("transferred descriptor is not a TUN"));
    }
    let end = request
        .name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(IFNAMSIZ);
    let name = String::from_utf8_lossy(&request.name[..end]).into_owned();
    if name != config.interface_name {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "transferred TUN is {name} but {} was expected",
                config.interface_name
            ),
        ));
    }
    let expected = (IFF_TUN | IFF_NO_PI) as c_short;
    if request.flags & expected != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("transferred TUN has flags {:#x}", request.flags),
        ));
    }
    let mtu = interface_mtu(&request.name)?;
    if mtu != config.mtu {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} has MTU {mtu} but {} was declared", config.mtu),
        ));
    }
    // Read here, where the descriptor is, and compared later against the address each config declares: this is
    // the one field of that declaration the daemon can check against the interface at all.
    Ok((name, interface_address(&request.name)?))
}

/// The interface's primary IPv4 address, which is the one field of the app's declaration the daemon can check
/// against the interface itself.
///
/// An ioctl on a descriptor rather than an enumeration, because an enumeration is not available: binding a
/// `NETLINK_ROUTE` socket is denied at the app UID, and so is `/proc/net/if_inet6`. Both were measured on device.
/// That is also why there is no IPv6 counterpart here - `SIOCGIFADDR` is IPv4-only, and nothing else at this UID
/// can read an IPv6 address - so the IPv6 gateway address is taken on the app's word by necessity rather than by
/// choice.
fn interface_address(name: &[u8; IFNAMSIZ]) -> io::Result<Ipv4Addr> {
    // SAFETY: an AF_INET datagram socket needs no privilege and is only an ioctl target here.
    let probe = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if probe < 0 {
        return Err(context("failed to open an ioctl socket"));
    }
    // SAFETY: probe is owned by this function and closed on every path below.
    let probe = unsafe { OwnedFd::from_raw_fd(probe) };
    let mut request = IfReqAddr {
        name: *name,
        family: 0,
        port: 0,
        address: [0; 4],
        padding: [0; 16],
    };
    // SAFETY: SIOCGIFADDR writes a sockaddr_in into the union following the name, which this view names.
    if unsafe {
        ioctl(
            probe.as_raw_fd(),
            SIOCGIFADDR as _,
            &mut request as *mut IfReqAddr,
        )
    } < 0
    {
        return Err(context("failed to read the interface address"));
    }
    if request.family != libc::AF_INET as u16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("interface address is family {}", request.family),
        ));
    }
    Ok(Ipv4Addr::from(request.address))
}

/// The MTU is immutable for the session in the agent's `LinkProperties` and in every packetization decision,
/// so it is read from the interface rather than believed. `TUNGETIFF` does not report it, hence a second ioctl
/// on a throwaway socket.
fn interface_mtu(name: &[u8; IFNAMSIZ]) -> io::Result<u32> {
    // SAFETY: an AF_INET datagram socket needs no privilege and is only an ioctl target here.
    let probe = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if probe < 0 {
        return Err(context("failed to open an ioctl socket"));
    }
    // SAFETY: probe is owned by this function and closed on every path below.
    let probe = unsafe { OwnedFd::from_raw_fd(probe) };
    let mut request = IfReqMtu {
        name: *name,
        mtu: 0,
        padding: [0; 20],
    };
    // SAFETY: SIOCGIFMTU writes an int into the union following the name, which this view names.
    if unsafe {
        ioctl(
            probe.as_raw_fd(),
            SIOCGIFMTU as _,
            &mut request as *mut IfReqMtu,
        )
    } < 0
    {
        return Err(context("failed to read the interface MTU"));
    }
    u32::try_from(request.mtu)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("negative MTU: {e}")))
}
