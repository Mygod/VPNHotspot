use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex as StdMutex};

use crate::shared::proto::daemon;

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub enum Nat66CounterSource {
    Tcp,
    Udp,
    Icmpv6,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Nat66CounterKey {
    pub mac: [u8; 6],
    pub source: Nat66CounterSource,
}

#[derive(Clone)]
pub struct Nat66Counters {
    inner: Arc<StdMutex<HashMap<Nat66CounterKey, Nat66Counter>>>,
    counter_epoch: Arc<[u8]>,
}

#[derive(Clone, Copy, Default)]
struct Nat66Counter {
    sent_packets: u64,
    sent_bytes: u64,
    received_packets: u64,
    received_bytes: u64,
}

impl Nat66Counters {
    pub fn new(counter_epoch: Vec<u8>) -> Self {
        Self {
            inner: Arc::new(StdMutex::new(HashMap::new())),
            counter_epoch: counter_epoch.into(),
        }
    }

    pub fn add_sent_bytes(
        &self,
        mac: [u8; 6],
        source: Nat66CounterSource,
        bytes: usize,
    ) -> io::Result<()> {
        self.update(mac, source, |counter| counter.sent_bytes += bytes as u64)
    }

    pub fn add_received_bytes(
        &self,
        mac: [u8; 6],
        source: Nat66CounterSource,
        bytes: usize,
    ) -> io::Result<()> {
        self.update(mac, source, |counter| {
            counter.received_bytes += bytes as u64;
        })
    }

    pub fn add_sent_packet(
        &self,
        mac: [u8; 6],
        source: Nat66CounterSource,
        bytes: usize,
    ) -> io::Result<()> {
        self.update(mac, source, |counter| {
            counter.sent_packets += 1;
            counter.sent_bytes += bytes as u64;
        })
    }

    pub fn add_received_packet(
        &self,
        mac: [u8; 6],
        source: Nat66CounterSource,
        bytes: usize,
    ) -> io::Result<()> {
        self.update(mac, source, |counter| {
            counter.received_packets += 1;
            counter.received_bytes += bytes as u64;
        })
    }

    pub fn counters(
        &self,
        downstream: &str,
        active: impl IntoIterator<Item = Nat66CounterKey>,
    ) -> io::Result<Vec<daemon::TrafficCounter>> {
        let active = active.into_iter().collect::<HashSet<_>>();
        let mut counters = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("nat66 counter state poisoned"))?;
        let result = counters
            .iter()
            .map(|(key, counter)| counter.proto(*key, downstream, &self.counter_epoch))
            .collect();
        counters.retain(|key, _| active.contains(key));
        Ok(result)
    }

    fn update(
        &self,
        mac: [u8; 6],
        source: Nat66CounterSource,
        update: impl FnOnce(&mut Nat66Counter),
    ) -> io::Result<()> {
        let mut counters = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("nat66 counter state poisoned"))?;
        update(counters.entry(Nat66CounterKey { mac, source }).or_default());
        Ok(())
    }
}

impl Nat66CounterSource {
    fn proto(self) -> daemon::DaemonTrafficSource {
        match self {
            Self::Tcp => daemon::DaemonTrafficSource::Nat66Tcp,
            Self::Udp => daemon::DaemonTrafficSource::Nat66Udp,
            Self::Icmpv6 => daemon::DaemonTrafficSource::Nat66Icmpv6,
        }
    }
}

impl Nat66Counter {
    fn proto(
        &self,
        key: Nat66CounterKey,
        downstream: &str,
        counter_epoch: &[u8],
    ) -> daemon::TrafficCounter {
        daemon::TrafficCounter {
            mac: key.mac.to_vec(),
            downstream: downstream.to_owned(),
            source: Some(daemon::TrafficCounterSource {
                source: Some(daemon::traffic_counter_source::Source::DaemonSource(
                    key.source.proto() as i32,
                )),
            }),
            counter_epoch: counter_epoch.to_vec(),
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
    fn counters_report_sources_and_drain_retired_entries() {
        let counters = Nat66Counters::new(b"nat66/test".to_vec());
        counters
            .add_sent_bytes(MAC, Nat66CounterSource::Tcp, 12)
            .unwrap();
        counters
            .add_received_bytes(MAC, Nat66CounterSource::Tcp, 34)
            .unwrap();
        counters
            .add_sent_packet(MAC, Nat66CounterSource::Udp, 56)
            .unwrap();
        counters
            .add_received_packet(MAC, Nat66CounterSource::Udp, 78)
            .unwrap();

        let active = [Nat66CounterKey {
            mac: MAC,
            source: Nat66CounterSource::Tcp,
        }];
        let first = counters.counters("ncm0", active).unwrap();
        assert_eq!(first.len(), 2);
        let tcp = first
            .iter()
            .find(|counter| {
                counter
                    .source
                    .as_ref()
                    .and_then(|source| source.source.clone())
                    == Some(daemon::traffic_counter_source::Source::DaemonSource(
                        daemon::DaemonTrafficSource::Nat66Tcp as i32,
                    ))
            })
            .unwrap();
        assert_eq!(tcp.sent_bytes, 12);
        assert_eq!(tcp.received_bytes, 34);
        let udp = first
            .iter()
            .find(|counter| {
                counter
                    .source
                    .as_ref()
                    .and_then(|source| source.source.clone())
                    == Some(daemon::traffic_counter_source::Source::DaemonSource(
                        daemon::DaemonTrafficSource::Nat66Udp as i32,
                    ))
            })
            .unwrap();
        assert_eq!(udp.sent_packets, 1);
        assert_eq!(udp.sent_bytes, 56);
        assert_eq!(udp.received_packets, 1);
        assert_eq!(udp.received_bytes, 78);

        let second = counters.counters("ncm0", active).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].counter_epoch, b"nat66/test");
    }
}
