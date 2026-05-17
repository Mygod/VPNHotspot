use std::io;
use std::net::IpAddr;

use futures_util::{pin_mut, TryStreamExt};
use rtnetlink::packet_route::{
    address::{AddressAttribute, AddressMessage},
    route::{
        RouteAttribute, RouteHeader, RouteMessage, RouteProtocol, RouteScope,
        RouteType as NetlinkRouteType,
    },
    rule::{RuleAction as NetlinkRuleAction, RuleAttribute, RuleMessage},
    AddressFamily,
};

use crate::{netlink, report};
use vpnhotspotd::shared::protocol::{error_errno, IoResultReportExt};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IpOperation {
    Replace,
    Delete,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum IpFamily {
    Ipv4,
    Ipv6,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RouteType {
    Unicast,
    Local,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuleAction {
    Lookup,
    Unreachable,
    Any,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IpAddressCommand {
    pub(super) operation: IpOperation,
    pub(super) address: IpAddr,
    pub(super) prefix_len: u8,
    pub(super) interface: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IpRouteCommand {
    pub(super) operation: IpOperation,
    pub(super) route_type: RouteType,
    pub(super) destination: IpAddr,
    pub(super) prefix_len: u8,
    pub(super) interface: String,
    pub(super) table: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct IpRuleCommand {
    pub(super) operation: IpOperation,
    pub(super) family: IpFamily,
    pub(super) iif: String,
    pub(super) priority: u32,
    pub(super) action: RuleAction,
    pub(super) table: u32,
    pub(super) fwmark: Option<(u32, u32)>,
}

pub(super) async fn add_rule(handle: &netlink::Handle, command: IpRuleCommand) -> io::Result<()> {
    match apply_rule_command(handle, &command).await {
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

pub(super) async fn delete_rule_result(
    handle: &netlink::Handle,
    mut command: IpRuleCommand,
) -> bool {
    command.operation = IpOperation::Delete;
    match apply_rule_command(handle, &command).await {
        Ok(()) => true,
        Err(e) if is_missing(&e) => true,
        Err(e) => {
            report::io_with_details("routing.delete_rule", e, rule_details(&command));
            false
        }
    }
}

pub(super) async fn delete_rule_repeated(
    handle: &netlink::Handle,
    family: IpFamily,
    priority: u32,
) -> io::Result<()> {
    loop {
        let result = apply_rule_command(
            handle,
            &IpRuleCommand {
                operation: IpOperation::Delete,
                family,
                iif: String::new(),
                priority,
                action: RuleAction::Any,
                table: 0,
                fwmark: None,
            },
        )
        .await;
        match result {
            Ok(()) => {}
            Err(e) if is_missing(&e) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

pub(super) async fn apply_route(
    handle: &netlink::Handle,
    command: IpRouteCommand,
) -> io::Result<()> {
    match apply_route_command(handle, &command).await {
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

pub(super) async fn delete_route_result(
    handle: &netlink::Handle,
    mut command: IpRouteCommand,
) -> bool {
    command.operation = IpOperation::Delete;
    match apply_route_command(handle, &command).await {
        Ok(()) => true,
        Err(e) if is_missing(&e) => true,
        Err(e) => {
            report::io_with_details("routing.delete_route", e, route_details(&command));
            false
        }
    }
}

pub(super) async fn apply_address(
    handle: &netlink::Handle,
    command: IpAddressCommand,
) -> io::Result<()> {
    match apply_address_command(handle, &command).await {
        Err(e) if error_errno(&e) == Some(libc::EEXIST) => Ok(()),
        result => result,
    }
}

pub(super) async fn delete_address_result(
    handle: &netlink::Handle,
    mut command: IpAddressCommand,
) -> bool {
    command.operation = IpOperation::Delete;
    match apply_address_command(handle, &command).await {
        Ok(()) => true,
        Err(e) if is_missing_address(&e) => true,
        Err(e) => {
            report::io_with_details("routing.delete_address", e, address_details(&command));
            false
        }
    }
}

pub(super) async fn apply_address_command(
    handle: &netlink::Handle,
    command: &IpAddressCommand,
) -> io::Result<()> {
    let index = netlink::link_index(handle, &command.interface)
        .await
        .with_report_context_details("routing.address.link_index", address_details(command))?;
    apply_address_command_with_index(handle, command, index).await
}

pub(super) async fn apply_address_command_with_index(
    handle: &netlink::Handle,
    command: &IpAddressCommand,
    index: u32,
) -> io::Result<()> {
    match command.operation {
        IpOperation::Replace => {
            let mut request = handle
                .raw()
                .address()
                .add(index, command.address, command.prefix_len)
                .replace();
            *request.message_mut() = address_message(index, command.address, command.prefix_len);
            request.execute().await
        }
        IpOperation::Delete => {
            handle
                .raw()
                .address()
                .del(address_message(index, command.address, command.prefix_len))
                .execute()
                .await
        }
    }
    .map_err(netlink::to_io_error)
    .with_report_context_details("routing.address", address_details(command))
}

pub(super) async fn apply_route_command(
    handle: &netlink::Handle,
    command: &IpRouteCommand,
) -> io::Result<()> {
    let message = route_message(
        command,
        netlink::link_index(handle, &command.interface)
            .await
            .with_report_context_details("routing.route.link_index", route_details(command))?,
    );
    match command.operation {
        IpOperation::Replace => handle.raw().route().add(message).replace().execute().await,
        IpOperation::Delete => handle.raw().route().del(message).execute().await,
    }
    .map_err(netlink::to_io_error)
    .with_report_context_details("routing.route", route_details(command))
}

async fn apply_rule_command(handle: &netlink::Handle, command: &IpRuleCommand) -> io::Result<()> {
    match command.operation {
        IpOperation::Replace => {
            let mut request = handle.raw().rule().add();
            fill_rule_message(request.message_mut(), command)
                .with_report_context_details("routing.rule.fill", rule_details(command))?;
            request.execute().await
        }
        IpOperation::Delete => {
            handle
                .raw()
                .rule()
                .del(
                    rule_message(command)
                        .with_report_context_details("routing.rule.fill", rule_details(command))?,
                )
                .execute()
                .await
        }
    }
    .map_err(netlink::to_io_error)
    .with_report_context_details("routing.rule", rule_details(command))
}

pub(super) async fn flush_routes(
    handle: &netlink::Handle,
    family: AddressFamily,
    table: u32,
) -> io::Result<()> {
    let _dump = handle.lock_dump().await;
    let routes = handle
        .raw()
        .route()
        .get(route_dump_message(family, table))
        .execute();
    pin_mut!(routes);
    while let Some(route) = routes.try_next().await.map_err(netlink::to_io_error)? {
        if route_table(&route) == Some(table) {
            if let Err(e) = handle.raw().route().del(route).execute().await {
                let e = netlink::to_io_error(e);
                if !is_missing(&e) {
                    report::io_with_details(
                        "routing.flush_routes.delete",
                        e,
                        [
                            ("family", format!("{family:?}")),
                            ("table", table.to_string()),
                        ],
                    );
                }
            }
        }
    }
    Ok(())
}

fn address_message(index: u32, address: IpAddr, prefix_len: u8) -> AddressMessage {
    let mut message = AddressMessage::default();
    message.header.family = address_family(&address);
    message.header.prefix_len = prefix_len;
    message.header.index = index;
    message.attributes.push(AddressAttribute::Local(address));
    message.attributes.push(AddressAttribute::Address(address));
    message
}

fn route_message(command: &IpRouteCommand, interface: u32) -> RouteMessage {
    let mut message = route_dump_message(address_family(&command.destination), command.table);
    message.header.destination_prefix_length = command.prefix_len;
    message.header.protocol = RouteProtocol::Static;
    match command.route_type {
        RouteType::Unicast => {
            message.header.kind = NetlinkRouteType::Unicast;
            message.header.scope = RouteScope::Link;
        }
        RouteType::Local => {
            message.header.kind = NetlinkRouteType::Local;
            message.header.scope = RouteScope::Host;
        }
    }
    if command.prefix_len > 0 {
        message
            .attributes
            .push(RouteAttribute::Destination(command.destination.into()));
    }
    message.attributes.push(RouteAttribute::Oif(interface));
    message
}

fn route_dump_message(family: AddressFamily, table: u32) -> RouteMessage {
    let mut message = RouteMessage::default();
    message.header.address_family = family;
    message.header.table = if table < 256 {
        table as u8
    } else {
        RouteHeader::RT_TABLE_UNSPEC
    };
    message.attributes.push(RouteAttribute::Table(table));
    message
}

fn rule_message(command: &IpRuleCommand) -> io::Result<RuleMessage> {
    let mut message = RuleMessage::default();
    fill_rule_message(&mut message, command)?;
    Ok(message)
}

fn fill_rule_message(message: &mut RuleMessage, command: &IpRuleCommand) -> io::Result<()> {
    if !command.iif.is_empty() {
        netlink::validate_interface_name(&command.iif)?;
    }
    message.header.family = family_value(command.family);
    message.header.dst_len = 0;
    message.header.src_len = 0;
    message.header.tos = 0;
    message.header.table = if command.table < 256 {
        command.table as u8
    } else {
        RouteHeader::RT_TABLE_UNSPEC
    };
    message.header.action = match command.action {
        RuleAction::Lookup => NetlinkRuleAction::ToTable,
        RuleAction::Unreachable => NetlinkRuleAction::Unreachable,
        RuleAction::Any => NetlinkRuleAction::Unspec,
    };
    message.attributes.clear();
    if !command.iif.is_empty() {
        message
            .attributes
            .push(RuleAttribute::Iifname(command.iif.clone()));
    }
    message
        .attributes
        .push(RuleAttribute::Priority(command.priority));
    if command.action == RuleAction::Lookup {
        message.attributes.push(RuleAttribute::Table(command.table));
    }
    if let Some((mark, mask)) = command.fwmark {
        message.attributes.push(RuleAttribute::FwMark(mark));
        message.attributes.push(RuleAttribute::FwMask(mask));
    }
    Ok(())
}

fn route_table(route: &RouteMessage) -> Option<u32> {
    route
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RouteAttribute::Table(table) = attribute {
                Some(*table)
            } else {
                None
            }
        })
        .or({
            if route.header.table == RouteHeader::RT_TABLE_UNSPEC {
                None
            } else {
                Some(route.header.table as u32)
            }
        })
}

fn address_family(address: &IpAddr) -> AddressFamily {
    match address {
        IpAddr::V4(_) => AddressFamily::Inet,
        IpAddr::V6(_) => AddressFamily::Inet6,
    }
}

fn family_value(family: IpFamily) -> AddressFamily {
    match family {
        IpFamily::Ipv4 => AddressFamily::Inet,
        IpFamily::Ipv6 => AddressFamily::Inet6,
    }
}

pub(super) fn is_missing(error: &io::Error) -> bool {
    matches!(
        error_errno(error),
        Some(libc::ENOENT | libc::ESRCH | libc::ENODEV)
    )
}

pub(super) fn is_missing_address(error: &io::Error) -> bool {
    is_missing(error) || error_errno(error) == Some(libc::EADDRNOTAVAIL)
}

fn rule_details(command: &IpRuleCommand) -> Vec<(String, String)> {
    vec![
        ("operation".to_owned(), format!("{:?}", command.operation)),
        ("family".to_owned(), format!("{:?}", command.family)),
        ("iif".to_owned(), command.iif.clone()),
        ("priority".to_owned(), command.priority.to_string()),
        ("action".to_owned(), format!("{:?}", command.action)),
        ("table".to_owned(), command.table.to_string()),
        (
            "fwmark".to_owned(),
            command
                .fwmark
                .map(|(mark, mask)| format!("{mark:#x}/{mask:#x}"))
                .unwrap_or_default(),
        ),
    ]
}

pub(super) fn route_details(command: &IpRouteCommand) -> Vec<(String, String)> {
    vec![
        ("operation".to_owned(), format!("{:?}", command.operation)),
        ("type".to_owned(), format!("{:?}", command.route_type)),
        ("destination".to_owned(), command.destination.to_string()),
        ("prefix_len".to_owned(), command.prefix_len.to_string()),
        ("interface".to_owned(), command.interface.clone()),
        ("table".to_owned(), command.table.to_string()),
    ]
}

pub(super) fn address_details(command: &IpAddressCommand) -> Vec<(String, String)> {
    vec![
        ("operation".to_owned(), format!("{:?}", command.operation)),
        ("address".to_owned(), command.address.to_string()),
        ("prefix_len".to_owned(), command.prefix_len.to_string()),
        ("interface".to_owned(), command.interface.clone()),
    ]
}
