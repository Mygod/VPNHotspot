use std::collections::HashMap;
use std::io;
use std::net::IpAddr;

use rtnetlink::packet_route::{
    neighbour::{
        NeighbourAddress, NeighbourAttribute, NeighbourMessage,
        NeighbourState as NetlinkNeighbourState,
    },
    RouteNetlinkMessage,
};
use rtnetlink::MulticastGroup;
use tokio::select;
use tokio_util::sync::CancellationToken;

use crate::report::{ControllerSender, ControllerSenderExt};
use crate::{netlink, report};
use vpnhotspotd::shared::proto::daemon::{self, NeighbourState};
use vpnhotspotd::shared::protocol::{
    daemon_error_report_with_details, neighbour_monitor_update_frame, IoResultReportExt,
};

const KNOWN_NEIGHBOUR_STATE_BITS: u16 = libc::NUD_INCOMPLETE
    | libc::NUD_REACHABLE
    | libc::NUD_STALE
    | libc::NUD_DELAY
    | libc::NUD_PROBE
    | libc::NUD_FAILED
    | libc::NUD_NOARP
    | libc::NUD_PERMANENT;
const EVENT_GROUPS: [MulticastGroup; 2] = [MulticastGroup::Neigh, MulticastGroup::Link];

pub(crate) async fn dump(
    handle: &mut netlink::RequestConnection,
    call_id: u64,
) -> io::Result<Vec<daemon::Neighbour>> {
    let interfaces = netlink::link_names(handle).await?;
    let mut neighbours = Vec::new();
    for message in handle.dump_neighbours().await? {
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

pub(crate) async fn run(
    call_id: u64,
    sender: ControllerSender,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let mut events = netlink::EventConnection::new(&EVENT_GROUPS)
        .with_report_context("neighbour.events.netlink")?;
    let mut request =
        netlink::RequestConnection::new().with_report_context("neighbour.request.netlink")?;
    let (mut topology, neighbours) = read_snapshot(&mut request, call_id).await?;
    send_replacement(&sender, call_id, neighbours, &topology)?;
    // Set by an event overrun until a replacement subscription and a fresh dump both succeed.
    let mut resync_pending = false;
    // Whether the pending resync already reported a failure; retries after each event would
    // otherwise report a persistent failure once per neighbour event.
    let mut resync_reported = false;

    loop {
        if resync_pending {
            // The kernel reports ENOBUFS before notifications already queued on the old socket,
            // and those are older than any new dump, so the replacement subscription is opened
            // before the dump and both are committed together. On failure the old subscription
            // keeps feeding deltas and the resync is retried after the next event, because ending
            // the call would freeze the app's view.
            let staged = async {
                let replacement = netlink::EventConnection::new(&EVENT_GROUPS)
                    .with_report_context("neighbour.events.resubscribe")?;
                Ok::<_, io::Error>((replacement, read_snapshot(&mut request, call_id).await?))
            }
            .await;
            match staged {
                Ok((replacement, (next, neighbours))) => {
                    send_replacement(&sender, call_id, neighbours, &next)?;
                    events = replacement;
                    topology = next;
                    resync_pending = false;
                    resync_reported = false;
                }
                Err(e) if !resync_reported => {
                    report::io("neighbour.resync", e);
                    resync_reported = true;
                }
                // This failure episode was already reported.
                Err(_) => {}
            }
        }
        let message = select! {
            _ = cancel.cancelled() => return Ok(()),
            event = events.next() => event.with_report_context("neighbour.events")?,
        };
        match message {
            netlink::Event::Overrun => resync_pending = true,
            netlink::Event::Message(
                message @ (RouteNetlinkMessage::NewNeighbour(_)
                | RouteNetlinkMessage::DelNeighbour(_)),
            ) => {
                if let Some(delta) = neighbour_from_event(call_id, &mut request, message).await {
                    send_update(&sender, call_id, std::iter::once(delta), None, false)?;
                }
            }
            netlink::Event::Message(
                RouteNetlinkMessage::NewLink(_) | RouteNetlinkMessage::DelLink(_),
            ) => match netlink::bridge_topology(&mut request).await {
                Ok(next) if next != topology => {
                    send_update(
                        &sender,
                        call_id,
                        std::iter::empty(),
                        Some(next.clone()),
                        false,
                    )?;
                    topology = next;
                }
                Ok(_) => {}
                Err(e) => report::io("neighbour.bridge_topology", e),
            },
            netlink::Event::Message(_) => {}
        }
    }
}

/// Reads a full neighbour dump and bridge topology. The monitor takes one when it starts and
/// another after an event overrun, because events lost on a full receive buffer may include
/// deletes.
async fn read_snapshot(
    request: &mut netlink::RequestConnection,
    call_id: u64,
) -> io::Result<(daemon::LinkTopologySnapshot, Vec<daemon::Neighbour>)> {
    Ok((
        netlink::bridge_topology(request).await?,
        dump(request, call_id).await?,
    ))
}

/// Sends a snapshot from [`read_snapshot`] that replaces every neighbour reported earlier.
fn send_replacement(
    sender: &ControllerSender,
    call_id: u64,
    neighbours: Vec<daemon::Neighbour>,
    topology: &daemon::LinkTopologySnapshot,
) -> io::Result<()> {
    send_update(
        sender,
        call_id,
        neighbours
            .into_iter()
            .map(|neighbour| daemon::NeighbourDelta {
                delta: Some(daemon::neighbour_delta::Delta::Upsert(neighbour)),
            }),
        Some(topology.clone()),
        true,
    )
}

fn send_update(
    sender: &ControllerSender,
    call_id: u64,
    deltas: impl IntoIterator<Item = daemon::NeighbourDelta>,
    topology: Option<daemon::LinkTopologySnapshot>,
    replace_neighbours: bool,
) -> io::Result<()> {
    if sender.send_frame(neighbour_monitor_update_frame(
        call_id,
        deltas,
        topology,
        replace_neighbours,
    )) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "controller disconnected",
        ))
    }
}

async fn neighbour_from_event(
    call_id: u64,
    handle: &mut netlink::RequestConnection,
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
                    ("dev", interface.clone()),
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
                    dev: interface,
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
            dev: interface,
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
