use std::net::{IpAddr, Ipv4Addr};

use rtnetlink::packet_route::{
    address::{AddressAttribute, AddressMessage},
    AddressFamily,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownstreamIpv4 {
    pub address: Ipv4Addr,
    pub prefix_len: u8,
}

pub fn select_ipv4_address(
    messages: &[AddressMessage],
) -> Result<Option<DownstreamIpv4>, Vec<DownstreamIpv4>> {
    let addresses: Vec<_> = messages.iter().filter_map(message_ipv4_address).collect();
    match addresses.as_slice() {
        [] => Ok(None),
        [address] => Ok(Some(*address)),
        _ => Err(addresses),
    }
}

fn message_ipv4_address(message: &AddressMessage) -> Option<DownstreamIpv4> {
    if message.header.family != AddressFamily::Inet {
        return None;
    }
    let mut fallback = None;
    for attribute in &message.attributes {
        match attribute {
            AddressAttribute::Local(IpAddr::V4(address)) => {
                return Some(DownstreamIpv4 {
                    address: *address,
                    prefix_len: message.header.prefix_len,
                });
            }
            AddressAttribute::Address(IpAddr::V4(address)) => {
                fallback = Some(DownstreamIpv4 {
                    address: *address,
                    prefix_len: message.header.prefix_len,
                });
            }
            _ => {}
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_ipv4_address_returns_none_without_ipv4_address() {
        assert_eq!(select_ipv4_address(&[]), Ok(None));
        assert_eq!(
            select_ipv4_address(&[address_message(
                AddressFamily::Inet6,
                64,
                [AddressAttribute::Address("2001:db8::1".parse().unwrap())],
            )]),
            Ok(None)
        );
    }

    #[test]
    fn select_ipv4_address_returns_single_ipv4_address() {
        assert_eq!(
            select_ipv4_address(&[address_message(
                AddressFamily::Inet,
                24,
                [AddressAttribute::Address(IpAddr::V4(Ipv4Addr::new(
                    192, 0, 2, 1
                )))],
            )]),
            Ok(Some(DownstreamIpv4 {
                address: Ipv4Addr::new(192, 0, 2, 1),
                prefix_len: 24,
            }))
        );
    }

    #[test]
    fn select_ipv4_address_rejects_multiple_ipv4_addresses() {
        let addresses = match select_ipv4_address(&[
            address_message(
                AddressFamily::Inet,
                24,
                [AddressAttribute::Address(IpAddr::V4(Ipv4Addr::new(
                    192, 0, 2, 1,
                )))],
            ),
            address_message(
                AddressFamily::Inet,
                24,
                [AddressAttribute::Address(IpAddr::V4(Ipv4Addr::new(
                    192, 0, 2, 2,
                )))],
            ),
        ]) {
            Err(addresses) => addresses,
            Ok(_) => panic!("expected multiple-address failure"),
        };
        assert_eq!(
            addresses,
            vec![
                DownstreamIpv4 {
                    address: Ipv4Addr::new(192, 0, 2, 1),
                    prefix_len: 24,
                },
                DownstreamIpv4 {
                    address: Ipv4Addr::new(192, 0, 2, 2),
                    prefix_len: 24,
                },
            ]
        );
    }

    #[test]
    fn select_ipv4_address_prefers_local_over_address_attribute() {
        assert_eq!(
            select_ipv4_address(&[address_message(
                AddressFamily::Inet,
                30,
                [
                    AddressAttribute::Address(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
                    AddressAttribute::Local(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2))),
                ],
            )]),
            Ok(Some(DownstreamIpv4 {
                address: Ipv4Addr::new(192, 0, 2, 2),
                prefix_len: 30,
            }))
        );
    }

    fn address_message<const N: usize>(
        family: AddressFamily,
        prefix_len: u8,
        attributes: [AddressAttribute; N],
    ) -> AddressMessage {
        let mut message = AddressMessage::default();
        message.header.family = family;
        message.header.prefix_len = prefix_len;
        message.attributes.extend(attributes);
        message
    }
}
