use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::mem::{take, MaybeUninit};
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libc::{if_nametoindex, MSG_DONTWAIT};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::model::{network_prefix, Route, SessionConfig};
use crate::socket::await_writable;

const RA_PERIOD: Duration = Duration::from_secs(30);
const SUPPRESSED_RA_PERIOD: Duration = Duration::from_secs(3);
const SUPPRESSED_RA_WINDOW: Duration = Duration::from_secs(15);
const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

enum RaRequest {
    RouterSolicitation(SocketAddrV6),
    Ignored,
    WouldBlock,
}

pub(crate) fn spawn_loop(
    config: Arc<Mutex<SessionConfig>>,
    config_changed: Arc<Notify>,
    stop: CancellationToken,
) -> io::Result<()> {
    spawn(async move {
        let snapshot = config.lock().await.clone();
        let socket = match create_recv_socket(&snapshot.downstream, snapshot.reply_mark) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("ra socket failed: {e}");
                return;
            }
        };
        let socket = match AsyncFd::new(socket) {
            Ok(socket) => socket,
            Err(e) => {
                eprintln!("ra async socket failed: {e}");
                return;
            }
        };
        let mut next_ra = Instant::now();
        let mut next_suppressed_ra = None;
        let mut suppressed_prefixes = HashMap::<Route, Instant>::new();
        let mut buffer = [MaybeUninit::<u8>::uninit(); 1500];
        loop {
            let now = Instant::now();
            let (current, send_current) = {
                let mut current = config.lock().await;
                let Some(ipv6_nat) = current.ipv6_nat.as_mut() else {
                    break;
                };
                let new_suppressed_prefixes = take(&mut ipv6_nat.suppressed_prefixes);
                let send_current = !new_suppressed_prefixes.is_empty();
                for prefix in new_suppressed_prefixes {
                    suppressed_prefixes.insert(prefix, now + SUPPRESSED_RA_WINDOW);
                }
                (current.clone(), send_current)
            };
            suppressed_prefixes.retain(|_, deadline| *deadline > now);
            if suppressed_prefixes.is_empty() {
                next_suppressed_ra = None;
            }
            if !suppressed_prefixes.is_empty()
                && next_suppressed_ra.is_none_or(|deadline| deadline <= now)
            {
                withdraw_prefixes_once(
                    &current,
                    &suppressed_prefixes.keys().copied().collect::<Vec<_>>(),
                    true,
                )
                .await;
                next_suppressed_ra = Some(now + SUPPRESSED_RA_PERIOD);
            }
            if send_current || next_ra <= now {
                let _ = send_ra(&current, None).await;
                next_ra = now + RA_PERIOD;
            }
            let next_deadline = [Some(next_ra), next_suppressed_ra]
                .into_iter()
                .chain([suppressed_prefixes.values().copied().min()])
                .flatten()
                .min()
                .unwrap();
            select! {
                _ = stop.cancelled() => break,
                _ = config_changed.notified() => {}
                _ = sleep_until(TokioInstant::from_std(next_deadline)) => {}
                ready = socket.readable() => {
                    let Ok(mut ready) = ready else {
                        break;
                    };
                    loop {
                        match recv_request(socket.get_ref(), &mut buffer) {
                            Ok(RaRequest::RouterSolicitation(source)) => {
                                let _ = send_ra(&current, Some(source)).await;
                            }
                            Ok(RaRequest::Ignored) => continue,
                            Ok(RaRequest::WouldBlock) => {
                                ready.clear_ready();
                                break;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                eprintln!("ra recv failed: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        }
    });
    Ok(())
}

pub(crate) async fn withdraw_prefixes_once(
    config: &SessionConfig,
    prefixes: &[Route],
    keep_router: bool,
) {
    let Some(ipv6_nat) = config.ipv6_nat.as_ref() else {
        return;
    };
    let fd = match create_send_socket(&config.downstream, config.reply_mark, ipv6_nat.router) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("ra send socket failed: {e}");
            return;
        }
    };
    for prefix in prefixes.iter().cloned() {
        let _ = send_zero_lifetime_ra(&fd, config, prefix, keep_router).await;
    }
}

fn create_recv_socket(interface: &str, mark: u32) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    socket.bind_device(Some(interface.as_bytes()))?;
    socket.set_mark(mark)?;
    socket.set_unicast_hops_v6(255)?;
    socket.set_multicast_hops_v6(255)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn create_send_socket(interface: &str, mark: u32, router: Ipv6Addr) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    socket.bind_device(Some(interface.as_bytes()))?;
    socket.set_mark(mark)?;
    socket.set_unicast_hops_v6(255)?;
    socket.set_multicast_hops_v6(255)?;
    let scope_id = if router.is_unicast_link_local() {
        interface_index(interface)?
    } else {
        0
    };
    socket.bind(&SockAddr::from(SocketAddrV6::new(router, 0, 0, scope_id)))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn interface_index(interface: &str) -> io::Result<u32> {
    let name = CString::new(interface)?;
    let index = unsafe { if_nametoindex(name.as_ptr()) };
    if index == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(index)
    }
}

fn recv_request(socket: &Socket, buffer: &mut [MaybeUninit<u8>]) -> io::Result<RaRequest> {
    let (size, address) = match socket.recv_from_with_flags(buffer, MSG_DONTWAIT) {
        Ok(result) => result,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            return Ok(RaRequest::WouldBlock)
        }
        Err(error) => return Err(error),
    };
    if size == 0 || unsafe { buffer[0].assume_init() } != 133 {
        return Ok(RaRequest::Ignored);
    }
    address
        .as_socket_ipv6()
        .map(RaRequest::RouterSolicitation)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "expected ipv6 source"))
}

async fn send_ra(config: &SessionConfig, target: Option<SocketAddrV6>) -> io::Result<()> {
    let ipv6_nat = config
        .ipv6_nat
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 NAT config"))?;
    let fd = create_send_socket(&config.downstream, config.reply_mark, ipv6_nat.router)?;
    let ifindex = interface_index(&config.downstream)?;
    let destination = target.unwrap_or_else(|| SocketAddrV6::new(ALL_NODES, 0, 0, ifindex));
    let packet = make_current_ra_packet(ipv6_nat.gateway, ipv6_nat.prefix_len, ipv6_nat.mtu);
    sendto_all(&fd, &packet, SockAddr::from(destination)).await
}

async fn send_zero_lifetime_ra(
    socket: &Socket,
    config: &SessionConfig,
    prefix: Route,
    keep_router: bool,
) -> io::Result<()> {
    let ipv6_nat = config
        .ipv6_nat
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 NAT config"))?;
    let ifindex = interface_index(&config.downstream)?;
    let destination = SocketAddrV6::new(ALL_NODES, 0, 0, ifindex);
    let packet = make_zero_lifetime_ra_packet(prefix, ipv6_nat.mtu, keep_router);
    sendto_all(socket, &packet, SockAddr::from(destination)).await
}

async fn sendto_all(socket: &Socket, mut packet: &[u8], address: SockAddr) -> io::Result<()> {
    while !packet.is_empty() {
        let written = match socket.send_to(packet, &address) {
            Ok(written) => written,
            Err(error) => {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    await_writable(socket.as_fd()).await?;
                    continue;
                }
                return Err(error);
            }
        };
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "socket write failed",
            ));
        }
        packet = &packet[written..];
    }
    Ok(())
}

struct RaAdvertisement {
    dns_server: Ipv6Addr,
    advertised_prefix: Ipv6Addr,
    prefix_len: u8,
    mtu: u32,
    router_lifetime: u16,
    valid_lifetime: u32,
    preferred_lifetime: u32,
    rdnss_lifetime: u32,
}

fn make_current_ra_packet(gateway: Ipv6Addr, prefix_len: u8, mtu: u32) -> Vec<u8> {
    RaAdvertisement {
        dns_server: gateway,
        advertised_prefix: gateway,
        prefix_len,
        mtu,
        router_lifetime: 1800,
        valid_lifetime: 3600,
        preferred_lifetime: 1800,
        rdnss_lifetime: 600,
    }
    .encode()
}

fn make_zero_lifetime_ra_packet(prefix: Route, mtu: u32, keep_router: bool) -> Vec<u8> {
    let gateway = Ipv6Addr::from(prefix.prefix);
    RaAdvertisement {
        dns_server: gateway,
        advertised_prefix: gateway,
        prefix_len: prefix.prefix_len,
        mtu,
        router_lifetime: if keep_router { 1800 } else { 0 },
        valid_lifetime: 0,
        preferred_lifetime: 0,
        rdnss_lifetime: 0,
    }
    .encode()
}

impl RaAdvertisement {
    fn encode(self) -> Vec<u8> {
        let mut packet = Vec::with_capacity(80);
        packet.push(134);
        packet.push(0);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.push(64);
        packet.push(0);
        packet.extend_from_slice(&self.router_lifetime.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());

        packet.push(3);
        packet.push(4);
        packet.push(self.prefix_len);
        packet.push(0xc0);
        packet.extend_from_slice(&self.valid_lifetime.to_be_bytes());
        packet.extend_from_slice(&self.preferred_lifetime.to_be_bytes());
        packet.extend_from_slice(&0u32.to_be_bytes());
        packet.extend_from_slice(&network_prefix(self.advertised_prefix, self.prefix_len));

        packet.push(5);
        packet.push(1);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&self.mtu.to_be_bytes());

        packet.push(25);
        packet.push(3);
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&self.rdnss_lifetime.to_be_bytes());
        packet.extend_from_slice(&self.dns_server.octets());
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_lifetime_ra_withdraws_dns_server() {
        let dns_server: Ipv6Addr = "fd47:6b7c:2186:b452::1".parse().unwrap();
        let packet = RaAdvertisement {
            dns_server,
            advertised_prefix: dns_server,
            prefix_len: 64,
            mtu: 1500,
            router_lifetime: 0,
            valid_lifetime: 0,
            preferred_lifetime: 0,
            rdnss_lifetime: 0,
        }
        .encode();
        assert_eq!(&packet[40..44], &0u32.to_be_bytes());
        assert_eq!(&packet[packet.len() - 16..], &dns_server.octets());
    }
}
