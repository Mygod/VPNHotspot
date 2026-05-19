use std::{collections::HashMap, net::Ipv4Addr};

use crate::shared::{model::ClientConfig, proto::daemon};

const ANYWHERE: &str = "0.0.0.0/0";

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct Ipv4ForwardKey {
    pub downstream: String,
    pub address: Ipv4Addr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Sent,
    Received,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Ipv4ForwardCounterLine {
    pub key: Ipv4ForwardKey,
    pub direction: Direction,
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Clone, Copy)]
struct CounterValue {
    packets: u64,
    bytes: u64,
}

#[derive(Clone)]
pub struct Ipv4ForwardCounter {
    mac: [u8; 6],
    sent: Option<CounterValue>,
    received: Option<CounterValue>,
}

pub fn changed_ipv4_forward_counter_addresses(
    previous: &[ClientConfig],
    next: &[ClientConfig],
) -> Vec<Ipv4Addr> {
    let mut previous_owners = HashMap::new();
    for client in previous {
        for address in &client.ipv4 {
            previous_owners.insert(*address, client.mac);
        }
    }
    let mut result = Vec::new();
    for client in next {
        for address in &client.ipv4 {
            if previous_owners
                .get(address)
                .is_some_and(|previous_mac| *previous_mac != client.mac)
                && !result.contains(address)
            {
                result.push(*address);
            }
        }
    }
    result
}

pub fn parse_ipv4_forward_counter_line(
    line: &str,
) -> Result<Option<Ipv4ForwardCounterLine>, String> {
    let columns = line.split_whitespace().collect::<Vec<_>>();
    if columns.len() < 9 || columns[2] != "RETURN" {
        return Ok(None);
    }
    let packets = columns[0]
        .parse()
        .map_err(|e| format!("failed to parse traffic packet counter from {line:?}: {e}"))?;
    let bytes = columns[1]
        .parse()
        .map_err(|e| format!("failed to parse traffic byte counter from {line:?}: {e}"))?;
    let is_received = columns[7] == ANYWHERE;
    let is_sent = columns[8] == ANYWHERE;
    if is_received == is_sent {
        return Err(format!("unexpected traffic counter rule shape: {line}"));
    }
    let address = columns[if is_received { 8 } else { 7 }]
        .parse()
        .map_err(|e| format!("failed to parse traffic counter address from {line:?}: {e}"))?;
    Ok(Some(Ipv4ForwardCounterLine {
        key: Ipv4ForwardKey {
            downstream: columns[if is_received { 6 } else { 5 }].to_owned(),
            address,
        },
        direction: if is_received {
            Direction::Received
        } else {
            Direction::Sent
        },
        packets,
        bytes,
    }))
}

impl Ipv4ForwardCounter {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            sent: None,
            received: None,
        }
    }

    pub fn update_if_missing(&mut self, direction: Direction, packets: u64, bytes: u64) {
        let value = CounterValue { packets, bytes };
        match direction {
            Direction::Sent if self.sent.is_none() => self.sent = Some(value),
            Direction::Received if self.received.is_none() => self.received = Some(value),
            _ => {}
        }
    }

    pub fn into_proto(self, key: Ipv4ForwardKey) -> Option<daemon::TrafficCounter> {
        let sent = self.sent?;
        let received = self.received?;
        Some(daemon::TrafficCounter {
            mac: self.mac.to_vec(),
            downstream: key.downstream,
            source: Some(daemon::TrafficCounterSource {
                source: Some(daemon::traffic_counter_source::Source::Ipv4ForwardAddress(
                    key.address.octets().to_vec(),
                )),
            }),
            sent_packets: sent.packets,
            sent_bytes: sent.bytes,
            received_packets: received.packets,
            received_bytes: received.bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [2, 3, 5, 7, 11, 13];

    #[test]
    fn changed_ipv4_forward_counter_addresses_finds_addresses_reassigned_to_another_mac() {
        let unchanged = Ipv4Addr::new(192, 0, 2, 8);
        let changed = Ipv4Addr::new(192, 0, 2, 9);
        let added = Ipv4Addr::new(192, 0, 2, 10);
        let previous = [
            client(MAC, vec![unchanged, changed]),
            client([17, 19, 23, 29, 31, 37], vec![Ipv4Addr::new(192, 0, 2, 11)]),
        ];
        let next = [
            client(MAC, vec![unchanged]),
            client([41, 43, 47, 53, 59, 61], vec![changed, added, changed]),
        ];

        assert_eq!(
            changed_ipv4_forward_counter_addresses(&previous, &next),
            vec![changed]
        );
    }

    #[test]
    fn parse_counter_line_reads_direction_downstream_and_address() {
        let line =
            parse_ipv4_forward_counter_line("5 500 RETURN all -- ncm0 * 192.0.2.8 0.0.0.0/0")
                .unwrap()
                .unwrap();
        assert_eq!(line.direction, Direction::Sent);
        assert_eq!(line.packets, 5);
        assert_eq!(line.bytes, 500);
        assert_eq!(line.key.downstream, "ncm0");
        assert_eq!(line.key.address, Ipv4Addr::new(192, 0, 2, 8));

        let line =
            parse_ipv4_forward_counter_line("7 700 RETURN all -- * ncm0 0.0.0.0/0 192.0.2.8")
                .unwrap()
                .unwrap();
        assert_eq!(line.direction, Direction::Received);
        assert_eq!(line.packets, 7);
        assert_eq!(line.bytes, 700);
        assert_eq!(line.key.downstream, "ncm0");
        assert_eq!(line.key.address, Ipv4Addr::new(192, 0, 2, 8));
    }

    #[test]
    fn ipv4_forward_counter_proto_uses_real_ipv4_source() {
        let key = Ipv4ForwardKey {
            downstream: "ncm0".to_owned(),
            address: Ipv4Addr::new(192, 0, 2, 8),
        };
        let mut counter = Ipv4ForwardCounter::new(MAC);
        counter.update_if_missing(Direction::Sent, 5, 500);
        counter.update_if_missing(Direction::Received, 7, 700);

        let proto = counter.into_proto(key).unwrap();
        assert_eq!(proto.mac, MAC);
        assert_eq!(proto.downstream, "ncm0");
        assert_eq!(proto.sent_packets, 5);
        assert_eq!(proto.sent_bytes, 500);
        assert_eq!(proto.received_packets, 7);
        assert_eq!(proto.received_bytes, 700);
        assert_eq!(
            proto.source.and_then(|source| source.source),
            Some(daemon::traffic_counter_source::Source::Ipv4ForwardAddress(
                [192, 0, 2, 8].to_vec(),
            )),
        );
    }

    #[test]
    fn ipv4_forward_counter_keeps_first_line_per_direction() {
        let key = Ipv4ForwardKey {
            downstream: "ncm0".to_owned(),
            address: Ipv4Addr::new(192, 0, 2, 8),
        };
        let mut counter = Ipv4ForwardCounter::new(MAC);
        counter.update_if_missing(Direction::Sent, 0, 0);
        counter.update_if_missing(Direction::Sent, 5, 500);
        counter.update_if_missing(Direction::Received, 7, 700);
        counter.update_if_missing(Direction::Received, 9, 900);

        let proto = counter.into_proto(key).unwrap();
        assert_eq!(proto.sent_packets, 0);
        assert_eq!(proto.sent_bytes, 0);
        assert_eq!(proto.received_packets, 7);
        assert_eq!(proto.received_bytes, 700);
    }

    #[test]
    fn ipv4_forward_counter_skips_incomplete_direction_pairs() {
        let key = Ipv4ForwardKey {
            downstream: "ncm0".to_owned(),
            address: Ipv4Addr::new(192, 0, 2, 8),
        };
        let mut counter = Ipv4ForwardCounter::new(MAC);
        counter.update_if_missing(Direction::Sent, 5, 500);
        assert!(counter.into_proto(key).is_none());

        let key = Ipv4ForwardKey {
            downstream: "ncm0".to_owned(),
            address: Ipv4Addr::new(192, 0, 2, 8),
        };
        let mut counter = Ipv4ForwardCounter::new(MAC);
        counter.update_if_missing(Direction::Received, 7, 700);
        assert!(counter.into_proto(key).is_none());
    }

    fn client(mac: [u8; 6], ipv4: Vec<Ipv4Addr>) -> ClientConfig {
        ClientConfig { mac, ipv4 }
    }
}
