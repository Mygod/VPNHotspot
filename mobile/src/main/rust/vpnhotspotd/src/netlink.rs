use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex as StdMutex};

use futures_util::{pin_mut, StreamExt, TryStreamExt};
use rtnetlink::{
    packet_core::NetlinkPayload,
    packet_route::{
        link::{LinkAttribute, LinkMessage},
        AddressFamily, RouteNetlinkMessage,
    },
    MulticastGroup,
};
use tokio::select;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, MutexGuard, Notify};
use tokio::task::JoinHandle;

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
    neighbour_events: Arc<StdMutex<Option<UnboundedSender<RouteNetlinkMessage>>>>,
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
        let neighbour_events =
            Arc::new(StdMutex::new(None::<UnboundedSender<RouteNetlinkMessage>>));
        let task_ipv4_address_changed = ipv4_address_changed.clone();
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
                                &task_ipv4_address_changed,
                                &task_ipv6_address_changed,
                                &task_neighbour_events,
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
    neighbour_events: Arc<StdMutex<Option<UnboundedSender<RouteNetlinkMessage>>>>,
}

impl Drop for NeighbourRegistration {
    fn drop(&mut self) {
        match self.neighbour_events.lock() {
            Ok(mut neighbour_events) => {
                neighbour_events.take();
            }
            Err(_) => report::io(
                "netlink.neighbour_registration.drop",
                io::Error::other("neighbour monitor state poisoned"),
            ),
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
    neighbour_events: &StdMutex<Option<UnboundedSender<RouteNetlinkMessage>>>,
) {
    match payload {
        NetlinkPayload::InnerMessage(
            message @ (RouteNetlinkMessage::NewNeighbour(_) | RouteNetlinkMessage::DelNeighbour(_)),
        ) => send_neighbour_event(neighbour_events, message),
        NetlinkPayload::InnerMessage(
            RouteNetlinkMessage::NewLink(_) | RouteNetlinkMessage::DelLink(_),
        ) => ipv4_address_changed.notify_waiters(),
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

fn send_neighbour_event(
    neighbour_events: &StdMutex<Option<UnboundedSender<RouteNetlinkMessage>>>,
    message: RouteNetlinkMessage,
) {
    match neighbour_events.lock() {
        Ok(mut neighbour_events) => {
            if neighbour_events
                .as_ref()
                .is_some_and(|sender| sender.send(message).is_err())
            {
                neighbour_events.take();
            }
        }
        Err(_) => report::io(
            "netlink.send_neighbour_event",
            io::Error::other("neighbour monitor state poisoned"),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_link_accepts_kernel_enodev_and_synthetic_not_found() {
        assert!(is_missing_link(&io::Error::from_raw_os_error(libc::ENODEV)));
        assert!(is_missing_link(&io::Error::new(
            io::ErrorKind::NotFound,
            "interface not found"
        )));
        assert!(!is_missing_link(&io::Error::from_raw_os_error(
            libc::EINVAL
        )));
    }
}
