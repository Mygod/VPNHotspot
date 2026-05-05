use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};

use futures_util::{pin_mut, StreamExt, TryStreamExt};
use rtnetlink::{
    packet_core::NetlinkPayload,
    packet_route::{
        link::{LinkAttribute, LinkMessage},
        RouteNetlinkMessage,
    },
    MulticastGroup,
};
use tokio::select;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub(crate) struct Runtime {
    handle: rtnetlink::Handle,
    ipv6_address_changed: Arc<Notify>,
    neighbour_events: Arc<Mutex<Option<UnboundedSender<RouteNetlinkMessage>>>>,
    task: JoinHandle<()>,
}

impl Runtime {
    pub(crate) fn new() -> io::Result<Self> {
        let (connection, handle, mut messages) = rtnetlink::new_multicast_connection(&[
            MulticastGroup::Neigh,
            MulticastGroup::Ipv6Ifaddr,
        ])?;
        let ipv6_address_changed = Arc::new(Notify::new());
        let neighbour_events = Arc::new(Mutex::new(None::<UnboundedSender<RouteNetlinkMessage>>));
        let task_ipv6_address_changed = ipv6_address_changed.clone();
        let task_neighbour_events = neighbour_events.clone();
        let task = tokio::spawn(async move {
            tokio::pin!(connection);
            loop {
                select! {
                    _ = &mut connection => break,
                    message = messages.next() => match message {
                        Some((message, _)) => dispatch_message(
                            message.payload,
                            &task_ipv6_address_changed,
                            &task_neighbour_events,
                        ),
                        None => break,
                    },
                }
            }
        });
        Ok(Self {
            handle,
            ipv6_address_changed,
            neighbour_events,
            task,
        })
    }

    pub(crate) fn handle(&self) -> rtnetlink::Handle {
        self.handle.clone()
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
        let (sender, receiver) = unbounded_channel();
        let mut neighbour_events = self
            .neighbour_events
            .lock()
            .map_err(|_| io::Error::other("neighbour monitor state poisoned"))?;
        if neighbour_events.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "neighbour monitor already active",
            ));
        }
        *neighbour_events = Some(sender);
        Ok((
            NeighbourRegistration {
                neighbour_events: self.neighbour_events.clone(),
            },
            receiver,
        ))
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) struct NeighbourRegistration {
    neighbour_events: Arc<Mutex<Option<UnboundedSender<RouteNetlinkMessage>>>>,
}

impl Drop for NeighbourRegistration {
    fn drop(&mut self) {
        if let Ok(mut neighbour_events) = self.neighbour_events.lock() {
            neighbour_events.take();
        }
    }
}

pub(crate) async fn link_index(handle: &rtnetlink::Handle, name: &str) -> io::Result<u32> {
    validate_interface_name(name)?;
    let links = handle.link().get().match_name(name.to_owned()).execute();
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

pub(crate) async fn link_name(handle: &rtnetlink::Handle, index: u32) -> io::Result<String> {
    let links = handle.link().get().match_index(index).execute();
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

pub(crate) async fn link_names(handle: &rtnetlink::Handle) -> io::Result<HashMap<u32, String>> {
    let links = handle.link().get().execute();
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

fn dispatch_message(
    payload: NetlinkPayload<RouteNetlinkMessage>,
    ipv6_address_changed: &Notify,
    neighbour_events: &Mutex<Option<UnboundedSender<RouteNetlinkMessage>>>,
) {
    match payload {
        NetlinkPayload::InnerMessage(
            message @ (RouteNetlinkMessage::NewNeighbour(_) | RouteNetlinkMessage::DelNeighbour(_)),
        ) => send_neighbour_event(neighbour_events, message),
        NetlinkPayload::InnerMessage(
            RouteNetlinkMessage::NewAddress(_) | RouteNetlinkMessage::DelAddress(_),
        ) => ipv6_address_changed.notify_waiters(),
        _ => {}
    }
}

fn send_neighbour_event(
    neighbour_events: &Mutex<Option<UnboundedSender<RouteNetlinkMessage>>>,
    message: RouteNetlinkMessage,
) {
    let mut disconnected = false;
    match neighbour_events.lock() {
        Ok(neighbour_events) => {
            if let Some(sender) = neighbour_events.as_ref() {
                disconnected = sender.send(message).is_err();
            }
        }
        Err(_) => {
            eprintln!("neighbour monitor state poisoned");
            return;
        }
    }
    if disconnected {
        if let Ok(mut neighbour_events) = neighbour_events.lock() {
            neighbour_events.take();
        }
    }
}

fn link_name_from_message(link: LinkMessage) -> Option<String> {
    link.attributes.into_iter().find_map(|attribute| {
        if let LinkAttribute::IfName(name) = attribute {
            Some(name)
        } else {
            None
        }
    })
}
