use std::net::{Ipv6Addr, SocketAddrV6};

use crate::{nat66::icmp, report};
use vpnhotspotd::shared::model::SessionConfig;

pub(super) async fn send_packet_too_big(
    config: &SessionConfig,
    gateway: Ipv6Addr,
    client: SocketAddrV6,
    destination: SocketAddrV6,
    mtu: u32,
    hop_limit: u8,
    payload: &[u8],
) {
    let context = icmp::UdpErrorContext {
        downstream: config.downstream.clone(),
        reply_mark: config.reply_mark,
        gateway,
        client,
        destination,
    };
    if let Err(e) = icmp::send_udp_packet_too_big(&context, mtu, hop_limit, payload).await {
        report::io_with_details(
            "nat66.udp_packet_too_big",
            e,
            [
                ("client", client.to_string()),
                ("destination", destination.to_string()),
            ],
        );
    }
}
