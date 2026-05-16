use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex as StdMutex};

use futures_util::{pin_mut, StreamExt, TryStreamExt};
use rtnetlink::{
    packet_core::NetlinkPayload,
    packet_route::{
        link::{InfoKind, LinkAttribute, LinkInfo, LinkMessage},
        AddressFamily, RouteNetlinkMessage,
    },
    MulticastGroup,
};
use tokio::select;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::task::JoinHandle;
use vpnhotspotd::shared::proto::daemon;

use crate::report;

#[derive(Clone)]
pub(crate) struct Handle {
    inner: rtnetlink::Handle,
    dump_lock: Arc<Mutex<()>>,
}

impl Handle {
    fn new(inner: rtnetlink::Handle) -> Self {
        Self {
            inner,
            dump_lock: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn raw(&self) -> &rtnetlink::Handle {
        &self.inner
    }

    pub(crate) async fn lock_dump(&self) -> MutexGuard<'_, ()> {
        self.dump_lock.lock().await
    }
}

pub(crate) struct Runtime {
    handle: Handle,
    ipv4_address_changed: Arc<Notify>,
    ipv6_address_changed: Arc<Notify>,
    neighbour_events: Arc<EventSlot>,
    link_events: Arc<EventSlot>,
    task: JoinHandle<()>,
}

impl Runtime {
    pub(crate) fn new() -> io::Result<Self> {
        let (connection, handle, mut messages) = rtnetlink::new_multicast_connection(&[
            MulticastGroup::Neigh,
            MulticastGroup::Link,
            MulticastGroup::Ipv4Ifaddr,
            MulticastGroup::Ipv6Ifaddr,
        ])?;
        let ipv4_address_changed = Arc::new(Notify::new());
        let ipv6_address_changed = Arc::new(Notify::new());
        let neighbour_events = Arc::new(EventSlot::new(
            "neighbour monitor already active",
            "neighbour monitor state poisoned",
            "netlink.neighbour_registration.drop",
            "netlink.send_neighbour_event",
        ));
        let link_events = Arc::new(EventSlot::new(
            "link monitor already active",
            "link monitor state poisoned",
            "netlink.link_registration.drop",
            "netlink.send_link_event",
        ));
        let task_ipv4_address_changed = ipv4_address_changed.clone();
        let task_ipv6_address_changed = ipv6_address_changed.clone();
        let task_neighbour_events = neighbour_events.clone();
        let task_link_events = link_events.clone();
        let task = tokio::spawn(async move {
            tokio::pin!(connection);
            loop {
                select! {
                    _ = &mut connection => break,
                    message = messages.next() => match message {
                        Some((message, _)) => dispatch_message(
                                message.payload,
                                &task_ipv4_address_changed,
                                &task_ipv6_address_changed,
                                &task_neighbour_events,
                                &task_link_events,
                            ),
                        None => break,
                    },
                }
            }
        });
        Ok(Self {
            handle: Handle::new(handle),
            ipv4_address_changed,
            ipv6_address_changed,
            neighbour_events,
            link_events,
            task,
        })
    }

    pub(crate) fn handle(&self) -> Handle {
        self.handle.clone()
    }

    pub(crate) fn ipv4_address_changed(&self) -> Arc<Notify> {
        self.ipv4_address_changed.clone()
    }

    pub(crate) fn ipv6_address_changed(&self) -> Arc<Notify> {
        self.ipv6_address_changed.clone()
    }

    pub(crate) fn register_neighbour_monitor(
        &self,
    ) -> io::Result<(
        NeighbourRegistration,
        UnboundedReceiver<RouteNetlinkMessage>,
    )> {
        self.neighbour_events.register()
    }

    pub(crate) fn register_link_monitor(
        &self,
    ) -> io::Result<(LinkRegistration, UnboundedReceiver<RouteNetlinkMessage>)> {
        self.link_events.register()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) type NeighbourRegistration = EventRegistration;
pub(crate) type LinkRegistration = EventRegistration;

pub(crate) struct EventRegistration {
    events: Arc<EventSlot>,
}

impl Drop for EventRegistration {
    fn drop(&mut self) {
        self.events.unregister();
    }
}

struct EventSlot {
    events: StdMutex<Option<UnboundedSender<RouteNetlinkMessage>>>,
    duplicate: &'static str,
    poisoned: &'static str,
    drop_context: &'static str,
    send_context: &'static str,
}

impl EventSlot {
    fn new(
        duplicate: &'static str,
        poisoned: &'static str,
        drop_context: &'static str,
        send_context: &'static str,
    ) -> Self {
        Self {
            events: StdMutex::new(None),
            duplicate,
            poisoned,
            drop_context,
            send_context,
        }
    }

    fn register(
        self: &Arc<Self>,
    ) -> io::Result<(EventRegistration, UnboundedReceiver<RouteNetlinkMessage>)> {
        let (sender, receiver) = unbounded_channel();
        let mut events = self
            .events
            .lock()
            .map_err(|_| io::Error::other(self.poisoned))?;
        if events.is_some() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, self.duplicate));
        }
        *events = Some(sender);
        Ok((
            EventRegistration {
                events: self.clone(),
            },
            receiver,
        ))
    }

    fn unregister(&self) {
        match self.events.lock() {
            Ok(mut events) => {
                events.take();
            }
            Err(_) => report::io(self.drop_context, io::Error::other(self.poisoned)),
        }
    }

    fn send(&self, message: RouteNetlinkMessage) {
        match self.events.lock() {
            Ok(mut events) => {
                if events
                    .as_ref()
                    .is_some_and(|sender| sender.send(message).is_err())
                {
                    events.take();
                }
            }
            Err(_) => report::io(self.send_context, io::Error::other(self.poisoned)),
        }
    }
}

pub(crate) async fn link_index(handle: &Handle, name: &str) -> io::Result<u32> {
    validate_interface_name(name)?;
    let links = handle
        .raw()
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

pub(crate) async fn link_mtu(handle: &Handle, name: &str) -> io::Result<u32> {
    validate_interface_name(name)?;
    let links = handle
        .raw()
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

pub(crate) async fn link_name(handle: &Handle, index: u32) -> io::Result<String> {
    let links = handle.raw().link().get().match_index(index).execute();
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

pub(crate) async fn link_names(handle: &Handle) -> io::Result<HashMap<u32, String>> {
    let _dump = handle.lock_dump().await;
    let links = handle.raw().link().get().execute();
    pin_mut!(links);
    let mut names = HashMap::new();
    while let Some(link) = links.try_next().await.map_err(to_io_error)? {
        let index = link.header.index;
        if let Some(name) = link_name_from_message(link) {
            names.insert(index, name);
        }
    }
    Ok(names)
}

pub(crate) async fn bridge_topology(handle: &Handle) -> io::Result<daemon::LinkTopologySnapshot> {
    let _dump = handle.lock_dump().await;
    let links = handle.raw().link().get().execute();
    pin_mut!(links);
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

fn dispatch_message(
    payload: NetlinkPayload<RouteNetlinkMessage>,
    ipv4_address_changed: &Notify,
    ipv6_address_changed: &Notify,
    neighbour_events: &EventSlot,
    link_events: &EventSlot,
) {
    match payload {
        NetlinkPayload::InnerMessage(
            message @ (RouteNetlinkMessage::NewNeighbour(_) | RouteNetlinkMessage::DelNeighbour(_)),
        ) => neighbour_events.send(message),
        NetlinkPayload::InnerMessage(
            message @ (RouteNetlinkMessage::NewLink(_) | RouteNetlinkMessage::DelLink(_)),
        ) => {
            ipv4_address_changed.notify_waiters();
            link_events.send(message);
        }
        NetlinkPayload::InnerMessage(
            RouteNetlinkMessage::NewAddress(message) | RouteNetlinkMessage::DelAddress(message),
        ) => match message.header.family {
            AddressFamily::Inet => ipv4_address_changed.notify_waiters(),
            AddressFamily::Inet6 => ipv6_address_changed.notify_waiters(),
            _ => {}
        },
        _ => {}
    }
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
