use std::collections::HashMap;
use std::io;
use std::mem::{take, MaybeUninit};
use std::net::{IpAddr, Ipv6Addr, SocketAddrV6};
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{pin_mut, TryStreamExt};
use libc::MSG_DONTWAIT;
use rtnetlink::packet_route::{
    address::{AddressAttribute, AddressMessage},
    AddressFamily,
};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio::{select, spawn, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::netlink;
use crate::report;
use crate::socket::await_writable;
use vpnhotspotd::shared::model::{Route, SessionConfig};
use vpnhotspotd::shared::ra_wire::{
    is_router_link_local, make_current_ra_packet, make_zero_lifetime_ra_packet,
};

const RA_PERIOD: Duration = Duration::from_secs(30);
const SUPPRESSED_RA_PERIOD: Duration = Duration::from_secs(3);
const SUPPRESSED_RA_WINDOW: Duration = Duration::from_secs(15);
const ALL_NODES: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);

enum RaRequest {
    RouterSolicitation(SocketAddrV6),
    Ignored,
    WouldBlock,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Router {
    address: Ipv6Addr,
    interface_index: u32,
}

pub(crate) fn spawn_loop(
    config: Arc<Mutex<SessionConfig>>,
    config_changed: Arc<Notify>,
    ipv6_address_changed: Arc<Notify>,
    netlink: netlink::Handle,
    stop: CancellationToken,
    initial: &SessionConfig,
) -> io::Result<JoinHandle<()>> {
    let socket = AsyncFd::new(create_recv_socket(&initial.downstream, initial.reply_mark)?)?;
    Ok(spawn(async move {
        let mut next_ra = Instant::now();
        let mut next_suppressed_ra = None;
        let mut suppressed_prefixes = HashMap::<Route, Instant>::new();
        let mut buffer = [MaybeUninit::<u8>::uninit(); 1500];
        let mut last_router = None;
        let mut address_changed = false;
        let mut waiting_logged = false;
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
            let send_address_changed = take(&mut address_changed);
            let router = match link_local_router(&netlink, &current.downstream).await {
                Ok(router) => router,
                Err(e) => {
                    report::io_with_details(
                        "nat66.ra_link_local_lookup",
                        e,
                        [("interface", current.downstream.clone())],
                    );
                    None
                }
            };
            let router_changed = router != last_router;
            if router_changed {
                last_router = router;
                if let Some(router) = router {
                    eprintln!(
                        "ra using link-local router address {} on {}",
                        router.address, current.downstream
                    );
                    waiting_logged = false;
                }
            }
            if router.is_none() {
                if !waiting_logged {
                    eprintln!(
                        "ra waiting for link-local router address on {}",
                        current.downstream
                    );
                    waiting_logged = true;
                }
                if next_ra <= now {
                    next_ra = now + RA_PERIOD;
                }
                next_suppressed_ra = None;
            }
            suppressed_prefixes.retain(|_, deadline| *deadline > now);
            if suppressed_prefixes.is_empty() {
                next_suppressed_ra = None;
            }
            if let Some(router) = router {
                if !suppressed_prefixes.is_empty()
                    && next_suppressed_ra.is_none_or(|deadline| deadline <= now)
                {
                    withdraw_prefixes_once_with_router(
                        &current,
                        &suppressed_prefixes.keys().copied().collect::<Vec<_>>(),
                        true,
                        router,
                    )
                    .await;
                    next_suppressed_ra = Some(now + SUPPRESSED_RA_PERIOD);
                }
                if send_current || router_changed || send_address_changed || next_ra <= now {
                    if let Err(e) = send_ra(&current, router, None).await {
                        report::io_with_details(
                            "nat66.ra_send_current",
                            e,
                            [("interface", current.downstream.clone())],
                        );
                    }
                    next_ra = now + RA_PERIOD;
                }
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
                _ = ipv6_address_changed.notified() => address_changed = true,
                ready = socket.readable() => {
                    let mut ready = match ready {
                        Ok(ready) => ready,
                        Err(e) => {
                            report::io("nat66.ra_readable", e);
                            break;
                        }
                    };
                    loop {
                        match recv_request(socket.get_ref(), &mut buffer) {
                            Ok(RaRequest::RouterSolicitation(source)) => {
                                if let Some(router) = router {
                                    if let Err(e) = send_ra(&current, router, Some(source)).await {
                                        report::io_with_details(
                                            "nat66.ra_send_solicited",
                                            e,
                                            [
                                                ("interface", current.downstream.clone()),
                                                ("target", source.to_string()),
                                            ],
                                        );
                                    }
                                }
                            }
                            Ok(RaRequest::Ignored) => continue,
                            Ok(RaRequest::WouldBlock) => {
                                ready.clear_ready();
                                break;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                report::io("nat66.ra_recv", e);
                                break;
                            }
                        }
                    }
                }
            }
        }
    }))
}

pub(crate) async fn withdraw_prefixes_once(
    netlink: &netlink::Handle,
    config: &SessionConfig,
    prefixes: &[Route],
    keep_router: bool,
) {
    if config.ipv6_nat.is_none() {
        return;
    }
    let router = match link_local_router(netlink, &config.downstream).await {
        Ok(Some(router)) => router,
        Ok(None) => {
            eprintln!(
                "ra withdraw skipped: missing link-local router address on {}",
                config.downstream
            );
            return;
        }
        Err(e) => {
            report::io_with_details(
                "nat66.ra_withdraw_link_local_lookup",
                e,
                [("interface", config.downstream.clone())],
            );
            return;
        }
    };
    withdraw_prefixes_once_with_router(config, prefixes, keep_router, router).await;
}

async fn withdraw_prefixes_once_with_router(
    config: &SessionConfig,
    prefixes: &[Route],
    keep_router: bool,
    router: Router,
) {
    if config.ipv6_nat.is_none() {
        return;
    }
    let fd = match create_send_socket(&config.downstream, config.reply_mark, router) {
        Ok(fd) => fd,
        Err(e) => {
            report::io_with_details(
                "nat66.ra_withdraw_socket",
                e,
                [("interface", config.downstream.clone())],
            );
            return;
        }
    };
    for prefix in prefixes.iter().cloned() {
        if let Err(e) = send_zero_lifetime_ra(&fd, config, router, prefix, keep_router).await {
            report::io_with_details(
                "nat66.ra_withdraw_send",
                e,
                [
                    ("interface", config.downstream.clone()),
                    (
                        "prefix",
                        format!("{:x}/{}", prefix.prefix, prefix.prefix_len),
                    ),
                ],
            );
        }
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

fn create_send_socket(interface: &str, mark: u32, router: Router) -> io::Result<Socket> {
    let socket = Socket::new(Domain::IPV6, Type::RAW, Some(Protocol::ICMPV6))?;
    socket.bind_device(Some(interface.as_bytes()))?;
    socket.set_mark(mark)?;
    socket.set_unicast_hops_v6(255)?;
    socket.set_multicast_hops_v6(255)?;
    let scope_id = if router.address.is_unicast_link_local() {
        router.interface_index
    } else {
        0
    };
    socket.bind(&SockAddr::from(SocketAddrV6::new(
        router.address,
        0,
        0,
        scope_id,
    )))?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

async fn link_local_router(
    handle: &netlink::Handle,
    interface: &str,
) -> io::Result<Option<Router>> {
    let interface_index = netlink::link_index(handle, interface).await?;
    let _dump = handle.lock_dump().await;
    let addresses = handle
        .raw()
        .address()
        .get()
        .set_link_index_filter(interface_index)
        .execute();
    pin_mut!(addresses);
    let mut router = None;
    while let Some(address) = addresses.try_next().await.map_err(netlink::to_io_error)? {
        if router.is_none() && address.header.family == AddressFamily::Inet6 {
            if let Some(address) = router_address(&address) {
                router = Some(Router {
                    address,
                    interface_index,
                });
            }
        }
    }
    Ok(router)
}

fn router_address(message: &AddressMessage) -> Option<Ipv6Addr> {
    let mut fallback = None;
    for attribute in &message.attributes {
        match attribute {
            AddressAttribute::Local(IpAddr::V6(address)) if is_router_link_local(*address) => {
                return Some(*address);
            }
            AddressAttribute::Address(IpAddr::V6(address)) if is_router_link_local(*address) => {
                fallback = Some(*address);
            }
            _ => {}
        }
    }
    fallback
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

async fn send_ra(
    config: &SessionConfig,
    router: Router,
    target: Option<SocketAddrV6>,
) -> io::Result<()> {
    let ipv6_nat = config
        .ipv6_nat
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 NAT config"))?;
    let fd = create_send_socket(&config.downstream, config.reply_mark, router)?;
    let destination =
        target.unwrap_or_else(|| SocketAddrV6::new(ALL_NODES, 0, 0, router.interface_index));
    let packet = make_current_ra_packet(ipv6_nat.gateway, ipv6_nat.prefix_len, ipv6_nat.mtu);
    sendto_all(&fd, &packet, SockAddr::from(destination)).await
}

async fn send_zero_lifetime_ra(
    socket: &Socket,
    config: &SessionConfig,
    router: Router,
    prefix: Route,
    keep_router: bool,
) -> io::Result<()> {
    let ipv6_nat = config
        .ipv6_nat
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 NAT config"))?;
    let destination = SocketAddrV6::new(ALL_NODES, 0, 0, router.interface_index);
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
