use std::collections::HashMap;
use std::io;

use crate::firewall::{self, IptablesTarget};
use crate::report;
use vpnhotspotd::shared::ipv4_forward_counter::{
    parse_ipv4_forward_counter_line, Ipv4ForwardCounter, Ipv4ForwardKey,
};
use vpnhotspotd::shared::model::SessionConfig;
use vpnhotspotd::shared::proto::daemon;

pub(crate) async fn read_counters(
    configs: &[SessionConfig],
) -> io::Result<Vec<daemon::TrafficCounter>> {
    let client_macs = client_macs(configs);
    let mut counters = HashMap::new();
    firewall::restore_stdout_lines(
        IptablesTarget::Ipv4,
        "*filter
-nvx -L vpnhotspot_stats
COMMIT
",
        |line| {
            if !is_counter_line(line) {
                return;
            }
            let Some(line) = (match parse_ipv4_forward_counter_line(line) {
                Ok(line) => line,
                Err(e) => {
                    report::stdout!("{e}");
                    None
                }
            }) else {
                return;
            };
            let Some(mac) = client_macs.get(&line.key) else {
                return;
            };
            let counter = counters
                .entry(line.key)
                .or_insert_with(|| Ipv4ForwardCounter::new(*mac));
            counter.update_if_missing(line.direction, line.packets, line.bytes);
        },
    )
    .await?;
    Ok(counters
        .into_iter()
        .filter_map(|(key, counter)| counter.into_proto(key))
        .collect())
}

fn is_counter_line(line: &str) -> bool {
    let mut columns = line.split_whitespace();
    columns
        .next()
        .is_some_and(|value| value.parse::<u64>().is_ok())
        && columns
            .next()
            .is_some_and(|value| value.parse::<u64>().is_ok())
}

fn client_macs(configs: &[SessionConfig]) -> HashMap<Ipv4ForwardKey, [u8; 6]> {
    let mut result = HashMap::new();
    for config in configs {
        for client in &config.clients {
            for address in &client.ipv4 {
                result.insert(
                    Ipv4ForwardKey {
                        downstream: config.downstream.clone(),
                        address: *address,
                    },
                    client.mac,
                );
            }
        }
    }
    result
}
