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
use vpnhotspotd::shared::model::{ipv6_to_u128, Route, SessionConfig};
use vpnhotspotd::shared::ra_wire::{
    is_router_link_local, make_current_ra_packet, make_zero_lifetime_ra_packet,
};

const RA_PERIOD: Duration = Duration::from_secs(30);
const SUPPRESSED_RA_PERIOD: Duration = Duration::from_secs(3);
const SUPPRESSED_RA_WINDOW: Duration = Duration::from_secs(15);
const DEFAULT_MTU: u32 = 1500;
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
        let mut refresh_downstream_prefixes = true;
        let mut waiting_logged = false;
        loop {
            let now = Instant::now();
            let current = {
                let current = config.lock().await;
                if current.ipv6_nat.is_none() {
                    break;
                }
                current.clone()
            };
            let send_address_changed = take(&mut address_changed);
            let mut send_current = false;
            if take(&mut refresh_downstream_prefixes) || send_address_changed {
                let mtu_prefixes =
                    match downstream_ipv6_prefixes(&netlink, &current.downstream).await {
                        Ok(prefixes) => prefixes,
                        Err(e) => {
                            report::io_with_details(
                                "nat66.ra_downstream_prefixes",
                                e,
                                [("interface", current.downstream.clone())],
                            );
                            Vec::new()
                        }
                    };
                if let Some(ipv6_nat) = current.ipv6_nat.as_ref() {
                    let current_prefix = Route {
                        prefix: ipv6_to_u128(ipv6_nat.gateway),
                        prefix_len: ipv6_nat.prefix_len,
                    };
                    for prefix in mtu_prefixes {
                        if prefix != current_prefix {
                            suppressed_prefixes.insert(prefix, now + SUPPRESSED_RA_WINDOW);
                            send_current = true;
                        }
                    }
                }
            }
            let mtu = downstream_mtu(&netlink, &current.downstream, "nat66.ra_mtu_lookup").await;
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
                        mtu,
                    )
                    .await;
                    next_suppressed_ra = Some(now + SUPPRESSED_RA_PERIOD);
                }
                if send_current || router_changed || send_address_changed || next_ra <= now {
                    if let Err(e) = send_ra(&current, router, None, mtu).await {
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
                _ = config_changed.notified() => refresh_downstream_prefixes = true,
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
                                    if let Err(e) = send_ra(&current, router, Some(source), mtu).await {
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
    let mtu = downstream_mtu(netlink, &config.downstream, "nat66.ra_withdraw_mtu_lookup").await;
    withdraw_prefixes_once_with_router(config, prefixes, keep_router, router, mtu).await;
}

async fn withdraw_prefixes_once_with_router(
    config: &SessionConfig,
    prefixes: &[Route],
    keep_router: bool,
    router: Router,
    mtu: u32,
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
        if let Err(e) = send_zero_lifetime_ra(&fd, router, prefix, keep_router, mtu).await {
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

async fn downstream_mtu(handle: &netlink::Handle, interface: &str, context: &str) -> u32 {
    match netlink::link_mtu(handle, interface).await {
        Ok(mtu) => mtu,
        Err(e) if e.kind() == io::ErrorKind::NotFound => DEFAULT_MTU,
        Err(e) => {
            report::io_with_details(context, e, [("interface", interface.to_owned())]);
            DEFAULT_MTU
        }
    }
}

async fn downstream_ipv6_prefixes(
    handle: &netlink::Handle,
    interface: &str,
) -> io::Result<Vec<Route>> {
    let interface_index = match netlink::link_index(handle, interface).await {
        Ok(index) => index,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let _dump = handle.lock_dump().await;
    let addresses = handle
        .raw()
        .address()
        .get()
        .set_link_index_filter(interface_index)
        .execute();
    pin_mut!(addresses);
    let mut prefixes = Vec::new();
    while let Some(message) = addresses.try_next().await.map_err(netlink::to_io_error)? {
        if message.header.family != AddressFamily::Inet6 {
            continue;
        }
        if let Some(prefix) = downstream_ipv6_prefix(&message) {
            if !prefixes.contains(&prefix) {
                prefixes.push(prefix);
            }
        }
    }
    Ok(prefixes)
}

fn downstream_ipv6_prefix(message: &AddressMessage) -> Option<Route> {
    let mut fallback = None;
    for attribute in &message.attributes {
        match attribute {
            AddressAttribute::Local(IpAddr::V6(address)) => {
                return routable_ipv6_prefix(*address, message.header.prefix_len);
            }
            AddressAttribute::Address(IpAddr::V6(address)) => {
                fallback = routable_ipv6_prefix(*address, message.header.prefix_len);
            }
            _ => {}
        }
    }
    fallback
}

fn routable_ipv6_prefix(address: Ipv6Addr, prefix_len: u8) -> Option<Route> {
    if address.is_unicast_link_local() || address.is_loopback() || address.is_multicast() {
        None
    } else {
        Some(Route {
            prefix: ipv6_to_u128(address),
            prefix_len,
        })
    }
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
    mtu: u32,
) -> io::Result<()> {
    let ipv6_nat = config
        .ipv6_nat
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 NAT config"))?;
    let fd = create_send_socket(&config.downstream, config.reply_mark, router)?;
    let destination =
        target.unwrap_or_else(|| SocketAddrV6::new(ALL_NODES, 0, 0, router.interface_index));
    let packet = make_current_ra_packet(ipv6_nat.gateway, ipv6_nat.prefix_len, mtu);
    sendto_all(&fd, &packet, SockAddr::from(destination)).await
}

async fn send_zero_lifetime_ra(
    socket: &Socket,
    router: Router,
    prefix: Route,
    keep_router: bool,
    mtu: u32,
) -> io::Result<()> {
    let destination = SocketAddrV6::new(ALL_NODES, 0, 0, router.interface_index);
    let packet = make_zero_lifetime_ra_packet(prefix, mtu, keep_router);
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
