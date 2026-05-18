use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex as StdMutex};

use crate::shared::proto::daemon;

#[derive(Clone)]
pub struct DnsCounters {
    inner: Arc<StdMutex<HashMap<[u8; 6], DnsCounter>>>,
    counter_epoch: Arc<[u8]>,
}

#[derive(Clone, Copy, Default)]
struct DnsCounter {
    sent_packets: u64,
    sent_bytes: u64,
    received_packets: u64,
    received_bytes: u64,
}

impl DnsCounters {
    pub fn new(counter_epoch: Vec<u8>) -> Self {
        Self {
            inner: Arc::new(StdMutex::new(HashMap::new())),
            counter_epoch: counter_epoch.into(),
        }
    }

    pub fn add_exchange(
        &self,
        mac: [u8; 6],
        query_len: usize,
        response_len: usize,
        sent_to_resolver: bool,
        received_from_resolver: bool,
    ) -> io::Result<()> {
        let mut counters = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("dns counter state poisoned"))?;
        counters.entry(mac).or_default().add_exchange(
            query_len,
            response_len,
            sent_to_resolver,
            received_from_resolver,
        );
        Ok(())
    }

    pub fn counters(
        &self,
        downstream: &str,
        active: impl IntoIterator<Item = [u8; 6]>,
    ) -> io::Result<Vec<daemon::TrafficCounter>> {
        let active = active.into_iter().collect::<HashSet<_>>();
        let mut counters = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("dns counter state poisoned"))?;
        let result = counters
            .iter()
            .map(|(mac, counter)| counter.proto(*mac, downstream, &self.counter_epoch))
            .collect();
        counters.retain(|mac, _| active.contains(mac));
        Ok(result)
    }
}

impl DnsCounter {
    fn add_exchange(
        &mut self,
        query_len: usize,
        response_len: usize,
        sent_to_resolver: bool,
        received_from_resolver: bool,
    ) {
        if sent_to_resolver {
            self.sent_packets += 1;
            self.sent_bytes += query_len as u64;
        }
        if received_from_resolver {
            self.received_packets += 1;
            self.received_bytes += response_len as u64;
        }
    }

    fn proto(
        &self,
        mac: [u8; 6],
        downstream: &str,
        counter_epoch: &[u8],
    ) -> daemon::TrafficCounter {
        daemon::TrafficCounter {
            mac: mac.to_vec(),
            downstream: downstream.to_owned(),
            source: Some(daemon::TrafficCounterSource {
                source: Some(daemon::traffic_counter_source::Source::DaemonSource(
                    daemon::DaemonTrafficSource::Dns as i32,
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
    fn counter_counts_only_resolver_api_sides() {
        let mut counter = DnsCounter::default();

        counter.add_exchange(10, 20, false, false);
        assert_eq!(counter.sent_packets, 0);
        assert_eq!(counter.sent_bytes, 0);
        assert_eq!(counter.received_packets, 0);
        assert_eq!(counter.received_bytes, 0);

        counter.add_exchange(10, 20, true, false);
        assert_eq!(counter.sent_packets, 1);
        assert_eq!(counter.sent_bytes, 10);
        assert_eq!(counter.received_packets, 0);
        assert_eq!(counter.received_bytes, 0);

        counter.add_exchange(30, 40, true, true);
        assert_eq!(counter.sent_packets, 2);
        assert_eq!(counter.sent_bytes, 40);
        assert_eq!(counter.received_packets, 1);
        assert_eq!(counter.received_bytes, 40);
    }

    #[test]
    fn counters_report_dns_source_and_drain_retired_entries() {
        let counters = DnsCounters::new(b"dns/test".to_vec());
        counters.add_exchange(MAC, 10, 20, true, true).unwrap();
        let first = counters.counters("ncm0", [MAC]).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].mac, MAC);
        assert_eq!(first[0].downstream, "ncm0");
        assert_eq!(first[0].counter_epoch, b"dns/test");
        assert_eq!(first[0].sent_packets, 1);
        assert_eq!(first[0].sent_bytes, 10);
        assert_eq!(first[0].received_packets, 1);
        assert_eq!(first[0].received_bytes, 20);
        assert_eq!(
            first[0].source.clone().and_then(|source| source.source),
            Some(daemon::traffic_counter_source::Source::DaemonSource(
                daemon::DaemonTrafficSource::Dns as i32,
            )),
        );

        assert_eq!(counters.counters("ncm0", []).unwrap().len(), 1);
        assert!(counters.counters("ncm0", []).unwrap().is_empty());
    }
}
