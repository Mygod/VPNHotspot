//! Bounded IPv4 and IPv6 ingress reassembly.
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
#[cfg(test)]
use std::net::Ipv6Addr;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::shared::admission::{logical_footprint, Admission, Class, Lease, Request};

use etherparse::{IpNumber, Ipv4Header, Ipv6FragmentHeaderSlice};

use crate::shared::ip_wire::Packet;
#[cfg(test)]
use crate::shared::packet_writer::{IPV4_HEADER_LEN, IPV6_FRAGMENT_HEADER_LEN, IPV6_HEADER_LEN};

const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(60);

const MAX_DATAGRAM: usize = u16::MAX as usize;

const MAX_HEADER: usize = 60;

#[derive(Debug, PartialEq, Eq)]
pub enum Accepted {
    Pending,
    Complete(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    Malformed(&'static str),
    Overlap,
    Denied,
}

struct Fragment<'a> {
    key: Key,
    offset: usize,
    more: bool,
    header: Option<Header>,
    payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    source: IpAddr,
    destination: IpAddr,
    identification: u32,
    protocol: u8,
}

struct Context {
    payload: Vec<u8>,
    received: Vec<Range<usize>>,
    header: Option<Header>,
    total: Option<usize>,
    deadline: Instant,
    charged: u64,
}

impl Context {
    fn complete(&self) -> bool {
        match (self.total, self.received.as_slice()) {
            (Some(total), [only]) => *only == (0..total),
            _ => false,
        }
    }

    fn footprint(&self) -> usize {
        self.payload.capacity() + self.received.capacity() * std::mem::size_of::<Range<usize>>()
    }

    fn project(&self, end: usize, completing: bool) -> Option<Projection> {
        let range = std::mem::size_of::<Range<usize>>() as u64;
        let payload = self.payload.capacity();
        let grows = end > payload;
        let next_payload = if grows { end } else { payload } as u64;
        let next_ranges = (self.received.len() as u64)
            .checked_add(1)?
            .checked_mul(range)?;
        let retained = next_payload.checked_add(next_ranges)?;
        let mut peak = (self.received.capacity() as u64).checked_mul(range)?;
        if grows {
            peak = peak.checked_add(payload as u64)?;
        }
        if completing {
            peak = peak
                .checked_add(MAX_HEADER as u64)?
                .checked_add(next_payload)?;
        }
        Some(Projection { retained, peak })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Projection {
    retained: u64,
    peak: u64,
}

#[derive(Clone, Copy)]
struct Header {
    bytes: [u8; MAX_HEADER],
    length: u8,
}

impl Header {
    fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_HEADER {
            return None;
        }
        let mut kept = [0u8; MAX_HEADER];
        kept[..bytes.len()].copy_from_slice(bytes);
        Some(Header {
            bytes: kept,
            length: bytes.len() as u8,
        })
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.length as usize]
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Header({} bytes)", self.length)
    }
}

#[derive(Default)]
struct Counters {
    held: u64,
    completed: u64,
    malformed: u64,
    overlapping: u64,
    denied: u64,
    expired: u64,
    headless: u64,
    undercharged: u64,
}

pub struct Table {
    contexts: HashMap<Key, Context>,
    charged: u64,
    prepared: usize,
    peak: u64,
    counters: Counters,
}

impl Table {
    pub fn with_capacity(contexts: usize) -> Self {
        Self {
            contexts: HashMap::with_capacity(contexts),
            charged: 0,
            prepared: contexts,
            peak: 0,
            counters: Counters::default(),
        }
    }

    pub fn footprint(contexts: usize) -> Option<u64> {
        logical_footprint::<(Key, Context)>(contexts)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    pub fn accept(
        &mut self,
        packet: &[u8],
        now: Instant,
        admission: &mut Admission,
        lease: &Lease,
    ) -> Result<Accepted, Reject> {
        let fragment = match parse(packet) {
            Ok(fragment) => fragment,
            Err(e) => {
                self.counters.malformed += 1;
                return Err(e);
            }
        };
        let end = fragment.offset + fragment.payload.len();
        if end > MAX_DATAGRAM {
            self.counters.malformed += 1;
            return Err(Reject::Malformed("fragment reaches past a datagram"));
        }
        let total = (!fragment.more).then_some(end);
        if !self.contexts.contains_key(&fragment.key) && self.contexts.len() >= self.prepared {
            self.counters.denied += 1;
            return Err(Reject::Denied);
        }
        let context = match self.contexts.entry(fragment.key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Context {
                payload: Vec::new(),
                received: Vec::new(),
                header: None,
                total: None,
                deadline: now + REASSEMBLY_TIMEOUT,
                charged: 0,
            }),
        };
        if let Some(known) = context.total {
            if total.is_some_and(|total| total != known) || end > known {
                self.discard(fragment.key, admission, lease);
                self.counters.overlapping += 1;
                return Err(Reject::Overlap);
            }
        }
        let previous = context.charged;
        let completing = total.or(context.total).is_some_and(|total| {
            end == total || context.received.iter().any(|range| range.end == total)
        });
        let Some(projection) = context.project(end, completing) else {
            self.counters.denied += 1;
            if context.received.is_empty() {
                self.contexts.remove(&fragment.key);
            }
            return Err(Reject::Denied);
        };
        let growth = projection.retained.saturating_sub(previous);
        let Some(reserve) = growth.checked_add(projection.peak) else {
            self.counters.denied += 1;
            if context.received.is_empty() {
                self.contexts.remove(&fragment.key);
            }
            return Err(Reject::Denied);
        };
        if reserve > 0
            && admission
                .grow(
                    lease,
                    Request {
                        bytes: reserve,
                        byte_class: Class::General,
                        fragment_bytes: reserve,
                        ..Request::default()
                    },
                )
                .is_err()
        {
            self.counters.denied += 1;
            if context.received.is_empty() {
                self.contexts.remove(&fragment.key);
            }
            return Err(Reject::Denied);
        }
        self.charged += reserve;
        let context = self
            .contexts
            .get_mut(&fragment.key)
            .expect("just inserted or found");
        context.charged = previous + reserve;
        if !insert(context, fragment.offset, fragment.payload) {
            self.discard(fragment.key, admission, lease);
            self.counters.overlapping += 1;
            return Err(Reject::Overlap);
        }
        if let Some(header) = fragment.header {
            context.header = Some(header);
        }
        if let Some(total) = total {
            context.total = Some(total);
        }
        let reserved = previous + reserve;
        let actual = context.footprint() as u64;
        let keep = if context.complete() {
            actual.checked_add(projection.peak)
        } else {
            Some(actual)
        };
        let Some(excess) = keep.and_then(|keep| reconcile(reserved, keep)) else {
            return Err(self.undercharged(fragment.key, admission, lease));
        };
        if excess > 0 {
            admission.shrink(
                lease,
                Request {
                    bytes: excess,
                    byte_class: Class::General,
                    fragment_bytes: excess,
                    ..Request::default()
                },
            );
            self.charged -= excess;
        }
        let context = self
            .contexts
            .get_mut(&fragment.key)
            .expect("just reconciled");
        context.charged = reserved - excess;
        self.peak = self.peak.max(self.charged);
        if !context.complete() {
            self.counters.held += 1;
            return Ok(Accepted::Pending);
        }
        let context = self.contexts.remove(&fragment.key).expect("just held");
        let Some(header) = context.header else {
            let charged = context.charged;
            drop(context);
            self.release(charged, admission, lease);
            self.counters.headless += 1;
            return Err(Reject::Malformed("reassembled without fragment zero"));
        };
        let (assembled, charged) = complete(context, &header);
        self.release(charged, admission, lease);
        self.counters.completed += 1;
        Ok(Accepted::Complete(assembled))
    }

    pub fn retire(&mut self, admission: &mut Admission, lease: &Lease) {
        self.contexts.clear();
        let held = self.charged;
        self.release(held, admission, lease);
    }

    fn discard(&mut self, key: Key, admission: &mut Admission, lease: &Lease) {
        if let Some(context) = self.contexts.remove(&key) {
            self.release(context.charged, admission, lease);
        }
    }

    fn undercharged(&mut self, key: Key, admission: &mut Admission, lease: &Lease) -> Reject {
        self.counters.undercharged += 1;
        self.discard(key, admission, lease);
        Reject::Denied
    }

    fn release(&mut self, bytes: u64, admission: &mut Admission, lease: &Lease) {
        if bytes == 0 {
            return;
        }
        admission.shrink(
            lease,
            Request {
                bytes,
                byte_class: Class::General,
                fragment_bytes: bytes,
                ..Request::default()
            },
        );
        self.charged -= bytes;
    }

    pub fn sweep(
        &mut self,
        now: Instant,
        admission: &mut Admission,
        lease: &Lease,
        mut quote: impl FnMut(Vec<u8>),
    ) -> u64 {
        let mut freed = 0;
        let mut retired = 0u64;
        let mut quoted = 0u64;
        self.contexts.retain(|_, context| {
            if context.deadline > now {
                return true;
            }
            freed += context.charged;
            retired += 1;
            if let Some(header) = &context.header {
                quoted += 1;
                quote(assemble(header.as_slice(), &context.payload));
            }
            false
        });
        self.release(freed, admission, lease);
        self.counters.expired += retired;
        self.counters.headless += retired - quoted;
        retired
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.contexts.values().map(|context| context.deadline).min()
    }

    pub fn describe(&self) -> String {
        format!(
            "{} contexts of {} prepared holding {} bytes, peak {}, held {} completed {} malformed {} \
             overlapping {} denied {} expired {} headless {} undercharged {}",
            self.contexts.len(),
            self.prepared,
            self.charged,
            self.peak,
            self.counters.held,
            self.counters.completed,
            self.counters.malformed,
            self.counters.overlapping,
            self.counters.denied,
            self.counters.expired,
            self.counters.headless,
            self.counters.undercharged
        )
    }
}

fn merge_ranges(old: &[Range<usize>], new: Range<usize>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(old.len() + 1);
    let mut placed = false;
    for range in old {
        if !placed && new.start <= range.start {
            push_merged(&mut merged, new.clone());
            placed = true;
        }
        push_merged(&mut merged, range.clone());
    }
    if !placed {
        push_merged(&mut merged, new);
    }
    merged
}

fn push_merged(merged: &mut Vec<Range<usize>>, range: Range<usize>) {
    match merged.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => merged.push(range),
    }
}

fn insert(context: &mut Context, offset: usize, payload: &[u8]) -> bool {
    let end = offset + payload.len();
    if context
        .received
        .iter()
        .any(|range| offset < range.end && range.start < end)
    {
        return false;
    }
    if context.payload.len() < end {
        if end > context.payload.capacity() {
            let mut next = Vec::with_capacity(end);
            next.extend_from_slice(&context.payload);
            next.resize(end, 0);
            context.payload = next;
        } else {
            context.payload.resize(end, 0);
        }
    }
    context.payload[offset..end].copy_from_slice(payload);
    context.received = merge_ranges(&context.received, offset..end);
    true
}

fn reconcile(reserved: u64, keep: u64) -> Option<u64> {
    reserved.checked_sub(keep)
}

fn complete(context: Context, header: &Header) -> (Vec<u8>, u64) {
    let assembled = assemble(header.as_slice(), &context.payload);
    let charged = context.charged;
    drop(context);
    (assembled, charged)
}

fn assemble(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(header.len() + payload.len());
    packet.extend_from_slice(header);
    packet.extend_from_slice(payload);
    match packet.first().map(|byte| byte >> 4) {
        Some(4) => {
            let length = u16::try_from(packet.len()).unwrap_or(u16::MAX);
            packet[2..4].copy_from_slice(&length.to_be_bytes());
            packet[6] = 0;
            packet[7] = 0;
            packet[10] = 0;
            packet[11] = 0;
            if let Ok((parsed, _)) = Ipv4Header::from_slice(&packet) {
                let checksum = parsed.calc_header_checksum();
                packet[10..12].copy_from_slice(&checksum.to_be_bytes());
            }
        }
        _ => {
            let length = u16::try_from(payload.len()).unwrap_or(u16::MAX);
            packet[4..6].copy_from_slice(&length.to_be_bytes());
        }
    }
    packet
}

fn shape(offset: usize, more: bool, payload: usize) -> Result<(), Reject> {
    if more {
        if payload == 0 {
            return Err(Reject::Malformed("a non-final fragment carries no payload"));
        }
        if !payload.is_multiple_of(8) {
            return Err(Reject::Malformed(
                "a non-final fragment is not a multiple of eight octets",
            ));
        }
    } else if offset != 0 && payload == 0 {
        return Err(Reject::Malformed("a final fragment carries no payload"));
    }
    Ok(())
}

fn parse(packet: &[u8]) -> Result<Fragment<'_>, Reject> {
    match Packet::parse(packet).map_err(|error| Reject::Malformed(error.message()))? {
        Packet::Ipv4 { header, payload } => {
            let offset = usize::from(header.fragments_offset().byte_offset());
            let more = header.more_fragments();
            if !more && offset == 0 {
                return Err(Reject::Malformed("IPv4 packet is not a fragment"));
            }
            shape(offset, more, payload.len())?;
            Ok(Fragment {
                key: Key {
                    source: IpAddr::V4(header.source_addr()),
                    destination: IpAddr::V4(header.destination_addr()),
                    identification: u32::from(header.identification()),
                    protocol: header.protocol().0,
                },
                offset,
                more,
                header: if offset == 0 {
                    Some(Header::new(header.slice()).ok_or(Reject::Malformed(
                        "IPv4 header is longer than a header can be",
                    ))?)
                } else {
                    None
                },
                payload,
            })
        }
        Packet::Ipv6 { header, payload } => {
            if header.next_header() != IpNumber::IPV6_FRAGMENTATION_HEADER {
                return Err(Reject::Malformed("IPv6 packet is not a fragment"));
            }
            let fragment = Ipv6FragmentHeaderSlice::from_slice(payload)
                .map_err(|_| Reject::Malformed("IPv6 fragment header does not fit"))?;
            let offset = usize::from(fragment.fragment_offset().byte_offset());
            let more = fragment.more_fragments();
            let payload = &payload[fragment.slice().len()..];
            shape(offset, more, payload.len())?;
            let mut zero = None;
            if offset == 0 {
                let mut first = Header::new(header.slice())
                    .ok_or(Reject::Malformed("IPv6 header does not fit"))?;
                first.bytes[6] = fragment.next_header().0;
                zero = Some(first);
            }
            Ok(Fragment {
                key: Key {
                    source: IpAddr::V6(header.source_addr()),
                    destination: IpAddr::V6(header.destination_addr()),
                    identification: fragment.identification(),
                    protocol: 0,
                },
                offset,
                more,
                header: zero,
                payload,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::packet_writer::{fragment_ipv4, fragment_ipv6};
    use crate::shared::udp_wire::{self, build_reply};
    use std::net::SocketAddr;

    const CLIENT4: &str = "192.0.2.1:40000";
    const REMOTE4: &str = "198.51.100.7:443";
    const CLIENT6: &str = "[2001:db8:1::2]:40000";
    const REMOTE6: &str = "[2606:4700::1111]:443";

    fn datagram(ipv6: bool, payload: usize) -> Vec<u8> {
        let (client, remote): (SocketAddr, SocketAddr) = if ipv6 {
            (CLIENT6.parse().unwrap(), REMOTE6.parse().unwrap())
        } else {
            (CLIENT4.parse().unwrap(), REMOTE4.parse().unwrap())
        };
        build_reply(client, remote, 64, Some(0x1234), &vec![0x5au8; payload]).unwrap()
    }

    fn fragments(packet: &[u8], mtu: usize) -> Vec<Vec<u8>> {
        let mut pieces = Vec::new();
        if packet[0] >> 4 == 6 {
            fragment_ipv6(packet, mtu, 0x9abcdef0, |piece| pieces.push(piece)).unwrap();
        } else {
            fragment_ipv4(packet, mtu, |piece| pieces.push(piece)).unwrap();
        }
        pieces
    }

    struct Fixture {
        admission: Admission,
        lease: Lease,
        table: Table,
    }

    impl Fixture {
        fn with_cap(fragment_cap: u64) -> Self {
            let mut admission = Admission::new(crate::shared::admission::Totals {
                admission_id: 1,
                record_total: 64,
                dns_record_floor: 0,
                byte_total: 8 << 20,
                reserved_byte_floor: 1 << 20,
                fragment_cap,
                byte_only_owners: 4,
            })
            .expect("the fixture totals hold their own accounting");
            let lease = admission
                .reserve(Request::bytes(
                    Table::footprint(64).expect("fits"),
                    Class::General,
                ))
                .expect("granted");
            Self {
                admission,
                lease,
                table: Table::with_capacity(64),
            }
        }

        fn accept(&mut self, packet: &[u8], now: Instant) -> Result<Accepted, Reject> {
            self.table
                .accept(packet, now, &mut self.admission, &self.lease)
        }

        fn sweep(&mut self, now: Instant, quote: impl FnMut(Vec<u8>)) -> u64 {
            self.table
                .sweep(now, &mut self.admission, &self.lease, quote)
        }

        fn retire(&mut self) {
            self.table.retire(&mut self.admission, &self.lease);
        }
    }

    fn table() -> Fixture {
        Fixture::with_cap(1 << 20)
    }

    #[test]
    fn fragments_reassemble_to_the_original_datagram() {
        for ipv6 in [false, true] {
            let packet = datagram(ipv6, 3000);
            let pieces = fragments(&packet, 1280);
            assert!(pieces.len() > 2, "{} pieces", pieces.len());
            let mut table = table();
            let now = Instant::now();
            let mut completed = None;
            for piece in &pieces {
                match table.accept(piece, now).unwrap() {
                    Accepted::Pending => {}
                    Accepted::Complete(whole) => completed = Some(whole),
                }
            }
            assert_eq!(completed.as_deref(), Some(packet.as_slice()), "ipv6 {ipv6}");
            assert!(udp_wire::parse(&completed.unwrap()).is_ok(), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0);
            assert!(table.table.contexts.is_empty());
        }
    }

    #[test]
    fn order_does_not_matter() {
        for ipv6 in [false, true] {
            let packet = datagram(ipv6, 2500);
            let mut pieces = fragments(&packet, 1280);
            pieces.reverse();
            let mut table = table();
            let now = Instant::now();
            let mut completed = None;
            for piece in &pieces {
                if let Accepted::Complete(whole) = table.accept(piece, now).unwrap() {
                    completed = Some(whole);
                }
            }
            assert_eq!(completed.as_deref(), Some(packet.as_slice()), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0);
        }
    }

    #[test]
    fn an_overlapping_fragment_discards_the_whole_datagram() {
        for ipv6 in [false, true] {
            let packet = datagram(ipv6, 3000);
            let pieces = fragments(&packet, 1280);
            let mut table = table();
            let now = Instant::now();
            assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
            assert_eq!(table.accept(&pieces[0], now), Err(Reject::Overlap));
            assert!(table.table.contexts.is_empty(), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0);
            for piece in &pieces[1..] {
                assert_eq!(table.accept(piece, now), Ok(Accepted::Pending));
            }
        }
    }

    #[test]
    fn a_fragment_reaching_past_a_declared_total_is_refused() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let tail = pieces.last().unwrap();
        let mut held = table();
        let now = Instant::now();
        assert_eq!(held.accept(tail, now), Ok(Accepted::Pending));
        let flags = u16::from_be_bytes([tail[6], tail[7]]);
        let header = ((tail[0] & 0xf) as usize) * 4;
        let total = ((flags & 0x1fff) as usize) * 8 + (tail.len() - header);
        let mut beyond = pieces[0].clone();
        beyond[6..8].copy_from_slice(&(0x2000 | (total / 8) as u16).to_be_bytes());
        assert_eq!(held.accept(&beyond, now), Err(Reject::Overlap));
        assert!(held.table.contexts.is_empty());
        assert_eq!(held.table.charged, 0);
    }

    #[test]
    fn an_atomic_ipv6_fragment_completes_at_once() {
        let packet = datagram(true, 100);
        let pieces = fragments(&packet, 1500);
        assert_eq!(pieces.len(), 1);
        let mut table = table();
        assert_eq!(
            table.accept(&pieces[0], Instant::now()),
            Ok(Accepted::Complete(packet))
        );
        assert!(table.table.contexts.is_empty());
    }

    #[test]
    fn an_unfragmented_packet_is_not_mistaken_for_one() {
        let mut table = table();
        let now = Instant::now();
        assert!(matches!(
            table.accept(&datagram(false, 100), now),
            Err(Reject::Malformed(_))
        ));
        assert!(matches!(
            table.accept(&datagram(true, 100), now),
            Err(Reject::Malformed(_))
        ));
        assert!(matches!(table.accept(&[], now), Err(Reject::Malformed(_))));
    }

    #[test]
    fn a_datagram_missing_fragment_zero_is_refused() {
        let packet = datagram(false, 2000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        for piece in &pieces[1..] {
            let _ = table.accept(piece, now);
        }
        assert!(!table.table.contexts.is_empty());
        assert!(table.table.charged > 0);
    }

    #[test]
    fn the_ceiling_refuses_growth_and_the_charge_is_returned() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let now = Instant::now();
        let mut probe = table();
        assert_eq!(probe.accept(&pieces[0], now), Ok(Accepted::Pending));
        let mut table = Fixture::with_cap(probe.table.charged);
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        let held = table.table.charged;
        assert!(held > 0);
        assert_eq!(table.accept(&pieces[1], now), Err(Reject::Denied));
        assert_eq!(table.table.charged, held);
        assert_eq!(table.table.contexts.len(), 1);
    }

    fn ipv4_fragment(offset: usize, more: bool, payload: usize) -> Vec<u8> {
        let mut packet = vec![0u8; IPV4_HEADER_LEN + payload];
        packet[0] = 0x45;
        let total = u16::try_from(packet.len()).unwrap();
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        packet[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
        let flags = u16::try_from(offset / 8).unwrap() | if more { 0x2000 } else { 0 };
        packet[6..8].copy_from_slice(&flags.to_be_bytes());
        packet[8] = 64;
        packet[9] = IpNumber::UDP.0;
        packet[12..16].copy_from_slice(&[192, 0, 2, 1]);
        packet[16..20].copy_from_slice(&[198, 51, 100, 7]);
        packet
    }

    fn ipv6_fragment(offset: usize, more: bool, payload: usize) -> Vec<u8> {
        let mut packet = vec![0u8; IPV6_HEADER_LEN + IPV6_FRAGMENT_HEADER_LEN + payload];
        packet[0] = 0x60;
        let length = u16::try_from(packet.len() - IPV6_HEADER_LEN).unwrap();
        packet[4..6].copy_from_slice(&length.to_be_bytes());
        packet[6] = IpNumber::IPV6_FRAGMENTATION_HEADER.0;
        packet[7] = 64;
        packet[8..24].copy_from_slice(&Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2).octets());
        packet[24..40]
            .copy_from_slice(&Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111).octets());
        packet[40] = IpNumber::UDP.0;
        let control = u16::try_from(offset).unwrap() | if more { 1 } else { 0 };
        packet[42..44].copy_from_slice(&control.to_be_bytes());
        packet[44..48].copy_from_slice(&0x9abcdef0u32.to_be_bytes());
        packet
    }

    #[test]
    fn an_empty_non_atomic_fragment_is_refused_in_both_families() {
        let mut table = table();
        let now = Instant::now();
        for packet in [
            ipv4_fragment(0, true, 0),
            ipv4_fragment(1480, false, 0),
            ipv6_fragment(0, true, 0),
            ipv6_fragment(1448, false, 0),
        ] {
            assert!(matches!(
                table.accept(&packet, now),
                Err(Reject::Malformed(_))
            ));
        }
        assert!(table.table.contexts.is_empty());
        assert_eq!(table.table.charged, 0);
    }

    #[test]
    fn a_misaligned_non_final_fragment_is_refused_in_both_families() {
        let mut table = table();
        let now = Instant::now();
        for payload in [1usize, 7, 9, 15, 1449] {
            for packet in [
                ipv4_fragment(0, true, payload),
                ipv6_fragment(0, true, payload),
            ] {
                assert!(
                    matches!(table.accept(&packet, now), Err(Reject::Malformed(_))),
                    "{payload} bytes"
                );
            }
        }
        assert_eq!(
            table.accept(&ipv4_fragment(8, false, 9), now),
            Ok(Accepted::Pending)
        );
        assert_eq!(
            table.accept(&ipv6_fragment(8, false, 9), now),
            Ok(Accepted::Pending)
        );
    }

    #[test]
    fn a_sparse_pattern_is_charged_for_the_ranges_it_opens() {
        let mut sparse = table();
        let now = Instant::now();
        for slot in 0..64 {
            assert_eq!(
                sparse.accept(&ipv4_fragment(slot * 16, true, 8), now),
                Ok(Accepted::Pending)
            );
        }
        let context = sparse.table.contexts.values().next().unwrap();
        assert_eq!(context.received.len(), 64);
        let mut dense = table();
        assert_eq!(
            dense.accept(&ipv4_fragment(0, true, 63 * 16 + 8), now),
            Ok(Accepted::Pending)
        );
        assert!(
            sparse.table.charged > dense.table.charged,
            "sparse {} dense {}",
            sparse.table.charged,
            dense.table.charged
        );
        assert_eq!(
            sparse.table.charged,
            sparse
                .table
                .contexts
                .values()
                .map(|c| c.charged)
                .sum::<u64>()
        );
    }

    #[test]
    fn a_context_is_charged_for_its_buffers_and_its_row_only_once() {
        let now = Instant::now();
        let mut fixture = table();
        assert_eq!(
            fixture.accept(&ipv4_fragment(0, true, 1200), now),
            Ok(Accepted::Pending)
        );
        let context = fixture.table.contexts.values().next().expect("held");
        assert_eq!(
            context.charged,
            context.payload.capacity() as u64
                + (context.received.capacity() * std::mem::size_of::<Range<usize>>()) as u64,
            "a context holds its two buffers and nothing else"
        );
        assert_eq!(
            Table::footprint(fixture.table.prepared).expect("chargeable"),
            (fixture.table.prepared * std::mem::size_of::<(Key, Context)>()) as u64
                + std::mem::size_of::<Table>() as u64,
            "the fixed lease owns the rows"
        );
    }

    #[test]
    fn the_ceiling_binds_on_a_sparse_pattern() {
        let now = Instant::now();
        let mut probe = table();
        assert_eq!(
            probe.accept(&ipv4_fragment(0, true, 8), now),
            Ok(Accepted::Pending)
        );
        let mut table = Fixture::with_cap(probe.table.charged);
        assert_eq!(
            table.accept(&ipv4_fragment(0, true, 8), now),
            Ok(Accepted::Pending)
        );
        assert_eq!(
            table.accept(&ipv4_fragment(16, true, 8), now),
            Err(Reject::Denied)
        );
        assert_eq!(table.table.contexts.len(), 1);
    }

    #[test]
    fn every_cleanup_path_returns_the_whole_charge() {
        let now = Instant::now();
        for retire in [true, false] {
            let mut table = table();
            for slot in 0..8 {
                assert_eq!(
                    table.accept(&ipv4_fragment(slot * 16, true, 8), now),
                    Ok(Accepted::Pending)
                );
            }
            assert!(table.table.charged > 0);
            if retire {
                table.retire();
            } else {
                table.sweep(now + REASSEMBLY_TIMEOUT, |_| {});
            }
            assert!(table.table.contexts.is_empty());
            assert_eq!(table.table.charged, 0);
        }
    }

    #[test]
    fn a_merge_allocates_once_at_the_projected_capacity() {
        for old_len in [0usize, 1, 2, 3, 4, 7, 8, 9, 16, 33] {
            let old: Vec<Range<usize>> = (0..old_len).map(|i| (i * 100)..(i * 100 + 8)).collect();
            let new = (old_len * 100 + 500)..(old_len * 100 + 508);
            let merged = merge_ranges(&old, new.clone());
            assert_eq!(merged.len(), old_len + 1);
            assert_eq!(
                merged.capacity(),
                old_len + 1,
                "old {old_len}: the replacement is exactly the projected size, not a doubling"
            );
            for pair in merged.windows(2) {
                assert!(pair[0].end < pair[1].start, "old {old_len}: not disjoint");
            }
            assert!(merged.contains(&new));
        }

        let old = vec![0..8, 16..24];
        let merged = merge_ranges(&old, 8..16);
        assert_eq!(merged, vec![0..24]);
        assert_eq!(merged.capacity(), 3);

        let before = merge_ranges(&[], 16..24);
        assert_eq!(merge_ranges(&before, 0..8), vec![0..8, 16..24]);
        let after = merge_ranges(&[], 0..8);
        assert_eq!(merge_ranges(&after, 16..24), vec![0..8, 16..24]);
        assert_eq!(
            merge_ranges(&[0..8, 32..40], 16..24),
            vec![0..8, 16..24, 32..40]
        );
    }

    #[test]
    fn completion_holds_its_charge_until_the_packet_exists() {
        let payload = vec![0x5au8; 2_000];
        let header = Header::new(&[0x45u8; 20]).expect("a header");
        let context = Context {
            payload: payload.clone(),
            received: merge_ranges(&[], 0..payload.len()),
            header: Some(header),
            total: Some(payload.len()),
            deadline: Instant::now(),
            charged: 4_096,
        };
        let (assembled, charged) = complete(context, &header);
        assert_eq!(assembled.len(), 20 + payload.len());
        assert_eq!(charged, 4_096);

        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        let mut completed = None;
        for piece in &pieces {
            match table.accept(piece, now).expect("accepted") {
                Accepted::Pending => assert!(
                    table.table.charged > 0,
                    "a held context is charged for what it holds"
                ),
                Accepted::Complete(whole) => completed = Some(whole),
            }
        }
        assert_eq!(completed.as_deref(), Some(packet.as_slice()));
        assert_eq!(table.table.charged, 0);
        assert_eq!(table.table.counters.undercharged, 0);
    }

    #[test]
    fn a_reconciliation_that_would_saturate_fails_closed_instead() {
        assert_eq!(reconcile(1_000, 400), Some(600));
        assert_eq!(reconcile(1_000, 1_000), Some(0));
        assert_eq!(reconcile(1_000, 1_001), None);
        assert_eq!(reconcile(0, 1), None);
        assert_eq!(reconcile(u64::MAX, u64::MAX), Some(0));
    }

    #[test]
    fn an_undercharged_context_is_discarded_rather_than_continued() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        table.accept(&pieces[0], now).expect("the first fits");
        table.accept(&pieces[1], now).expect("and the second");
        assert_eq!(table.table.contexts.len(), 1);
        let held = table.table.charged;
        assert!(held > 0);
        let charged = table.admission.bytes_charged();
        let key = *table.table.contexts.keys().next().expect("held");

        let refused = table
            .table
            .undercharged(key, &mut table.admission, &table.lease);
        assert_eq!(refused, Reject::Denied);
        assert_eq!(
            table.table.counters.undercharged, 1,
            "the violation is counted rather than absorbed"
        );
        assert!(
            table.table.contexts.is_empty(),
            "and the context is gone rather than left holding more than was granted"
        );
        assert_eq!(
            table.table.charged, 0,
            "with its whole reservation given back"
        );
        assert_eq!(table.admission.bytes_charged(), charged - held);
        assert_eq!(table.admission.invariant_violations(), 0);

        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        assert!(table.table.charged > 0);
    }

    #[test]
    fn a_completed_context_frees_its_slot_for_the_next_identification() {
        let mut fixture = table();
        let prepared = fixture.table.prepared;
        let packet = datagram(true, 3_000);
        let now = Instant::now();
        let pieces = |identification: u32| -> Vec<Vec<u8>> {
            let mut pieces = Vec::new();
            fragment_ipv6(&packet, 1_280, identification, |piece| pieces.push(piece))
                .expect("a fragmentable datagram");
            assert!(pieces.len() > 2, "{} pieces", pieces.len());
            pieces
        };

        let mut live: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut identification = 1u32;
        while fixture.table.contexts.len() < prepared {
            let mut fragments = pieces(identification);
            identification += 1;
            assert_eq!(fixture.accept(&fragments[0], now), Ok(Accepted::Pending));
            fragments.remove(0);
            live.push(fragments);
        }
        assert_eq!(fixture.table.contexts.len(), prepared);

        let extra = pieces(identification);
        identification += 1;
        assert_eq!(fixture.accept(&extra[0], now), Err(Reject::Denied));
        assert_eq!(
            fixture.table.contexts.len(),
            prepared,
            "nothing was evicted"
        );

        let finishing = live.remove(0);
        let mut completed = false;
        for piece in &finishing {
            match fixture
                .accept(piece, now)
                .expect("a held context takes its own pieces")
            {
                Accepted::Pending => {}
                Accepted::Complete(_) => completed = true,
            }
        }
        assert!(
            completed,
            "the datagram was reassembled and its row released"
        );
        assert_eq!(
            fixture.table.contexts.len(),
            prepared - 1,
            "the slot came back"
        );
        let newcomer = pieces(identification);
        assert_eq!(
            fixture.accept(&newcomer[0], now),
            Ok(Accepted::Pending),
            "and the next identification takes it"
        );
        assert_eq!(fixture.table.contexts.len(), prepared);
        assert_eq!(
            fixture.table.prepared, prepared,
            "with the bound the charge covers where it was"
        );
        for fragments in &live {
            for piece in fragments {
                assert!(
                    fixture.accept(piece, now).is_ok(),
                    "a held context takes its own pieces"
                );
            }
        }

        fixture.retire();
        assert_eq!(fixture.table.charged, 0);
        assert_eq!(fixture.admission.invariant_violations(), 0);
    }

    #[test]
    fn a_denied_fragment_zero_allocates_and_retains_nothing() {
        for ipv6 in [false, true] {
            let mut table = Fixture::with_cap(8);
            let packet = datagram(ipv6, 3000);
            let pieces = fragments(&packet, 1280);
            let charged = table.admission.bytes_charged();
            let now = Instant::now();
            assert_eq!(table.accept(&pieces[0], now), Err(Reject::Denied));
            assert!(table.table.contexts.is_empty(), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0, "ipv6 {ipv6}");
            assert_eq!(
                table.admission.bytes_charged(),
                charged,
                "a refusal charges nothing, ipv6 {ipv6}"
            );
            assert_eq!(table.table.counters.undercharged, 0);
        }
    }

    #[test]
    fn a_denial_preserves_the_context_it_could_not_grow() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut probe = Fixture::with_cap(1 << 20);
        let now = Instant::now();
        probe.accept(&pieces[0], now).expect("the first fits");
        let held = probe.table.charged;

        let mut table = Fixture::with_cap(held);
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        assert_eq!(table.table.charged, held);
        let contexts = table.table.contexts.len();
        assert_eq!(table.accept(&pieces[1], now), Err(Reject::Denied));
        assert_eq!(table.table.contexts.len(), contexts);
        assert_eq!(table.table.charged, held);
        assert_eq!(table.table.contexts.values().next().unwrap().charged, held);
    }

    #[test]
    fn no_offset_pattern_ever_allocates_past_its_reservation() {
        for ipv6 in [false, true] {
            for mtu in [576usize, 1280, 1500] {
                for reverse in [false, true] {
                    for size in [8usize, 100, 3000, 20_000] {
                        let packet = datagram(ipv6, size);
                        let mut pieces = fragments(&packet, mtu);
                        if reverse {
                            pieces.reverse();
                        }
                        let mut table = Fixture::with_cap(1 << 22);
                        let now = Instant::now();
                        for piece in &pieces {
                            let _ = table.accept(piece, now);
                            assert_eq!(
                                table.table.counters.undercharged, 0,
                                "ipv6 {ipv6} mtu {mtu} reverse {reverse} size {size}"
                            );
                            assert_eq!(
                                table.table.charged,
                                table
                                    .table
                                    .contexts
                                    .values()
                                    .map(|context| context.charged)
                                    .sum::<u64>(),
                                "ipv6 {ipv6} mtu {mtu} reverse {reverse} size {size}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_sparse_pattern_charges_the_old_and_new_range_storage_at_once() {
        let packet = datagram(false, 4000);
        let pieces = fragments(&packet, 600);
        let mut table = Fixture::with_cap(1 << 22);
        let now = Instant::now();
        for piece in pieces.iter().step_by(2) {
            assert_eq!(table.accept(piece, now), Ok(Accepted::Pending));
        }
        let context = table.table.contexts.values().next().expect("held");
        assert!(
            context.received.len() > 1,
            "the pattern must actually be sparse: {} ranges",
            context.received.len()
        );
        let range = std::mem::size_of::<Range<usize>>() as u64;
        let projection = context.project(context.payload.len(), false).expect("fits");
        assert!(
            projection.peak >= context.received.capacity() as u64 * range,
            "the peak covers the range list being replaced"
        );
        assert_eq!(table.table.counters.undercharged, 0);
    }

    #[test]
    fn payload_growth_and_completion_charge_both_buffers_at_once() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = Fixture::with_cap(1 << 22);
        let now = Instant::now();
        table.accept(&pieces[0], now).expect("the first fits");

        let context = table.table.contexts.values().next().expect("held");
        let held = context.payload.capacity() as u64;
        assert!(held > 0);
        let growing = context
            .project(context.payload.capacity() + 1, false)
            .expect("fits");
        assert!(
            growing.peak >= held,
            "the peak covers the payload being replaced: {} < {held}",
            growing.peak
        );
        let completing = context
            .project(context.payload.capacity() + 1, true)
            .expect("fits");
        assert!(
            completing.peak > growing.peak,
            "completion costs more than growth alone"
        );

        for piece in &pieces[1..] {
            if let Ok(Accepted::Complete(whole)) = table.accept(piece, now) {
                assert_eq!(whole, packet);
            }
        }
        assert_eq!(table.table.charged, 0);
        assert!(table.table.contexts.is_empty());
        assert_eq!(table.table.counters.undercharged, 0);
    }

    #[test]
    fn simultaneous_expiries_never_hold_more_than_one_quote() {
        let mut table = table();
        let now = Instant::now();
        let mut opened = 0u64;
        for id in 0..64u16 {
            let mut packet = datagram(false, 3000);
            packet[4..6].copy_from_slice(&id.to_be_bytes());
            let pieces = fragments(&packet, 1280);
            if table.accept(&pieces[0], now) != Ok(Accepted::Pending) {
                break;
            }
            opened += 1;
        }
        assert!(opened > 1);

        let mut live = 0usize;
        let mut peak = 0usize;
        let mut quoted = 0u64;
        let retired = table.sweep(now + REASSEMBLY_TIMEOUT, |quote| {
            live += 1;
            peak = peak.max(live);
            quoted += 1;
            assert!(!quote.is_empty());
            drop(quote);
            live -= 1;
        });
        assert_eq!(retired, opened);
        assert_eq!(quoted, opened);
        assert_eq!(peak, 1);
        assert_eq!(table.table.charged, 0);
        assert!(table.table.contexts.is_empty());
    }

    #[test]
    fn an_expired_context_yields_fragment_zero_and_frees_its_bytes() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        assert_eq!(table.table.next_deadline(), Some(now + REASSEMBLY_TIMEOUT));
        assert_eq!(table.sweep(now, |_| panic!("not due yet")), 0);
        let mut expired = Vec::new();
        assert_eq!(
            table.sweep(now + REASSEMBLY_TIMEOUT, |quote| expired.push(quote)),
            1
        );
        assert_eq!(expired.len(), 1);
        let quote = &expired[0];
        assert_eq!(&quote[12..16], &packet[12..16]);
        assert_eq!(u16::from_be_bytes([quote[6], quote[7]]) & 0x3fff, 0);
        assert_eq!(table.table.charged, 0);
        assert!(table.table.contexts.is_empty());
        assert_eq!(table.table.next_deadline(), None);
    }

    #[test]
    fn contexts_for_different_identifications_do_not_mix() {
        let packet = datagram(false, 2000);
        let first = fragments(&packet, 1280);
        let mut second = first.clone();
        for piece in &mut second {
            piece[4..6].copy_from_slice(&0x7777u16.to_be_bytes());
        }
        let mut table = table();
        let now = Instant::now();
        assert_eq!(table.accept(&first[0], now), Ok(Accepted::Pending));
        assert_eq!(table.accept(&second[0], now), Ok(Accepted::Pending));
        assert_eq!(table.table.contexts.len(), 2);
        assert!(matches!(
            table.accept(&first[1], now),
            Ok(Accepted::Complete(_))
        ));
        assert_eq!(table.table.contexts.len(), 1);
    }
}
