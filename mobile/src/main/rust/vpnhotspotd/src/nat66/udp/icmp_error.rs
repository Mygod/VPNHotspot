use crate::{nat66::icmp, report};
use vpnhotspotd::shared::model::mac_string;

pub(super) async fn send_packet_too_big(
    context: icmp::UdpErrorContext,
    mtu: u32,
    hop_limit: u8,
    payload: &[u8],
) {
    if let Err(e) = icmp::send_udp_packet_too_big(&context, mtu, hop_limit, payload).await {
        report::io_with_details(
            "nat66.udp_packet_too_big",
            e,
            [
                ("mac", mac_string(&context.client_mac)),
                ("client", context.client.to_string()),
                ("destination", context.destination.to_string()),
            ],
        );
    }
}
