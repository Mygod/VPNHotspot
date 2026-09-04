//! Opens app-UID egress sockets and installs descriptor-lifetime options.
use std::io;
use std::os::fd::AsFd;

use nix::sys::socket::{setsockopt, sockopt};
use socket2::{Domain, Protocol, Socket, Type};

/// Opens an unconnected UDP mapping socket.
pub fn open_udp(ipv6: bool) -> io::Result<Socket> {
    open_socket(ipv6, Protocol::UDP)
}

/// Opens a datagram ping socket; replies use a kernel-assigned identifier that callers translate for clients.
pub fn open_ping(ipv6: bool) -> io::Result<Socket> {
    open_socket(
        ipv6,
        if ipv6 {
            Protocol::ICMPV6
        } else {
            Protocol::ICMPV4
        },
    )
}

fn open_socket(ipv6: bool, protocol: Protocol) -> io::Result<Socket> {
    let socket = Socket::new(
        if ipv6 { Domain::IPV6 } else { Domain::IPV4 },
        Type::DGRAM,
        Some(protocol),
    )?;
    socket.set_nonblocking(true)?;
    // Never expose a socket without all descriptor-lifetime options installed.
    configure(&socket, ipv6)?;
    Ok(socket)
}

/// Enables error/hop metadata and disables IPv6 source fragmentation.
fn configure<F: AsFd>(socket: &F, ipv6: bool) -> io::Result<()> {
    if ipv6 {
        setsockopt(socket, sockopt::Ipv6RecvErr, &true).map_err(io::Error::from)?;
        setsockopt(socket, sockopt::Ipv6RecvHopLimit, &true).map_err(io::Error::from)?;
        // Force EMSGSIZE/error-queue PMTU reporting instead of Linux source fragmentation.
        setsockopt(socket, sockopt::Ipv6DontFrag, &true).map_err(io::Error::from)?;
    } else {
        setsockopt(socket, sockopt::Ipv4RecvErr, &true).map_err(io::Error::from)?;
        setsockopt(socket, sockopt::Ipv4RecvTtl, &true).map_err(io::Error::from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::mem::size_of;
    use std::os::fd::AsRawFd;

    use nix::sys::socket::getsockopt;

    use super::*;

    /// `IP_PMTUDISC_DO`; IPv4 fragmentation policy is applied per send instead.
    const IP_PMTUDISC_DO: libc::c_int = 2;

    /// Returns `None` when the host does not permit unprivileged ping sockets.
    fn ping(ipv6: bool) -> Option<Socket> {
        match open_ping(ipv6) {
            Ok(socket) => Some(socket),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM)) => None,
            Err(e) => panic!("an unprivileged ping socket for ipv6={ipv6}: {e}"),
        }
    }

    fn ipv4_mtu_discover(socket: &Socket) -> libc::c_int {
        let mut value: libc::c_int = -1;
        let mut length = size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: the pointers name live locals and `length` matches the integer option.
        let read = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_MTU_DISCOVER,
                &mut value as *mut libc::c_int as *mut libc::c_void,
                &mut length,
            )
        };
        assert_eq!(read, 0, "{}", io::Error::last_os_error());
        value
    }

    #[test]
    fn ipv6_relay_sockets_refuse_to_fragment_what_they_send() {
        for socket in [
            Some(open_udp(true).expect("an IPv6 UDP socket")),
            ping(true),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(getsockopt(&socket, sockopt::Ipv6DontFrag), Ok(true));
            assert_eq!(getsockopt(&socket, sockopt::Ipv6RecvErr), Ok(true));
            assert_eq!(getsockopt(&socket, sockopt::Ipv6RecvHopLimit), Ok(true));
        }
    }

    #[test]
    fn ipv4_relay_sockets_leave_fragmentation_per_send() {
        for socket in [
            Some(open_udp(false).expect("an IPv4 UDP socket")),
            ping(false),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(getsockopt(&socket, sockopt::Ipv4RecvErr), Ok(true));
            assert_eq!(getsockopt(&socket, sockopt::Ipv4RecvTtl), Ok(true));
            assert_ne!(ipv4_mtu_discover(&socket), IP_PMTUDISC_DO);
            assert!(getsockopt(&socket, sockopt::Ipv6DontFrag).is_err());
        }
    }

    #[test]
    fn every_egress_socket_is_nonblocking_before_any_owner_registers_it() {
        for socket in [
            Some(open_udp(true).expect("an IPv6 UDP socket")),
            Some(open_udp(false).expect("an IPv4 UDP socket")),
            ping(true),
            ping(false),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(socket.nonblocking().map_err(|e| e.to_string()), Ok(true));
        }
    }
}
