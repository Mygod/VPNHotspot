//! Routes that carry replies for the NAT66 prefix back to the downstream.

use std::net::IpAddr;

use crate::{netlink, report};
use vpnhotspotd::shared::model::{
    Ipv6NatConfig, SessionConfig, ANDROID_ROUTE_TABLE_LOCAL_NETWORK,
    ANDROID_ROUTE_TABLE_OFFSET_FROM_INDEX,
};

use super::netlink_commands::{IpCommand, IpOperation, IpRouteCommand, RouteType};
use super::{push_unique, RoutingMutation};

/// Routes `<nat66-prefix> dev <downstream>` in Android's `local_network` table 97 and in the
/// downstream's netd interface table.
///
/// On Android 15+, a tethering or Wi-Fi P2P group-owner downstream can get its own local network
/// agent, which ConnectivityService creates as a `PHYSICAL_LOCAL` netd network instead of adding
/// the interface to `LOCAL_NET_ID`. The platform release decides: 25Q4+ releases enable it unless a
/// kill switch is set, and earlier Android 15+ releases only behind a beta flag. netd then marks
/// packets arriving on the downstream with that network's netId and points the network's explicit
/// rule at the interface table, so a locally delivered TCP connection, whose accepted socket
/// inherits that mark, and kernel replies reflecting it never consult table 97. The kernel may
/// already route the gateway address's prefix there, because netd sets the downstream's
/// `accept_ra_rt_table` to -1000; this route is then redundant but harmless. Unmarked replies
/// still match netd's startup `fwmark 0x0/0x10000 lookup 97` rule before the agent's rule at the
/// same priority, because the kernel orders equal-priority rules by insertion. In `LOCAL_NET_ID`
/// mode netd keeps the downstream on table 97 and no rule selects the interface table, so that
/// route is inert there; installing it unconditionally avoids racing netd's network membership.
///
/// Sources:
/// https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#408
/// https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#1226
/// https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#6374
/// https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#903
/// https://android.googlesource.com/platform/system/netd/+/android-17.0.0_r1/server/RouteController.cpp#1284
pub(super) async fn push_prefix_routes(
    mutations: &mut Vec<RoutingMutation>,
    connection: &mut netlink::RequestConnection,
    config: &SessionConfig,
    ipv6_nat: &Ipv6NatConfig,
) {
    let route = |table| {
        RoutingMutation::Ip(IpCommand::Route(IpRouteCommand {
            operation: IpOperation::Replace,
            route_type: RouteType::Unicast,
            destination: IpAddr::V6(ipv6_nat.gateway.first_address()),
            prefix_len: ipv6_nat.gateway.network_length(),
            interface: config.downstream.clone(),
            table,
        }))
    };
    push_unique(mutations, route(ANDROID_ROUTE_TABLE_LOCAL_NETWORK));
    match netlink::link_index(connection, &config.downstream).await {
        Ok(ifindex) => push_unique(
            mutations,
            route(ANDROID_ROUTE_TABLE_OFFSET_FROM_INDEX + ifindex),
        ),
        Err(e) if netlink::is_missing_link(&e) => {}
        Err(e) => report::io_with_details(
            "routing.resolve_downstream_index",
            e,
            [("downstream", config.downstream.as_str())],
        ),
    }
}
