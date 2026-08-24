use std::io;

use tokio::select;
use tokio_util::sync::CancellationToken;
use vpnhotspotd::shared::downstream::{select_ipv4_address, DownstreamIpv4};
use vpnhotspotd::shared::protocol::{IoErrorReportExt, IoResultReportExt};

use crate::netlink;

pub(crate) async fn wait_ipv4(
    handle: &mut netlink::RequestConnection,
    events: &mut netlink::EventConnection,
    downstream: &str,
    cancel: &CancellationToken,
) -> io::Result<DownstreamIpv4> {
    loop {
        let address = select! {
            result = query_ipv4(handle, downstream) => result?,
            _ = cancel.cancelled() => return Err(cancelled_error()),
        };
        if let Some(address) = address {
            return Ok(address);
        }
        select! {
            _ = cancel.cancelled() => return Err(cancelled_error()),
            result = events.next() => { result?; }
        }
    }
}

async fn query_ipv4(
    handle: &mut netlink::RequestConnection,
    downstream: &str,
) -> io::Result<Option<DownstreamIpv4>> {
    let index = match netlink::link_index(handle, downstream).await {
        Ok(index) => index,
        Err(e) if netlink::is_missing_link(&e) => return Ok(None),
        Err(e) => {
            return Err(e.with_report_context_details(
                "downstream.ipv4.link_index",
                [("downstream", downstream.to_owned())],
            ));
        }
    };
    let messages = handle
        .dump_addresses(index)
        .await
        .with_report_context_details(
            "downstream.ipv4.dump",
            [("downstream", downstream.to_owned())],
        )?;
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
