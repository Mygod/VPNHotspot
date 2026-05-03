use std::collections::{hash_map::Entry, HashMap};
use std::future::pending;
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::{Duration, Instant};

use libc::{
    c_void, iovec, msghdr, recvmsg, sockaddr_in6, socklen_t, CMSG_DATA, CMSG_FIRSTHDR, CMSG_NXTHDR,
    IPPROTO_IPV6, IPV6_RECVORIGDSTADDR, MSG_DONTWAIT,
};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::io::unix::AsyncFd;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep_until, Instant as TokioInstant};
use tokio::{select, spawn};
use tokio_util::sync::CancellationToken;

use crate::dns::{resolve_or_error, DNS_PORT};
use crate::upstream::connect_udp;
use vpnhotspotd::shared::model::{select_network, SessionConfig};

const UDP_ASSOC_IDLE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct AssociationKey {
    client: SocketAddrV6,
    destination: SocketAddrV6,
}

struct UdpAssociation {
    id: u64,
    socket: Arc<TokioUdpSocket>,
    last_active: Instant,
    stop: CancellationToken,
}

enum UdpAssociationEvent {
    Active(AssociationKey, u64),
    Closed(AssociationKey, u64),
}

pub(crate) fn spawn_loop(
    listener: UdpSocket,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
) -> io::Result<()> {
    let listener = AsyncFd::new(listener)?;
    spawn(async move {
        let listener_fd = listener.get_ref().as_raw_fd();
        let mut associations = HashMap::<AssociationKey, UdpAssociation>::new();
        let (association_event_tx, mut association_event_rx) = mpsc::unbounded_channel();
        let mut next_association_id = 0u64;
        let mut buffer = [0u8; 65535];
        let handle_association_event =
            |associations: &mut HashMap<AssociationKey, UdpAssociation>, event| match event {
                UdpAssociationEvent::Active(key, id) => {
                    if let Some(association) = associations.get_mut(&key) {
                        if association.id == id {
                            association.last_active = Instant::now();
                        }
                    }
                }
                UdpAssociationEvent::Closed(key, id) => {
                    let remove = associations
                        .get(&key)
                        .is_some_and(|association| association.id == id);
                    if remove {
                        if let Some(association) = associations.remove(&key) {
                            association.stop.cancel();
                        }
                    }
                }
            };
        loop {
            while let Ok(event) = association_event_rx.try_recv() {
                handle_association_event(&mut associations, event);
            }
            let now = Instant::now();
            associations.retain(|_, association| {
                let active = now.duration_since(association.last_active) < UDP_ASSOC_IDLE;
                if !active {
                    association.stop.cancel();
                }
                active
            });
            let next_expiry = associations
                .values()
                .map(|association| association.last_active + UDP_ASSOC_IDLE)
                .min();

            select! {
                _ = stop.cancelled() => break,
                _ = async {
                    if let Some(deadline) = next_expiry {
                        sleep_until(TokioInstant::from_std(deadline)).await;
                    } else {
                        pending::<()>().await;
                    }
                } => {}
                event = association_event_rx.recv() => match event {
                    Some(event) => handle_association_event(&mut associations, event),
                    None => break,
                },
                ready = listener.readable() => {
                    let Ok(mut ready) = ready else {
                        break;
                    };
                    loop {
                        match recv_packet(listener_fd, &mut buffer) {
                            Ok((size, client, destination)) => {
                                let activity = Instant::now();
                                let snapshot = config.lock().await.clone();
                                let Some(ipv6_nat) = snapshot.ipv6_nat.as_ref() else {
                                    continue;
                                };
                                if destination.ip() == &ipv6_nat.gateway && destination.port() == DNS_PORT {
                                    let query = buffer[..size].to_vec();
                                    let query_stop = stop.child_token();
                                    spawn(async move {
                                        select! {
                                            _ = query_stop.cancelled() => {}
                                            response = resolve_or_error(&snapshot, &query) => {
                                                if let Some(response) = response {
                                                    if let Err(e) = send_response(
                                                        destination,
                                                        client,
                                                        snapshot.reply_mark,
                                                        &response,
                                                    ).await {
                                                        eprintln!("dns udp response failed: {e}");
                                                    }
                                                }
                                            }
                                        }
                                    });
                                    continue;
                                }
                                if destination.ip().is_multicast()
                                    || destination.ip().is_unicast_link_local()
                                    || destination.ip().is_loopback()
                                    || destination.ip().is_unspecified()
                                {
                                    continue;
                                }
                                let network = match select_network(&snapshot, *destination.ip()) {
                                    Some(network) => network,
                                    None => continue,
                                };
                                let key = AssociationKey { client, destination };
                                let association = match associations.entry(key) {
                                    Entry::Occupied(entry) => entry.into_mut(),
                                    Entry::Vacant(entry) => {
                                        let upstream = match connect_udp(network, destination).await {
                                            Ok(socket) => Arc::new(socket),
                                            Err(e) => {
                                                eprintln!("udp connect failed: {e}");
                                                continue;
                                            }
                                        };
                                        let association_stop = stop.child_token();
                                        let association_id = next_association_id;
                                        next_association_id = next_association_id.wrapping_add(1);
                                        spawn(run_association(
                                            key,
                                            association_id,
                                            upstream.clone(),
                                            config.clone(),
                                            association_stop.clone(),
                                            association_event_tx.clone(),
                                        ));
                                        entry.insert(UdpAssociation {
                                            id: association_id,
                                            socket: upstream,
                                            last_active: activity,
                                            stop: association_stop,
                                        })
                                    }
                                };
                                association.last_active = activity;
                                if let Err(e) = association.socket.send(&buffer[..size]).await {
                                    eprintln!("udp send failed: {e}");
                                    if let Some(association) = associations.remove(&key) {
                                        association.stop.cancel();
                                    }
                                }
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                ready.clear_ready();
                                break;
                            }
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => {
                                eprintln!("udp recv failed: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        }
        for association in associations.values() {
            association.stop.cancel();
        }
    });
    Ok(())
}

async fn run_association(
    key: AssociationKey,
    id: u64,
    socket: Arc<TokioUdpSocket>,
    config: Arc<Mutex<SessionConfig>>,
    stop: CancellationToken,
    association_event_tx: mpsc::UnboundedSender<UdpAssociationEvent>,
) {
    let mut buffer = [0u8; 65535];
    loop {
        select! {
            _ = stop.cancelled() => break,
            result = socket.recv(&mut buffer) => match result {
                Ok(size) => {
                    let mark = config.lock().await.reply_mark;
                    if let Err(e) = send_response(key.destination, key.client, mark, &buffer[..size]).await {
                        eprintln!("udp response failed: {e}");
                        break;
                    }
                    let _ = association_event_tx.send(UdpAssociationEvent::Active(key, id));
                }
                Err(e) => {
                    eprintln!("udp upstream recv failed: {e}");
                    break;
                }
            }
        }
    }
    stop.cancel();
    let _ = association_event_tx.send(UdpAssociationEvent::Closed(key, id));
}

async fn send_response(
    source: SocketAddrV6,
    target: SocketAddrV6,
    mark: u32,
    payload: &[u8],
) -> io::Result<()> {
    let socket = TokioUdpSocket::from_std(create_reply_socket(source, mark)?)?;
    socket.send_to(payload, SocketAddr::V6(target)).await?;
    Ok(())
}

fn create_reply_socket(source: SocketAddrV6, mark: u32) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    socket.set_mark(mark)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(source))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

fn socket_addr_v6_from_raw(address: sockaddr_in6) -> SocketAddrV6 {
    SocketAddrV6::new(
        Ipv6Addr::from(address.sin6_addr.s6_addr),
        u16::from_be(address.sin6_port),
        address.sin6_flowinfo,
        address.sin6_scope_id,
    )
}

fn recv_packet(fd: i32, buffer: &mut [u8]) -> io::Result<(usize, SocketAddrV6, SocketAddrV6)> {
    let mut source: sockaddr_in6 = unsafe { zeroed() };
    let source_len = size_of::<sockaddr_in6>() as socklen_t;
    let mut control = [0u8; 128];
    let mut iov = iovec {
        iov_base: buffer.as_mut_ptr() as *mut c_void,
        iov_len: buffer.len(),
    };
    let mut message = msghdr {
        msg_name: &mut source as *mut _ as *mut c_void,
        msg_namelen: source_len,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr() as *mut c_void,
        msg_controllen: control.len(),
        msg_flags: 0,
    };
    let size = unsafe { recvmsg(fd, &mut message, MSG_DONTWAIT) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    let source = socket_addr_v6_from_raw(source);
    let mut destination = None;
    let mut current = unsafe { CMSG_FIRSTHDR(&message) };
    while !current.is_null() {
        unsafe {
            if (*current).cmsg_level == IPPROTO_IPV6 && (*current).cmsg_type == IPV6_RECVORIGDSTADDR
            {
                let raw = CMSG_DATA(current) as *const sockaddr_in6;
                destination = Some(socket_addr_v6_from_raw(*raw));
                break;
            }
            current = CMSG_NXTHDR(&message, current);
        }
    }
    let destination = destination.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing original destination")
    })?;
    Ok((size as usize, source, destination))
}
