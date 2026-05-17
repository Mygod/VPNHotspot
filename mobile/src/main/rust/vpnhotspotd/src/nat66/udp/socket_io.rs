use std::io::{self, IoSliceMut};
use std::mem::size_of;
use std::net::{SocketAddrV6, UdpSocket};
use std::os::fd::AsRawFd;

use etherparse::{Ipv6Header, UdpHeader};
use libc::{c_int, c_void, socklen_t, IPPROTO_IPV6};
use nix::cmsg_space;
use nix::sys::socket::{
    recvmsg, send, setsockopt, sockopt, ControlMessageOwned, MsgFlags, SockaddrIn6,
};
use tokio::net::UdpSocket as TokioUdpSocket;

use crate::report;

pub(super) enum UdpForwardResult {
    Sent,
    Dropped,
    PacketTooBig(u32),
    Failed,
}

pub(super) fn enable_recv_hop_limit(socket: &UdpSocket) -> io::Result<()> {
    setsockopt(socket, sockopt::Ipv6RecvHopLimit, &true).map_err(io::Error::from)
}

fn set_upstream_hop_limit(socket: &TokioUdpSocket, hop_limit: u8) -> io::Result<()> {
    setsockopt(socket, sockopt::Ipv6Ttl, &c_int::from(hop_limit)).map_err(io::Error::from)
}

fn upstream_mtu(socket: &TokioUdpSocket) -> io::Result<u32> {
    let mut mtu = 0 as c_int;
    let mut len = size_of::<c_int>() as socklen_t;
    let result = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            IPPROTO_IPV6,
            libc::IPV6_MTU,
            &mut mtu as *mut _ as *mut c_void,
            &mut len,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if mtu <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid ipv6 mtu",
        ));
    }
    Ok(mtu as u32)
}

fn udp_ipv6_packet_exceeds_mtu(payload: &[u8], mtu: u32) -> bool {
    (Ipv6Header::LEN + UdpHeader::LEN + payload.len()) as u64 > u64::from(mtu)
}

pub(super) fn forward_udp_datagram(
    socket: &TokioUdpSocket,
    hop_limit: u8,
    payload: &[u8],
    client: SocketAddrV6,
    destination: SocketAddrV6,
    icmp_errors_registered: bool,
) -> UdpForwardResult {
    if let Err(e) = set_upstream_hop_limit(socket, hop_limit) {
        report::io_with_details(
            "nat66.udp_hop_limit",
            e,
            [
                ("client", client.to_string()),
                ("destination", destination.to_string()),
            ],
        );
        return UdpForwardResult::Dropped;
    }
    loop {
        match send_udp_payload(socket, payload) {
            Ok(_) => return UdpForwardResult::Sent,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Keep the shared UDP owner moving; local datagram queue pressure drops only this packet.
                return UdpForwardResult::Dropped;
            }
            Err(e) if e.raw_os_error() == Some(libc::EMSGSIZE) => match upstream_mtu(socket) {
                Ok(mtu) if udp_ipv6_packet_exceeds_mtu(payload, mtu) => {
                    return UdpForwardResult::PacketTooBig(mtu);
                }
                Ok(_) if icmp_errors_registered => return UdpForwardResult::Dropped,
                Ok(_) => {
                    report::io_with_details(
                        "nat66.udp_send",
                        e,
                        [
                            ("client", client.to_string()),
                            ("destination", destination.to_string()),
                        ],
                    );
                    return UdpForwardResult::Failed;
                }
                Err(mtu_error) => {
                    report::io_with_details(
                        "nat66.udp_mtu",
                        mtu_error,
                        [
                            ("client", client.to_string()),
                            ("destination", destination.to_string()),
                        ],
                    );
                    report::io_with_details(
                        "nat66.udp_send",
                        e,
                        [
                            ("client", client.to_string()),
                            ("destination", destination.to_string()),
                        ],
                    );
                    return UdpForwardResult::Failed;
                }
            },
            Err(e) if icmp_errors_registered && is_udp_icmp_error(&e) => {
                return UdpForwardResult::Dropped;
            }
            Err(e) => {
                report::io_with_details(
                    "nat66.udp_send",
                    e,
                    [
                        ("client", client.to_string()),
                        ("destination", destination.to_string()),
                    ],
                );
                return UdpForwardResult::Failed;
            }
        }
    }
}

pub(super) fn is_udp_icmp_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(
            libc::EACCES
                | libc::ECONNREFUSED
                | libc::EHOSTUNREACH
                | libc::EMSGSIZE
                | libc::ENETUNREACH
                | libc::EPROTO
                | libc::ETIMEDOUT
        )
    )
}

fn send_udp_payload(socket: &TokioUdpSocket, payload: &[u8]) -> io::Result<()> {
    let written =
        send(socket.as_raw_fd(), payload, MsgFlags::MSG_DONTWAIT).map_err(io::Error::from)?;
    if written == payload.len() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short udp datagram write",
        ))
    }
}

pub(super) fn recv_packet(
    fd: i32,
    buffer: &mut [u8],
) -> io::Result<(usize, SocketAddrV6, SocketAddrV6, Option<u8>)> {
    let mut control = cmsg_space!(libc::sockaddr_in6, libc::c_int);
    let mut iov = [IoSliceMut::new(buffer)];
    let message = recvmsg::<SockaddrIn6>(fd, &mut iov, Some(&mut control), MsgFlags::MSG_DONTWAIT)
        .map_err(io::Error::from)?;
    let source = message
        .address
        .map(SocketAddrV6::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing source address"))?;
    let mut destination = None;
    let mut hop_limit = None;
    for cmsg in message.cmsgs().map_err(io::Error::from)? {
        match cmsg {
            ControlMessageOwned::Ipv6OrigDstAddr(raw) => {
                destination = Some(SocketAddrV6::from(SockaddrIn6::from(raw)));
            }
            ControlMessageOwned::Ipv6HopLimit(value)
                if (0..=i32::from(u8::MAX)).contains(&value) =>
            {
                hop_limit = Some(value as u8);
            }
            _ => {}
        }
    }
    let destination = destination.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing original destination")
    })?;
    Ok((message.bytes, source, destination, hop_limit))
}
