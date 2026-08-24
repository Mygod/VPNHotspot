use std::collections::{HashMap, HashSet};
use std::io;
use std::net::IpAddr;

use futures_channel::mpsc::UnboundedReceiver;
use futures_util::{pin_mut, StreamExt, TryStreamExt};
use rtnetlink::{
    packet_core::{NetlinkMessage, NetlinkPayload},
    packet_route::{
        address::AddressMessage,
        link::{InfoKind, LinkAttribute, LinkInfo, LinkMessage},
        neighbour::NeighbourMessage,
        route::RouteMessage,
        rule::RuleMessage,
        RouteNetlinkMessage,
    },
    sys::SocketAddr,
    IpVersion, MulticastGroup,
};
use tokio::task::JoinHandle;
use vpnhotspotd::shared::proto::daemon;

pub(crate) struct RequestConnection {
    inner: rtnetlink::Handle,
    task: JoinHandle<()>,
}

impl RequestConnection {
    pub(crate) fn new() -> io::Result<Self> {
        let (connection, inner, _) = rtnetlink::new_connection()?;
        Ok(Self {
            inner,
            task: tokio::spawn(async move {
                connection.await;
                crate::report::message(
                    "netlink.request_connection",
                    "rtnetlink request connection closed",
                    "ConnectionClosed",
                );
            }),
        })
    }

    pub(crate) async fn dump_addresses(
        &mut self,
        link_index: u32,
    ) -> io::Result<Vec<AddressMessage>> {
        self.inner
            .address()
            .get()
            .set_link_index_filter(link_index)
            .execute()
            .try_collect()
            .await
            .map_err(to_io_error)
    }

    pub(crate) async fn dump_neighbours(&mut self) -> io::Result<Vec<NeighbourMessage>> {
        self.inner
            .neighbours()
            .get()
            .execute()
            .try_collect()
            .await
            .map_err(to_io_error)
    }

    pub(crate) async fn dump_routes(
        &mut self,
        message: RouteMessage,
    ) -> io::Result<Vec<RouteMessage>> {
        self.inner
            .route()
            .get(message)
            .execute()
            .try_collect()
            .await
            .map_err(to_io_error)
    }

    pub(crate) async fn dump_rules(&mut self, version: IpVersion) -> io::Result<Vec<RuleMessage>> {
        self.inner
            .rule()
            .get(version)
            .execute()
            .try_collect()
            .await
            .map_err(to_io_error)
    }

    pub(crate) async fn replace_address(
        &mut self,
        index: u32,
        address: IpAddr,
        prefix_len: u8,
        message: AddressMessage,
    ) -> io::Result<()> {
        let mut request = self
            .inner
            .address()
            .add(index, address, prefix_len)
            .replace();
        *request.message_mut() = message;
        request.execute().await.map_err(to_io_error)
    }

    pub(crate) async fn delete_address(&mut self, message: AddressMessage) -> io::Result<()> {
        self.inner
            .address()
            .del(message)
            .execute()
            .await
            .map_err(to_io_error)
    }

    pub(crate) async fn replace_route(&mut self, message: RouteMessage) -> io::Result<()> {
        self.inner
            .route()
            .add(message)
            .replace()
            .execute()
            .await
            .map_err(to_io_error)
    }

    pub(crate) async fn delete_route(&mut self, message: RouteMessage) -> io::Result<()> {
        self.inner
            .route()
            .del(message)
            .execute()
            .await
            .map_err(to_io_error)
    }

    pub(crate) async fn add_rule(&mut self, message: RuleMessage) -> io::Result<()> {
        let mut request = self.inner.rule().add();
        *request.message_mut() = message;
        request.execute().await.map_err(to_io_error)
    }

    pub(crate) async fn delete_rule(&mut self, message: RuleMessage) -> io::Result<()> {
        self.inner
            .rule()
            .del(message)
            .execute()
            .await
            .map_err(to_io_error)
    }
}

impl Drop for RequestConnection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) struct EventConnection {
    messages: UnboundedReceiver<(NetlinkMessage<RouteNetlinkMessage>, SocketAddr)>,
    task: JoinHandle<()>,
}

impl EventConnection {
    pub(crate) fn new(groups: &[MulticastGroup]) -> io::Result<Self> {
        let (connection, _request_handle, messages) = rtnetlink::new_multicast_connection(groups)?;
        Ok(Self {
            messages,
            task: tokio::spawn(connection),
        })
    }

    pub(crate) async fn next(&mut self) -> io::Result<RouteNetlinkMessage> {
        while let Some((message, _)) = self.messages.next().await {
            if let NetlinkPayload::InnerMessage(message) = message.payload {
                return Ok(message);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "netlink event stream closed",
        ))
    }
}

impl Drop for EventConnection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) async fn link_index(handle: &mut RequestConnection, name: &str) -> io::Result<u32> {
    validate_interface_name(name)?;
    let links = handle
        .inner
        .link()
        .get()
        .match_name(name.to_owned())
        .execute();
    pin_mut!(links);
    if let Some(link) = links.try_next().await.map_err(to_io_error)? {
        Ok(link.header.index)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface {name} not found"),
        ))
    }
}

pub(crate) async fn link_mtu(handle: &mut RequestConnection, name: &str) -> io::Result<u32> {
    validate_interface_name(name)?;
    let links = handle
        .inner
        .link()
        .get()
        .match_name(name.to_owned())
        .execute();
    pin_mut!(links);
    if let Some(link) = links.try_next().await.map_err(to_io_error)? {
        link.attributes
            .into_iter()
            .find_map(|attribute| {
                if let LinkAttribute::Mtu(mtu) = attribute {
                    Some(mtu)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("interface {name} missing MTU"),
                )
            })
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface {name} not found"),
        ))
    }
}

pub(crate) async fn link_name(handle: &mut RequestConnection, index: u32) -> io::Result<String> {
    let links = handle.inner.link().get().match_index(index).execute();
    pin_mut!(links);
    if let Some(link) = links.try_next().await.map_err(to_io_error)? {
        link_name_from_message(link).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("interface {index} missing name"),
            )
        })
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface {index} not found"),
        ))
    }
}

pub(crate) async fn link_names(handle: &mut RequestConnection) -> io::Result<HashMap<u32, String>> {
    let mut links = handle.inner.link().get().execute();
    let mut names = HashMap::new();
    while let Some(link) = links.try_next().await.map_err(to_io_error)? {
        let index = link.header.index;
        if let Some(name) = link_name_from_message(link) {
            names.insert(index, name);
        }
    }
    Ok(names)
}

pub(crate) async fn bridge_topology(
    handle: &mut RequestConnection,
) -> io::Result<daemon::LinkTopologySnapshot> {
    let mut links = handle.inner.link().get().execute();
    let mut names = HashMap::new();
    let mut bridges = HashSet::new();
    let mut members = Vec::new();
    while let Some(link) = links.try_next().await.map_err(to_io_error)? {
        let summary = link_summary_from_message(link);
        if let Some(name) = summary.name {
            names.insert(summary.index, name);
        }
        if summary.is_bridge {
            bridges.insert(summary.index);
        }
        if let Some(controller) = summary.controller {
            members.push((summary.index, controller));
        }
    }
    let mut bridge_members = HashMap::<u32, Vec<String>>::new();
    for (member, bridge) in members {
        if bridges.contains(&bridge) {
            if let Some(member_name) = names.get(&member) {
                bridge_members
                    .entry(bridge)
                    .or_default()
                    .push(member_name.clone());
            }
        }
    }
    let mut bridge_interfaces = Vec::new();
    for bridge in bridges {
        if let Some(name) = names.get(&bridge) {
            let mut members = bridge_members.remove(&bridge).unwrap_or_default();
            members.sort_unstable();
            bridge_interfaces.push(daemon::BridgeInterface {
                name: name.clone(),
                members,
            });
        }
    }
    bridge_interfaces.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    Ok(daemon::LinkTopologySnapshot {
        bridges: bridge_interfaces,
    })
}

pub(crate) fn validate_interface_name(name: &str) -> io::Result<()> {
    if name.as_bytes().contains(&0) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name contains NUL byte",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn to_io_error(error: rtnetlink::Error) -> io::Error {
    match error {
        rtnetlink::Error::NetlinkError(error) => error.to_io(),
        error => io::Error::other(error),
    }
}

pub(crate) fn is_missing_link(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ENODEV)
}

fn link_name_from_message(link: LinkMessage) -> Option<String> {
    link_summary_from_message(link).name
}

struct LinkSummary {
    index: u32,
    name: Option<String>,
    is_bridge: bool,
    controller: Option<u32>,
}

fn link_summary_from_message(link: LinkMessage) -> LinkSummary {
    let index = link.header.index;
    let mut name = None;
    let mut is_bridge = false;
    let mut controller = None;
    for attribute in link.attributes {
        match attribute {
            LinkAttribute::IfName(value) => name = Some(value),
            LinkAttribute::Controller(value) => controller = Some(value),
            LinkAttribute::LinkInfo(info) => {
                is_bridge |= info
                    .iter()
                    .any(|info| matches!(info, LinkInfo::Kind(InfoKind::Bridge)));
            }
            _ => {}
        }
    }
    LinkSummary {
        index,
        name,
        is_bridge,
        controller,
    }
}
