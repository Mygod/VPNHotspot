//! IPv6 destinations that NAT66 leaves to the kernel instead of intercepting.

use std::net::{IpAddr, Ipv6Addr};

use cidr::{Ipv6Cidr, Ipv6Inet};
use rtnetlink::packet_route::{
    address::{AddressAttribute, AddressFlags, AddressMessage},
    AddressFamily,
};

/// Destination prefixes that every NAT66 session returns before interception, besides its own
/// gateway prefix: link-local unicast, multicast, and the unspecified and loopback addresses.
pub const SPECIAL_DESTINATIONS: [(Ipv6Addr, u8); 3] = [
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    (Ipv6Addr::UNSPECIFIED, 127),
];

/// Collects the distinct addresses that an IPv6 `RTM_GETADDR` dump reports on any interface,
/// sorted, leaving out those inside [`SPECIAL_DESTINATIONS`].
///
/// An address that is still tentative without being optimistic, or whose duplicate address
/// detection failed, is left out because the kernel does not deliver traffic to it yet. Linux
/// inserts an address's local route when DAD completes, or at DAD start for an optimistic address
/// (`addrconf_dad_begin` and `__ipv6_ifa_notify` in `net/ipv6/addrconf.c`), and
/// `addrconf_dad_stop` keeps a failed address only as tentative and DAD-failed. Such an address
/// may belong to another node on its link, so traffic to it keeps its upstream path. DAD
/// completion sends `RTM_NEWADDR`, which brings the address in.
///
/// A point-to-point address carries the local address in `IFA_LOCAL` and the peer in
/// `IFA_ADDRESS` (`inet6_fill_ifaddr`), so `IFA_LOCAL` wins when both are present.
pub fn local_ipv6_addresses(messages: &[AddressMessage]) -> Vec<Ipv6Addr> {
    let special = SPECIAL_DESTINATIONS.map(|(address, prefix_len)| {
        Ipv6Cidr::new(address, prefix_len).expect("special IPv6 destination prefix is valid")
    });
    let mut addresses = Vec::new();
    for message in messages {
        if message.header.family != AddressFamily::Inet6 {
            continue;
        }
        let mut flags = AddressFlags::from_bits_retain(message.header.flags.bits().into());
        let mut local = None;
        let mut fallback = None;
        for attribute in &message.attributes {
            match attribute {
                AddressAttribute::Flags(value) => flags = *value,
                AddressAttribute::Local(IpAddr::V6(address)) => local = Some(*address),
                AddressAttribute::Address(IpAddr::V6(address)) => fallback = Some(*address),
                _ => {}
            }
        }
        if flags.contains(AddressFlags::Dadfailed)
            || flags.contains(AddressFlags::Tentative) && !flags.contains(AddressFlags::Optimistic)
        {
            continue;
        }
        if let Some(address) = local.or(fallback) {
            if !special.iter().any(|prefix| prefix.contains(&address)) {
                addresses.push(address);
            }
        }
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

/// Selects the addresses from [`local_ipv6_addresses`] that need their own exemption in a NAT66
/// session: those outside the session's gateway prefix, which its prefix return already covers.
pub fn exempt_ipv6_addresses(
    addresses: &[Ipv6Addr],
    gateway: Ipv6Inet,
) -> impl Iterator<Item = Ipv6Addr> + '_ {
    addresses
        .iter()
        .copied()
        .filter(move |address| !gateway.contains(address))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtnetlink::packet_route::address::AddressHeaderFlags;

    #[test]
    fn keeps_usable_global_and_unique_local_addresses_sorted_and_distinct() {
        let addresses = local_ipv6_addresses(&[
            message(AddressFlags::empty(), [address("fd00::2")]),
            message(AddressFlags::Permanent, [address("2001:db8::1")]),
            message(AddressFlags::Deprecated, [address("2001:db8::3")]),
            // The same address on a second interface yields one exemption.
            message(AddressFlags::empty(), [address("2001:db8::1")]),
        ]);
        assert_eq!(
            addresses,
            [ip("2001:db8::1"), ip("2001:db8::3"), ip("fd00::2")]
        );
    }

    #[test]
    fn leaves_out_addresses_already_covered_by_special_destinations() {
        let addresses = local_ipv6_addresses(&[
            message(AddressFlags::Permanent, [address("::1")]),
            message(AddressFlags::Permanent, [address("fe80::1")]),
            message(AddressFlags::Permanent, [address("febf::1")]),
            message(AddressFlags::Permanent, [address("ff02::1")]),
            message(AddressFlags::Permanent, [address("fec0::1")]),
        ]);
        assert_eq!(addresses, [ip("fec0::1")]);
    }

    #[test]
    fn leaves_out_addresses_the_kernel_does_not_deliver_yet() {
        let addresses = local_ipv6_addresses(&[
            message(AddressFlags::Tentative, [address("2001:db8::1")]),
            message(
                AddressFlags::Tentative | AddressFlags::Dadfailed,
                [address("2001:db8::2")],
            ),
            message(
                AddressFlags::Tentative | AddressFlags::Optimistic,
                [address("2001:db8::3")],
            ),
        ]);
        assert_eq!(addresses, [ip("2001:db8::3")]);
    }

    #[test]
    fn reads_header_flags_when_the_flags_attribute_is_absent() {
        let header_only = |flags, text| {
            let mut message = AddressMessage::default();
            message.header.family = AddressFamily::Inet6;
            message.header.flags = flags;
            message.attributes.push(address(text));
            message
        };
        assert_eq!(
            local_ipv6_addresses(&[
                header_only(AddressHeaderFlags::Tentative, "2001:db8::1"),
                header_only(AddressHeaderFlags::Permanent, "2001:db8::2"),
            ]),
            [ip("2001:db8::2")]
        );
    }

    #[test]
    fn prefers_the_local_attribute_of_a_point_to_point_address() {
        let addresses = local_ipv6_addresses(&[message(
            AddressFlags::Permanent,
            [
                address("2001:db8::2"),
                AddressAttribute::Local(IpAddr::V6(ip("2001:db8::1"))),
            ],
        )]);
        assert_eq!(addresses, [ip("2001:db8::1")]);
    }

    #[test]
    fn ignores_ipv4_addresses() {
        let mut message = AddressMessage::default();
        message.header.family = AddressFamily::Inet;
        message
            .attributes
            .push(AddressAttribute::Address("192.0.2.1".parse().unwrap()));
        assert!(local_ipv6_addresses(&[message]).is_empty());
    }

    #[test]
    fn exempts_only_addresses_outside_the_gateway_prefix() {
        let gateway = Ipv6Inet::new(ip("fd47:6b7c:2186:b452::1"), 64).unwrap();
        let addresses = [
            ip("2001:db8::1"),
            ip("fd47:6b7c:2186:b452::1"),
            ip("fd47:6b7c:2186:b452::2"),
            ip("fd47:6b7c:2186:b453::1"),
        ];
        assert_eq!(
            exempt_ipv6_addresses(&addresses, gateway).collect::<Vec<_>>(),
            [ip("2001:db8::1"), ip("fd47:6b7c:2186:b453::1")]
        );
    }

    fn ip(address: &str) -> Ipv6Addr {
        address.parse().unwrap()
    }

    fn address(address: &str) -> AddressAttribute {
        AddressAttribute::Address(IpAddr::V6(ip(address)))
    }

    fn message<const N: usize>(
        flags: AddressFlags,
        attributes: [AddressAttribute; N],
    ) -> AddressMessage {
        let mut message = AddressMessage::default();
        message.header.family = AddressFamily::Inet6;
        message.header.prefix_len = 64;
        message.attributes.push(AddressAttribute::Flags(flags));
        message.attributes.extend(attributes);
        message
    }
}
