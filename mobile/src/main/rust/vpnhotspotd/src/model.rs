use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub(crate) struct Route {
    pub(crate) prefix: u128,
    pub(crate) prefix_len: u8,
}

#[derive(Clone)]
pub(crate) struct Upstream {
    pub(crate) network_handle: u64,
    pub(crate) interface: String,
    pub(crate) routes: Vec<Route>,
}

#[derive(Clone)]
pub(crate) struct SessionConfig {
    pub(crate) session_id: String,
    pub(crate) downstream: String,
    pub(crate) router: Ipv6Addr,
    pub(crate) gateway: Ipv6Addr,
    pub(crate) prefix_len: u8,
    pub(crate) reply_mark: u32,
    pub(crate) dns_bind_address: Ipv4Addr,
    pub(crate) mtu: u32,
    pub(crate) suppressed_prefixes: Vec<Route>,
    pub(crate) cleanup_prefixes: Vec<Route>,
    pub(crate) primary: Option<Upstream>,
    pub(crate) fallback: Option<Upstream>,
}

#[derive(Clone, Copy)]
pub(crate) struct SessionPorts {
    pub(crate) tcp: u16,
    pub(crate) udp: u16,
    pub(crate) dns_tcp: u16,
    pub(crate) dns_udp: u16,
}

pub(crate) fn network_prefix(address: Ipv6Addr, prefix_len: u8) -> [u8; 16] {
    let shift = 128u32.saturating_sub(prefix_len as u32);
    (ipv6_to_u128(address) & (!0u128 << shift)).to_be_bytes()
}

pub(crate) fn ipv6_to_u128(address: Ipv6Addr) -> u128 {
    u128::from_be_bytes(address.octets())
}
