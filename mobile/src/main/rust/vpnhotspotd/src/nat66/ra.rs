use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io;
use std::mem::{size_of, take, MaybeUninit};
use std::net::{Ipv6Addr, SocketAddrV6};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libc::{if_nametoindex, MSG_DONTWAIT};
use linux_raw_sys::netlink::{sockaddr_nl, NETLINK_ROUTE, RTMGRP_IPV6_IFADDR};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, Notify};
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio::{select, spawn, task::JoinHandle};
use tokio_util::sync::CancellationToken;

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

pub(crate) fn spawn_loop(
    config: Arc<Mutex<SessionConfig>>,
    config_changed: Arc<Notify>,
    stop: CancellationToken,
    initial: &SessionConfig,
) -> io::Result<JoinHandle<()>> {
    let socket = AsyncFd::new(create_recv_socket(&initial.downstream, initial.reply_mark)?)?;
    Ok(spawn(async move {
        let mut address_monitor = match AddressMonitor::new() {
            Ok(monitor) => Some(monitor),
            Err(e) => {
                eprintln!("ra address monitor failed: {e}");
                None
            }
        };
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
            let router = match link_local_router(&current.downstream) {
                Ok(router) => router,
                Err(e) => {
                    eprintln!("ra link-local lookup failed on {}: {e}", current.downstream);
                    None
                }
            };
            let router_changed = router != last_router;
            if router_changed {
                last_router = router;
                if let Some(router) = router {
                    eprintln!(
                        "ra using link-local router address {router} on {}",
                        current.downstream
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
                    let _ = send_ra(&current, router, None).await;
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
                address_event = async {
                    match address_monitor.as_ref() {
                        Some(monitor) => monitor.wait().await,
                        None => std::future::pending::<io::Result<()>>().await,
                    }
                } => {
                    if let Err(e) = address_event {
                        eprintln!("ra address monitor failed: {e}");
                        address_monitor = None;
                    } else {
                        address_changed = true;
                    }
                }
                ready = socket.readable() => {
                    let Ok(mut ready) = ready else {
                        break;
                    };
                    loop {
                        match recv_request(socket.get_ref(), &mut buffer) {
                            Ok(RaRequest::RouterSolicitation(source)) => {
                                if let Some(router) = router {
                                    let _ = send_ra(&current, router, Some(source)).await;
                                }
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
    }))
}

pub(crate) async fn withdraw_prefixes_once(
    config: &SessionConfig,
    prefixes: &[Route],
    keep_router: bool,
) {
    if config.ipv6_nat.is_none() {
        return;
    }
    let router = match link_local_router(&config.downstream) {
        Ok(Some(router)) => router,
        Ok(None) => {
            eprintln!(
                "ra withdraw skipped: missing link-local router address on {}",
                config.downstream
            );
            return;
        }
        Err(e) => {
            eprintln!("ra link-local lookup failed on {}: {e}", config.downstream);
            return;
        }
    };
    withdraw_prefixes_once_with_router(config, prefixes, keep_router, router).await;
}

async fn withdraw_prefixes_once_with_router(
    config: &SessionConfig,
    prefixes: &[Route],
    keep_router: bool,
    router: Ipv6Addr,
) {
    if config.ipv6_nat.is_none() {
        return;
    }
    let fd = match create_send_socket(&config.downstream, config.reply_mark, router) {
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

fn link_local_router(interface: &str) -> io::Result<Option<Ipv6Addr>> {
    let interface = CString::new(interface)?;
    let mut addresses = ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addresses) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let addresses = NonNull::new(addresses);
    let _addresses = IfAddrs(addresses);
    let mut current = addresses;
    while let Some(ifaddr) = current {
        let ifaddr = unsafe { ifaddr.as_ref() };
        if let (Some(name), Some(address)) =
            (NonNull::new(ifaddr.ifa_name), NonNull::new(ifaddr.ifa_addr))
        {
            let name = unsafe { CStr::from_ptr(name.as_ptr()) };
            let address = unsafe { address.as_ref() };
            if name == interface.as_c_str() && address.sa_family as i32 == libc::AF_INET6 {
                let address = unsafe { &*(address as *const _ as *const libc::sockaddr_in6) };
                let address = Ipv6Addr::from(address.sin6_addr.s6_addr);
                if is_router_link_local(address) {
                    return Ok(Some(address));
                }
            }
        }
        current = NonNull::new(ifaddr.ifa_next);
    }
    Ok(None)
}

struct IfAddrs(Option<NonNull<libc::ifaddrs>>);

impl Drop for IfAddrs {
    fn drop(&mut self) {
        if let Some(addresses) = self.0 {
            unsafe { libc::freeifaddrs(addresses.as_ptr()) };
        }
    }
}

struct AddressMonitor {
    socket: AsyncFd<OwnedFd>,
}

impl AddressMonitor {
    fn new() -> io::Result<Self> {
        let fd = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                NETLINK_ROUTE as libc::c_int,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let address = sockaddr_nl {
            nl_family: libc::AF_NETLINK as _,
            nl_pad: 0,
            nl_pid: 0,
            nl_groups: RTMGRP_IPV6_IFADDR,
        };
        if unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &address as *const _ as *const libc::sockaddr,
                size_of::<sockaddr_nl>() as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            socket: AsyncFd::new(fd)?,
        })
    }

    async fn wait(&self) -> io::Result<()> {
        let mut buffer = [0u8; 4096];
        loop {
            let mut ready = self.socket.readable().await?;
            let mut changed = false;
            loop {
                let result = unsafe {
                    libc::recv(
                        self.socket.get_ref().as_raw_fd(),
                        buffer.as_mut_ptr() as *mut libc::c_void,
                        buffer.len(),
                        libc::MSG_DONTWAIT,
                    )
                };
                if result > 0 {
                    changed = true;
                    continue;
                }
                if result == 0 {
                    ready.clear_ready();
                    return Ok(());
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() == io::ErrorKind::WouldBlock {
                    ready.clear_ready();
                    if changed {
                        return Ok(());
                    }
                    break;
                }
                return Err(error);
            }
        }
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

async fn send_ra(
    config: &SessionConfig,
    router: Ipv6Addr,
    target: Option<SocketAddrV6>,
) -> io::Result<()> {
    let ipv6_nat = config
        .ipv6_nat
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing ipv6 NAT config"))?;
    let fd = create_send_socket(&config.downstream, config.reply_mark, router)?;
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
