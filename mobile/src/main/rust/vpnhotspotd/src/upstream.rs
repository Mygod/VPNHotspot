use std::io;
use std::mem::size_of_val;
use std::net::{Ipv6Addr, SocketAddrV6, TcpListener, UdpSocket};
use std::os::fd::AsRawFd;
use std::time::Duration;

use libc::{c_int, c_void, setsockopt, socklen_t, EINPROGRESS, IPPROTO_IPV6, IPV6_RECVORIGDSTADDR};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use tokio::net::{TcpStream as TokioTcpStream, UdpSocket as TokioUdpSocket};

use crate::model::{ipv6_to_u128, Network, Route, SessionConfig};
use crate::socket::await_connect;

const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(target_os = "android")]
#[link(name = "android")]
unsafe extern "C" {
    fn android_setsocknetwork(network: u64, fd: c_int) -> c_int;
}

pub(crate) fn select_network(config: &SessionConfig, destination: Ipv6Addr) -> Option<Network> {
    let destination = ipv6_to_u128(destination);
    if config.primary_network.is_some() && route_matches(&config.primary_routes, destination) {
        config.primary_network
    } else {
        config.fallback_network
    }
}

pub(crate) fn create_tproxy_tcp_listener(mark: u32) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_mark(mark)?;
    socket.set_only_v6(true)?;
    socket.set_ip_transparent_v6(true)?;
    socket.bind(&SockAddr::from(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    )))?;
    socket.listen(32)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

pub(crate) fn create_tproxy_udp_listener(mark: u32) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, None)?;
    socket.set_reuse_address(true)?;
    socket.set_mark(mark)?;
    socket.set_only_v6(true)?;
    socket.set_ip_transparent_v6(true)?;
    let one = 1;
    if unsafe {
        setsockopt(
            socket.as_raw_fd(),
            IPPROTO_IPV6,
            IPV6_RECVORIGDSTADDR,
            &one as *const _ as *const c_void,
            size_of_val(&one) as socklen_t,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    socket.bind(&SockAddr::from(SocketAddrV6::new(
        Ipv6Addr::UNSPECIFIED,
        0,
        0,
        0,
    )))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

pub(crate) async fn connect_tcp(
    network: Network,
    destination: SocketAddrV6,
) -> io::Result<TokioTcpStream> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.set_nonblocking(true)?;
    if let Err(error) = socket.connect(&SockAddr::from(destination)) {
        let raw_os_error = error.raw_os_error();
        if error.kind() != io::ErrorKind::WouldBlock && raw_os_error != Some(EINPROGRESS) {
            return Err(error);
        }
        await_connect(&socket, TCP_CONNECT_TIMEOUT).await?;
    }
    TokioTcpStream::from_std(socket.into())
}

pub(crate) async fn connect_udp(
    network: Network,
    destination: SocketAddrV6,
) -> io::Result<TokioUdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    set_socket_network(network, socket.as_raw_fd())?;
    socket.connect(&SockAddr::from(destination))?;
    socket.set_nonblocking(true)?;
    TokioUdpSocket::from_std(socket.into())
}

#[cfg(target_os = "android")]
fn set_socket_network(network: Network, fd: c_int) -> io::Result<()> {
    if unsafe { android_setsocknetwork(network, fd) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "android"))]
fn set_socket_network(_network: Network, _fd: c_int) -> io::Result<()> {
    Ok(())
}

fn route_matches(routes: &[Route], destination: u128) -> bool {
    routes
        .iter()
        .any(|route| prefix_matches(destination, route.prefix, route.prefix_len))
}

fn prefix_matches(destination: u128, prefix: u128, prefix_len: u8) -> bool {
    if prefix_len == 0 {
        true
    } else {
        let shift = 128 - prefix_len as u32;
        destination >> shift == prefix >> shift
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::model::SessionConfig;

    #[test]
    fn primary_route_match_selects_primary_network() {
        let config = config(Some(123), vec![route("2001:db8::", 32)], Some(456));
        assert_eq!(
            select_network(&config, "2001:db8:1::1".parse().unwrap()),
            Some(123)
        );
    }

    #[test]
    fn non_primary_destination_selects_fallback_network() {
        let config = config(Some(123), vec![route("2001:db8::", 32)], Some(456));
        assert_eq!(
            select_network(&config, "fd00::1".parse().unwrap()),
            Some(456)
        );
    }

    #[test]
    fn missing_fallback_network_returns_none() {
        let config = config(Some(123), vec![route("2001:db8::", 32)], None);
        assert_eq!(select_network(&config, "fd00::1".parse().unwrap()), None);
    }

    #[test]
    fn primary_absent_selects_fallback_network() {
        let config = config(None, vec![route("::", 0)], Some(456));
        assert_eq!(
            select_network(&config, "2001:db8::1".parse().unwrap()),
            Some(456)
        );
    }

    fn config(
        primary_network: Option<Network>,
        primary_routes: Vec<Route>,
        fallback_network: Option<Network>,
    ) -> SessionConfig {
        SessionConfig {
            downstream: "wlan0".to_string(),
            dns_bind_address: Ipv4Addr::new(192, 0, 2, 1),
            reply_mark: 0,
            primary_network,
            primary_routes,
            fallback_network,
            ipv6_nat: None,
        }
    }

    fn route(address: &str, prefix_len: u8) -> Route {
        Route {
            prefix: ipv6_to_u128(address.parse::<Ipv6Addr>().unwrap()),
            prefix_len,
        }
    }
}
