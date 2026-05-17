use std::collections::HashSet;
use std::io;
use std::net::IpAddr;

use futures_util::{pin_mut, TryStreamExt};
use rtnetlink::packet_route::{address::AddressAttribute, AddressFamily};

use crate::{netlink, report};
use vpnhotspotd::shared::model::{
    ipv6_nat_gateway, ipv6_nat_prefix, DAEMON_TABLE, LOCAL_NETWORK_TABLE,
};
use vpnhotspotd::shared::proto::daemon::{CleanRoutingCommand, ReplaceStaticAddressesCommand};
use vpnhotspotd::shared::protocol::{error_errno, read_ip_address_entry, IoResultReportExt};

use super::netlink_commands::{
    address_details, apply_address_command, apply_address_command_with_index, apply_route_command,
    flush_routes, is_missing, is_missing_address, route_details, IpAddressCommand, IpOperation,
    IpRouteCommand, RouteType,
};

pub(crate) async fn replace_static_addresses(
    handle: &netlink::Handle,
    command: &ReplaceStaticAddressesCommand,
) -> io::Result<()> {
    let index = netlink::link_index(handle, &command.dev)
        .await
        .with_report_context_details(
            "routing.static_addresses.link_index",
            [("dev", command.dev.clone())],
        )?;
    let requested_addresses = command
        .addresses
        .iter()
        .map(read_ip_address_entry)
        .collect::<io::Result<Vec<_>>>()?;
    let requested = requested_addresses.iter().copied().collect::<HashSet<_>>();
    for (address, prefix_len) in requested_addresses {
        let address_command = IpAddressCommand {
            operation: IpOperation::Replace,
            address,
            prefix_len,
            interface: command.dev.clone(),
        };
        match apply_address_command_with_index(handle, &address_command, index).await {
            Ok(()) => {}
            Err(e) if error_errno(&e) == Some(libc::EEXIST) => {}
            Err(e) => return Err(e),
        }
    }
    let _dump = handle.lock_dump().await;
    let addresses = handle
        .raw()
        .address()
        .get()
        .set_link_index_filter(index)
        .execute();
    pin_mut!(addresses);
    while let Some(message) = addresses
        .try_next()
        .await
        .map_err(netlink::to_io_error)
        .with_report_context_details(
            "routing.static_addresses.dump",
            [("dev", command.dev.clone())],
        )?
    {
        let mut address = None;
        for attribute in &message.attributes {
            match attribute {
                AddressAttribute::Local(value) => {
                    address = Some(*value);
                    break;
                }
                AddressAttribute::Address(value) if address.is_none() => {
                    address = Some(*value);
                }
                _ => {}
            }
        }
        let Some(address) = address else {
            continue;
        };
        let prefix_len = message.header.prefix_len;
        if address.is_loopback() || requested.contains(&(address, prefix_len)) {
            continue;
        }
        let address_command = IpAddressCommand {
            operation: IpOperation::Delete,
            address,
            prefix_len,
            interface: command.dev.clone(),
        };
        match apply_address_command_with_index(handle, &address_command, index).await {
            Ok(()) => {}
            Err(e) if is_missing_address(&e) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

pub(super) async fn clean_ip(
    handle: &netlink::Handle,
    command: &CleanRoutingCommand,
) -> io::Result<()> {
    flush_routes(handle, AddressFamily::Inet6, DAEMON_TABLE).await?;
    for interface in netlink::link_names(handle).await?.into_values() {
        let prefix = ipv6_nat_prefix(&command.ipv6_nat_prefix_seed, &interface);
        let gateway = ipv6_nat_gateway(prefix);
        let address = IpAddressCommand {
            operation: IpOperation::Delete,
            address: IpAddr::V6(gateway.address()),
            prefix_len: gateway.network_length(),
            interface: interface.clone(),
        };
        if let Err(e) = apply_address_command(handle, &address).await {
            if !is_missing_address(&e) {
                report::io_with_details("routing.clean_ip.address", e, address_details(&address));
            }
        }
        let route = IpRouteCommand {
            operation: IpOperation::Delete,
            route_type: RouteType::Unicast,
            destination: IpAddr::V6(prefix.first_address()),
            prefix_len: prefix.network_length(),
            interface,
            table: LOCAL_NETWORK_TABLE,
        };
        if let Err(e) = apply_route_command(handle, &route).await {
            if !is_missing(&e) {
                report::io_with_details("routing.clean_ip.route", e, route_details(&route));
            }
        }
    }
    Ok(())
}
