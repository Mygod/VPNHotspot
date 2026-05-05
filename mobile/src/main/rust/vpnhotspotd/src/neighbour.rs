use std::collections::HashMap;
use std::io;
use std::net::IpAddr;

use futures_util::TryStreamExt;
use rtnetlink::packet_route::{
    neighbour::{
        NeighbourAddress, NeighbourAttribute, NeighbourMessage,
        NeighbourState as NetlinkNeighbourState,
    },
    RouteNetlinkMessage,
};
use tokio::sync::mpsc::UnboundedSender;
use tokio::{spawn, task::JoinHandle};

use crate::netlink;
use vpnhotspotd::shared::protocol::{neighbours_frame, Neighbour, NeighbourState};

pub(crate) async fn dump(handle: &netlink::Handle) -> io::Result<Vec<Neighbour>> {
    let interfaces = netlink::link_names(handle).await?;
    let _dump = handle.lock_dump().await;
    let mut messages = handle.raw().neighbours().get().execute();
    let mut neighbours = Vec::new();
    while let Some(message) = messages.try_next().await.map_err(netlink::to_io_error)? {
        let interface = interface_name_from_map(&interfaces, message.header.ifindex);
        if let Some(neighbour) = neighbour_from_message(false, message, interface) {
            neighbours.push(neighbour);
        }
    }
    Ok(neighbours)
}

pub(crate) struct Monitor {
    registration: netlink::NeighbourRegistration,
    task: JoinHandle<()>,
}

impl Monitor {
    pub(crate) fn spawn(
        netlink: &netlink::Runtime,
        sender: UnboundedSender<Vec<u8>>,
    ) -> io::Result<Self> {
        let (registration, mut events) = netlink.register_neighbour_monitor()?;
        let handle = netlink.handle();
        Ok(Self {
            registration,
            task: spawn(async move {
                while let Some(message) = events.recv().await {
                    if let Some(neighbour) = neighbour_from_event(&handle, message).await {
                        if sender.send(neighbours_frame(false, &[neighbour])).is_err() {
                            break;
                        }
                    }
                }
            }),
        })
    }

    pub(crate) async fn stop(self) {
        let Self { registration, task } = self;
        drop(registration);
        let _ = task.await;
    }
}

async fn neighbour_from_event(
    handle: &netlink::Handle,
    message: RouteNetlinkMessage,
) -> Option<Neighbour> {
    let (deleting, message) = match message {
        RouteNetlinkMessage::NewNeighbour(message) => (false, message),
        RouteNetlinkMessage::DelNeighbour(message) => (true, message),
        _ => return None,
    };
    let interface = match netlink::link_name(handle, message.header.ifindex).await {
        Ok(name) => name,
        Err(_) => format!("if{}", message.header.ifindex),
    };
    neighbour_from_message(deleting, message, interface)
}

fn interface_name_from_map(interfaces: &HashMap<u32, String>, index: u32) -> String {
    match interfaces.get(&index) {
        Some(name) => name.clone(),
        None => format!("if{index}"),
    }
}

fn neighbour_from_message(
    deleting: bool,
    message: NeighbourMessage,
    interface: String,
) -> Option<Neighbour> {
    if has_state(message.header.state, NetlinkNeighbourState::Noarp) || message.header.ifindex == 0
    {
        return None;
    }
    if !deleting && message.header.state == NetlinkNeighbourState::None {
        return None;
    }
    let mut address = None;
    let mut lladdr = Vec::new();
    for attribute in message.attributes {
        match attribute {
            NeighbourAttribute::Destination(NeighbourAddress::Inet(value)) => {
                address = Some(IpAddr::V4(value));
            }
            NeighbourAttribute::Destination(NeighbourAddress::Inet6(value)) => {
                address = Some(IpAddr::V6(value));
            }
            NeighbourAttribute::LinkLayerAddress(value) => lladdr.extend(value),
            _ => {}
        }
    }
    let address = address?;
    let state = if deleting {
        NeighbourState::Deleting
    } else {
        let state = message.header.state;
        if has_state(state, NetlinkNeighbourState::Reachable)
            || has_state(state, NetlinkNeighbourState::Delay)
            || has_state(state, NetlinkNeighbourState::Stale)
            || has_state(state, NetlinkNeighbourState::Probe)
            || has_state(state, NetlinkNeighbourState::Permanent)
        {
            NeighbourState::Valid
        } else if has_state(state, NetlinkNeighbourState::Failed) {
            NeighbourState::Failed
        } else if has_state(state, NetlinkNeighbourState::Incomplete) {
            NeighbourState::Incomplete
        } else {
            NeighbourState::Unset
        }
    };
    Some(Neighbour {
        address,
        interface,
        lladdr,
        state,
    })
}

fn has_state(state: NetlinkNeighbourState, flag: NetlinkNeighbourState) -> bool {
    u16::from(state) & u16::from(flag) != 0
}
