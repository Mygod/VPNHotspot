use std::net::Ipv4Addr;

use crate::shared::proto::daemon;

const ANYWHERE: &str = "0.0.0.0/0";
pub const IPV4_FORWARD_EPOCH: &[u8] = b"ipv4-forward";

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

#[derive(Clone)]
pub struct Ipv4ForwardCounter {
    mac: [u8; 6],
    sent_packets: u64,
    sent_bytes: u64,
    received_packets: u64,
    received_bytes: u64,
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
            sent_packets: 0,
            sent_bytes: 0,
            received_packets: 0,
            received_bytes: 0,
        }
    }

    pub fn update(&mut self, direction: Direction, packets: u64, bytes: u64) {
        match direction {
            Direction::Sent => {
                self.sent_packets = packets;
                self.sent_bytes = bytes;
            }
            Direction::Received => {
                self.received_packets = packets;
                self.received_bytes = bytes;
            }
        }
    }

    pub fn into_proto(self, key: Ipv4ForwardKey) -> daemon::TrafficCounter {
        daemon::TrafficCounter {
            mac: self.mac.to_vec(),
            downstream: key.downstream,
            source: Some(daemon::TrafficCounterSource {
                source: Some(daemon::traffic_counter_source::Source::Ipv4ForwardAddress(
                    key.address.octets().to_vec(),
                )),
            }),
            counter_epoch: IPV4_FORWARD_EPOCH.to_vec(),
            sent_packets: self.sent_packets,
            sent_bytes: self.sent_bytes,
            received_packets: self.received_packets,
            received_bytes: self.received_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [2, 3, 5, 7, 11, 13];

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
        counter.update(Direction::Sent, 5, 500);
        counter.update(Direction::Received, 7, 700);

        let proto = counter.into_proto(key);
        assert_eq!(proto.mac, MAC);
        assert_eq!(proto.downstream, "ncm0");
        assert_eq!(proto.counter_epoch, IPV4_FORWARD_EPOCH);
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
}
