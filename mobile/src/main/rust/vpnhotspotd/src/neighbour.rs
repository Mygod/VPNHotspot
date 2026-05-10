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

use crate::{netlink, report};
use vpnhotspotd::shared::proto::daemon::{self, NeighbourState};
use vpnhotspotd::shared::protocol::{daemon_error_report_with_details, neighbour_deltas_frame};

const KNOWN_NEIGHBOUR_STATE_BITS: u16 = 0x01 | 0x02 | 0x04 | 0x08 | 0x10 | 0x20 | 0x40 | 0x80;

pub(crate) async fn dump(
    handle: &netlink::Handle,
    call_id: u64,
) -> io::Result<Vec<daemon::Neighbour>> {
    let interfaces = netlink::link_names(handle).await?;
    let _dump = handle.lock_dump().await;
    let mut messages = handle.raw().neighbours().get().execute();
    let mut neighbours = Vec::new();
    while let Some(message) = messages.try_next().await.map_err(netlink::to_io_error)? {
        let interface = interface_name_from_map(&interfaces, message.header.ifindex);
        if let Some(neighbour) = neighbour_from_message(call_id, false, message, interface)
            .and_then(|delta| match delta.delta {
                Some(daemon::neighbour_delta::Delta::Upsert(neighbour)) => Some(neighbour),
                _ => None,
            })
        {
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
    pub(crate) async fn spawn(
        call_id: u64,
        netlink: &netlink::Runtime,
        sender: UnboundedSender<Vec<u8>>,
    ) -> io::Result<Self> {
        let (registration, mut events) = netlink.register_neighbour_monitor()?;
        let handle = netlink.handle();
        let neighbours = dump(&handle, call_id).await?;
        if sender
            .send(neighbour_deltas_frame(
                call_id,
                neighbours
                    .into_iter()
                    .map(|neighbour| daemon::NeighbourDelta {
                        delta: Some(daemon::neighbour_delta::Delta::Upsert(neighbour)),
                    }),
            ))
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "controller disconnected",
            ));
        }
        Ok(Self {
            registration,
            task: spawn(async move {
                while let Some(message) = events.recv().await {
                    if let Some(delta) = neighbour_from_event(call_id, &handle, message).await {
                        if sender
                            .send(neighbour_deltas_frame(call_id, std::iter::once(delta)))
                            .is_err()
                        {
                            report::stderr!(
                                "neighbour monitor frame send failed: controller disconnected"
                            );
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
        if let Err(e) = task.await {
            report::message("neighbour.monitor_join", e.to_string(), "JoinError");
        }
    }
}

async fn neighbour_from_event(
    call_id: u64,
    handle: &netlink::Handle,
    message: RouteNetlinkMessage,
) -> Option<daemon::NeighbourDelta> {
    let (deleting, message) = match message {
        RouteNetlinkMessage::NewNeighbour(message) => (false, message),
        RouteNetlinkMessage::DelNeighbour(message) => (true, message),
        _ => return None,
    };
    let interface = match netlink::link_name(handle, message.header.ifindex).await {
        Ok(name) => name,
        Err(e) if netlink::is_missing_link(&e) => format!("if{}", message.header.ifindex),
        Err(e) => {
            report::io_with_details(
                "neighbour.link_name",
                e,
                [("ifindex", message.header.ifindex.to_string())],
            );
            format!("if{}", message.header.ifindex)
        }
    };
    neighbour_from_message(call_id, deleting, message, interface)
}

fn interface_name_from_map(interfaces: &HashMap<u32, String>, index: u32) -> String {
    match interfaces.get(&index) {
        Some(name) => name.clone(),
        // Match iproute2's if_indextoname fallback shape.
        //
        // Source: https://android.googlesource.com/platform/external/iproute2/+/4b9e917/lib/ll_map.c#152
        None => format!("if{index}"),
    }
}

fn neighbour_from_message(
    call_id: u64,
    deleting: bool,
    message: NeighbourMessage,
    interface: String,
) -> Option<daemon::NeighbourDelta> {
    if has_state(message.header.state, NetlinkNeighbourState::Noarp) || message.header.ifindex == 0
    {
        return None;
    }
    if !deleting && message.header.state == NetlinkNeighbourState::None {
        return None;
    }
    let mut address = None;
    let mut lladdr = None;
    for attribute in message.attributes {
        match attribute {
            NeighbourAttribute::Destination(NeighbourAddress::Inet(value)) => {
                address = Some(IpAddr::V4(value));
            }
            NeighbourAttribute::Destination(NeighbourAddress::Inet6(value)) => {
                address = Some(IpAddr::V6(value));
            }
            NeighbourAttribute::LinkLayerAddress(value) => {
                if value.len() == 6 {
                    let mut bytes = [0u8; 6];
                    bytes.copy_from_slice(&value);
                    lladdr = Some(bytes);
                } else {
                    report::report_for(
                        Some(call_id),
                        daemon_error_report_with_details(
                            "neighbour.lladdr",
                            "invalid link-layer address length",
                            "InvalidData",
                            [("length", value.len().to_string())],
                        ),
                    );
                }
            }
            _ => {}
        }
    }
    let address = address?;
    let raw_state = u16::from(message.header.state);
    let unknown_state_bits = raw_state & !KNOWN_NEIGHBOUR_STATE_BITS;
    if unknown_state_bits != 0 {
        report::report_for(
            Some(call_id),
            daemon_error_report_with_details(
                "neighbour.state",
                "unrecognized neighbour state bits",
                "InvalidData",
                [
                    ("state", format!("0x{raw_state:04x}")),
                    ("unknown", format!("0x{unknown_state_bits:04x}")),
                    ("interface", interface.clone()),
                    ("address", address.to_string()),
                    ("deleting", deleting.to_string()),
                ],
            ),
        );
    }
    if deleting {
        return Some(daemon::NeighbourDelta {
            delta: Some(daemon::neighbour_delta::Delta::Delete(
                daemon::NeighbourDelete {
                    address: ip_address_bytes(address),
                    interface,
                },
            )),
        });
    }
    let state = {
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
    Some(daemon::NeighbourDelta {
        delta: Some(daemon::neighbour_delta::Delta::Upsert(daemon::Neighbour {
            address: ip_address_bytes(address),
            interface,
            lladdr: lladdr.map(Vec::from),
            state: state as i32,
        })),
    })
}

fn ip_address_bytes(address: IpAddr) -> Vec<u8> {
    match address {
        IpAddr::V4(address) => address.octets().to_vec(),
        IpAddr::V6(address) => address.octets().to_vec(),
    }
}

fn has_state(state: NetlinkNeighbourState, flag: NetlinkNeighbourState) -> bool {
    u16::from(state) & u16::from(flag) != 0
}
