use std::io;
use std::sync::Arc;

use futures_util::{pin_mut, TryStreamExt};
use tokio::select;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::downstream::{select_ipv4_address, DownstreamIpv4};
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

use crate::netlink;

pub(crate) async fn wait_ipv4(
    handle: &netlink::Handle,
    changed: Arc<Notify>,
    downstream: &str,
    cancel: &CancellationToken,
) -> io::Result<DownstreamIpv4> {
    loop {
        let changed = changed.notified();
        pin_mut!(changed);
        let address = select! {
            result = query_ipv4(handle, downstream) => result?,
            _ = cancel.cancelled() => return Err(cancelled_error()),
        };
        if let Some(address) = address {
            return Ok(address);
        }
        select! {
            _ = &mut changed => {}
            _ = cancel.cancelled() => return Err(cancelled_error()),
        }
    }
}

async fn query_ipv4(
    handle: &netlink::Handle,
    downstream: &str,
) -> io::Result<Option<DownstreamIpv4>> {
    let index = match netlink::link_index(handle, downstream).await {
        Ok(index) => index,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e.with_report_context_details(
                "downstream.ipv4.link_index",
                [("downstream", downstream.to_owned())],
            ));
        }
    };
    let _dump = handle.lock_dump().await;
    let addresses = handle
        .raw()
        .address()
        .get()
        .set_link_index_filter(index)
        .execute();
    pin_mut!(addresses);
    let mut messages = Vec::new();
    while let Some(message) = addresses
        .try_next()
        .await
        .map_err(netlink::to_io_error)
        .with_report_context_details(
            "downstream.ipv4.dump",
            [("downstream", downstream.to_owned())],
        )?
    {
        messages.push(message);
    }
    select_ipv4_address(&messages).map_err(|addresses| {
        io::Error::other("multiple downstream IPv4 addresses").with_report_context_details(
            "downstream.ipv4",
            [
                ("downstream", downstream.to_owned()),
                (
                    "addresses",
                    addresses
                        .into_iter()
                        .map(|address| format!("{}/{}", address.address, address.prefix_len))
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            ],
        )
    })
}

fn cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "session start cancelled")
}
