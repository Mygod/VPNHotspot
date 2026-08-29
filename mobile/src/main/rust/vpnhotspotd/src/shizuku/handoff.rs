use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use libc::{c_int, c_short, ioctl, F_GETFL, IFF_NO_PI, IFF_TUN, IFNAMSIZ, O_NONBLOCK};
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use tokio::net::UnixStream;
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

use crate::control_wire::MAX_CONTROL_PACKET_SIZE;

/// Keeps the failing syscall's errno while naming what was being attempted, since a bare errno on a
/// descriptor check says nothing about which check failed. `#[track_caller]` so the report names the check
/// rather than this line.
#[track_caller]
fn context(message: &'static str) -> io::Error {
    io::Error::last_os_error().with_report_context(message)
}

#[track_caller]
fn refused(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
        .with_report_context("shizuku.handoff.verify_tun")
}

/// `TUNGETIFF`, which is the only way to learn which interface a descriptor belongs to. Not exported
/// by the `libc` crate for Android, so it is spelled out here: `_IOR('T', 210, unsigned int)`.
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

/// Reads the first frame entirely through `recvmsg`, because a plain `read` that consumed the bytes the
/// descriptor is attached to would discard it. Ancillary data is collected across every call, since the
/// sender's write boundaries are not something this side can rely on.
pub(crate) async fn recv_start_frame(stream: &UnixStream) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
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
            format!("invalid start frame length {length}"),
        ));
    }
    let mut payload = vec![0u8; length];
    let mut filled = 0;
    while filled < length {
        filled += recv_into(stream, &mut payload[filled..], &mut received).await?;
    }
    Ok((payload, received))
}

async fn recv_into(
    stream: &UnixStream,
    buffer: &mut [u8],
    received: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    loop {
        stream.readable().await?;
        let attempt = stream.try_io(tokio::io::Interest::READABLE, || {
            // Room for two descriptors lets us reject an over-sending peer instead of truncating it.
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
                        "control socket closed before the session started",
                    ));
                }
                return Ok(bytes);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Turns what the start call transferred into the one descriptor this session owns, or refuses it.
pub(crate) fn verify_tun(
    received: Vec<OwnedFd>,
    interface_name: &str,
    mtu: u32,
) -> io::Result<(OwnedFd, Ipv4Addr)> {
    let count = received.len();
    let mut descriptors = received.into_iter();
    let Some(descriptor) = descriptors.next() else {
        return Err(
            io::Error::new(io::ErrorKind::InvalidData, "no descriptor was transferred")
                .with_report_context("shizuku.handoff.descriptors"),
        );
    };
    if count > 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} descriptors were transferred", count),
        )
        .with_report_context("shizuku.handoff.descriptors"));
    }
    let fd = descriptor.as_raw_fd();
    // SAFETY: fd is owned and open for the duration of this call.
    let flags = unsafe { libc::fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(context("shizuku.handoff.descriptor_flags"));
    }
    if flags & O_NONBLOCK == 0 {
        return Err(refused("transferred descriptor is blocking".to_owned()));
    }
    let mut request = IfReqFlags {
        name: [0; IFNAMSIZ],
        flags: 0,
        padding: [0; 22],
    };
    // SAFETY: TUNGETIFF writes an ifreq, and request is exactly that layout.
    if unsafe { ioctl(fd, TUNGETIFF as _, &mut request as *mut IfReqFlags) } < 0 {
        return Err(context("shizuku.handoff.tungetiff"));
    }
    let end = request
        .name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(IFNAMSIZ);
    let name = String::from_utf8_lossy(&request.name[..end]).into_owned();
    if name != interface_name {
        return Err(refused(format!(
            "transferred TUN is {name} but {interface_name} was expected"
        )));
    }
    let expected = (IFF_TUN | IFF_NO_PI) as c_short;
    if request.flags & expected != expected {
        return Err(refused(format!(
            "transferred TUN has flags {:#x}",
            request.flags
        )));
    }
    let interface = interface_mtu(&request.name)?;
    if interface != mtu {
        return Err(refused(format!(
            "{name} has MTU {interface} but {mtu} was declared"
        )));
    }
    // Verify the startup declaration against the address on the transferred interface.
    Ok((descriptor, interface_address(&request.name)?))
}

fn ioctl_probe() -> io::Result<OwnedFd> {
    // SAFETY: an AF_INET datagram socket needs no privilege and is only an ioctl target here.
    let probe = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if probe < 0 {
        return Err(context("shizuku.handoff.ioctl_socket"));
    }
    // SAFETY: this function owns the successful descriptor and returns that ownership.
    Ok(unsafe { OwnedFd::from_raw_fd(probe) })
}

/// Primary IPv4 address checked against the startup declaration.
fn interface_address(name: &[u8; IFNAMSIZ]) -> io::Result<Ipv4Addr> {
    let probe = ioctl_probe()?;
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
        return Err(context("shizuku.handoff.interface_address"));
    }
    if request.family != libc::AF_INET as u16 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("interface address is family {}", request.family),
        )
        .with_report_context("shizuku.handoff.interface_address"));
    }
    Ok(Ipv4Addr::from(request.address))
}

/// The MTU is immutable for the session in the agent's `LinkProperties` and in every packetization decision,
/// so it is read from the interface rather than believed. `TUNGETIFF` does not report it, hence a second ioctl
/// on a throwaway socket.
fn interface_mtu(name: &[u8; IFNAMSIZ]) -> io::Result<u32> {
    let probe = ioctl_probe()?;
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
        return Err(context("shizuku.handoff.interface_mtu"));
    }
    u32::try_from(request.mtu)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("negative MTU: {e}")))
        .with_report_context("shizuku.handoff.interface_mtu")
}
