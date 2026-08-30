//! IPv4 and IPv6 ingress reassembly with protocol-derived size and lifetime bounds.
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
#[cfg(test)]
use std::net::Ipv6Addr;
use std::ops::Range;
use std::time::{Duration, Instant};

use etherparse::{IpNumber, Ipv4Header, Ipv6FragmentHeaderSlice};

use crate::shared::ip_wire::Packet;
#[cfg(test)]
use crate::shared::packet_writer::{IPV4_HEADER_LEN, IPV6_FRAGMENT_HEADER_LEN, IPV6_HEADER_LEN};

/// Android/Linux IPv4's `IP_FRAG_TIME`. Expiry discards the incomplete context (and yields fragment zero to
/// the owner when available); a later fragment starts a fresh context.
/// https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/include/net/ip.h#146
const IPV4_REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Android/Linux IPv6's `IPV6_FRAG_TIMEOUT`, with the same expiry behavior as IPv4.
/// https://android.googlesource.com/kernel/common/+/refs/tags/android15-6.6-2025-09_r7/include/net/ipv6.h#548
const IPV6_REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(60);

/// Widest length representable by IPv4's 16-bit Total Length or IPv6's 16-bit Payload Length field. IPv4
/// reassembly also subtracts its actual header length before accepting that many body bytes. A fragment beyond
/// the applicable bound is rejected without growing its context; an IPv4 fragment-zero header that makes
/// already-retained body bytes exceed Total Length discards that context.
/// <https://www.rfc-editor.org/rfc/rfc791.html#section-3.1>
/// <https://www.rfc-editor.org/rfc/rfc8200.html#section-3>
/// <https://www.rfc-editor.org/rfc/rfc8200.html#section-4.5>
const MAX_ENCODED_LENGTH: usize = u16::MAX as usize;

/// Largest IPv4 header representable by the four-bit IHL field: 15 32-bit words. A larger claimed header is
/// malformed and no reassembly context is retained.
/// <https://www.rfc-editor.org/rfc/rfc791.html#section-3.1>
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
}

impl Context {
    fn complete(&self) -> bool {
        match (self.total, self.received.as_slice()) {
            (Some(total), [only]) => *only == (0..total),
            _ => false,
        }
    }
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

    fn len(&self) -> usize {
        self.length as usize
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
    expired: u64,
    headless: u64,
}

#[derive(Default)]
pub struct Table {
    contexts: HashMap<Key, Context>,
    counters: Counters,
}

impl Table {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn accept(&mut self, packet: &[u8], now: Instant) -> Result<Accepted, Reject> {
        let fragment = match parse(packet) {
            Ok(fragment) => fragment,
            Err(e) => {
                self.counters.malformed += 1;
                return Err(e);
            }
        };
        let end = fragment.offset + fragment.payload.len();
        let maximum_body = match fragment.key.source {
            IpAddr::V4(_) => MAX_ENCODED_LENGTH - Ipv4Header::MIN_LEN,
            IpAddr::V6(_) => MAX_ENCODED_LENGTH,
        };
        if end > maximum_body {
            self.counters.malformed += 1;
            return Err(Reject::Malformed("fragment reaches past a datagram"));
        }
        let total = (!fragment.more).then_some(end);
        if let Some(context) = self.contexts.get(&fragment.key) {
            if context.total.is_some_and(|known| {
                total.is_some_and(|total| total != known)
                    || end > known
                    || fragment.more && end == known
            }) || total.is_some_and(|total| context.payload.len() > total)
            {
                self.discard(fragment.key);
                self.counters.overlapping += 1;
                return Err(Reject::Overlap);
            }
        }
        let existing = self.contexts.get(&fragment.key);
        let header = fragment
            .header
            .as_ref()
            .or_else(|| existing.and_then(|context| context.header.as_ref()));
        let body_end = existing.map_or(end, |context| context.payload.len().max(end));
        if fragment.key.source.is_ipv4()
            && header.is_some_and(|header| header.len() + body_end > MAX_ENCODED_LENGTH)
        {
            self.discard(fragment.key);
            self.counters.malformed += 1;
            return Err(Reject::Malformed("IPv4 reassembly exceeds total length"));
        }
        let context = match self.contexts.entry(fragment.key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Context {
                payload: Vec::new(),
                received: Vec::new(),
                header: None,
                total: None,
                deadline: now
                    + match fragment.key.source {
                        IpAddr::V4(_) => IPV4_REASSEMBLY_TIMEOUT,
                        IpAddr::V6(_) => IPV6_REASSEMBLY_TIMEOUT,
                    },
            }),
        };
        if !insert(context, fragment.offset, fragment.payload) {
            self.discard(fragment.key);
            self.counters.overlapping += 1;
            return Err(Reject::Overlap);
        }
        if let Some(header) = fragment.header {
            context.header = Some(header);
        }
        if let Some(total) = total {
            context.total = Some(total);
        }
        if !context.complete() {
            self.counters.held += 1;
            return Ok(Accepted::Pending);
        }
        let context = self.contexts.remove(&fragment.key).expect("just held");
        let Some(header) = context.header else {
            self.counters.headless += 1;
            return Err(Reject::Malformed("reassembled without fragment zero"));
        };
        let assembled = assemble(header.as_slice(), &context.payload);
        self.counters.completed += 1;
        Ok(Accepted::Complete(assembled))
    }

    pub fn retire(&mut self) {
        self.contexts = HashMap::new();
    }

    fn discard(&mut self, key: Key) {
        self.contexts.remove(&key);
    }

    pub fn sweep(&mut self, now: Instant, mut quote: impl FnMut(Vec<u8>)) -> u64 {
        let mut retired = 0u64;
        let mut quoted = 0u64;
        self.contexts.retain(|_, context| {
            if context.deadline > now {
                return true;
            }
            retired += 1;
            if let Some(header) = &context.header {
                quoted += 1;
                quote(assemble(header.as_slice(), &context.payload));
            }
            false
        });
        self.counters.expired += retired;
        self.counters.headless += retired - quoted;
        retired
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.contexts.values().map(|context| context.deadline).min()
    }

    pub fn describe(&self) -> String {
        format!(
            "{} contexts, held {} completed {} malformed {} overlapping {} expired {} headless {}",
            self.contexts.len(),
            self.counters.held,
            self.counters.completed,
            self.counters.malformed,
            self.counters.overlapping,
            self.counters.expired,
            self.counters.headless
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

fn assemble(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(header.len() + payload.len());
    packet.extend_from_slice(header);
    packet.extend_from_slice(payload);
    match packet.first().map(|byte| byte >> 4) {
        Some(4) => {
            let length = u16::try_from(packet.len())
                .expect("IPv4 reassembly length was validated before insertion");
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
            let length = u16::try_from(payload.len())
                .expect("IPv6 reassembly length was validated before insertion");
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
        table: Table,
    }

    impl Fixture {
        fn accept(&mut self, packet: &[u8], now: Instant) -> Result<Accepted, Reject> {
            self.table.accept(packet, now)
        }

        fn sweep(&mut self, now: Instant, quote: impl FnMut(Vec<u8>)) -> u64 {
            self.table.sweep(now, quote)
        }
    }

    fn table() -> Fixture {
        Fixture {
            table: Table::new(),
        }
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
    fn ipv4_reassembly_accepts_the_largest_encodable_total_length() {
        let mut table = table();
        let now = Instant::now();
        assert_eq!(
            table.accept(&ipv4_fragment(0, true, 65_512), now),
            Ok(Accepted::Pending)
        );
        let Accepted::Complete(packet) = table
            .accept(&ipv4_fragment(65_512, false, 3), now)
            .expect("the maximum-length datagram fits")
        else {
            panic!("the two contiguous fragments should complete")
        };
        assert_eq!(packet.len(), MAX_ENCODED_LENGTH);
        assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), u16::MAX);
    }

    #[test]
    fn ipv4_reassembly_rejects_a_body_that_only_exceeds_the_total_with_its_header() {
        let now = Instant::now();
        for reverse in [false, true] {
            let plain = ipv4_fragment(0, true, 8);
            let mut zero = Vec::with_capacity(plain.len() + 4);
            zero.extend_from_slice(&plain[..IPV4_HEADER_LEN]);
            zero.extend_from_slice(&[0; 4]);
            zero.extend_from_slice(&plain[IPV4_HEADER_LEN..]);
            zero[0] = 0x46;
            let total = zero.len() as u16;
            zero[2..4].copy_from_slice(&total.to_be_bytes());
            let tail = ipv4_fragment(65_504, false, 11);
            let mut table = table();
            let (first, second) = if reverse {
                (&tail, &zero)
            } else {
                (&zero, &tail)
            };
            assert_eq!(table.accept(first, now), Ok(Accepted::Pending));
            assert_eq!(
                table.accept(second, now),
                Err(Reject::Malformed("IPv4 reassembly exceeds total length"))
            );
            assert!(table.table.contexts.is_empty());
        }
    }

    #[test]
    fn headless_ipv4_reassembly_reserves_space_for_the_minimum_header() {
        let mut table = table();
        assert_eq!(
            table.accept(&ipv4_fragment(65_504, false, 12), Instant::now()),
            Err(Reject::Malformed("fragment reaches past a datagram"))
        );
        assert!(table.table.contexts.is_empty());
    }

    #[test]
    fn a_non_final_fragment_cannot_end_at_an_already_declared_total() {
        let mut table = table();
        let now = Instant::now();
        assert_eq!(
            table.accept(&ipv4_fragment(16, false, 8), now),
            Ok(Accepted::Pending)
        );
        assert_eq!(
            table.accept(&ipv4_fragment(0, true, 24), now),
            Err(Reject::Overlap)
        );
        assert!(table.table.contexts.is_empty());
    }

    #[test]
    fn a_final_fragment_cannot_shrink_below_previously_held_data() {
        let mut table = table();
        let now = Instant::now();
        assert_eq!(
            table.accept(&ipv4_fragment(16, true, 8), now),
            Ok(Accepted::Pending)
        );
        assert_eq!(
            table.accept(&ipv4_fragment(8, false, 8), now),
            Err(Reject::Overlap)
        );
        assert!(table.table.contexts.is_empty());
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
        let retired = table.sweep(now + IPV4_REASSEMBLY_TIMEOUT, |quote| {
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
        assert!(table.table.contexts.is_empty());
    }

    #[test]
    fn an_expired_context_yields_fragment_zero_and_is_removed() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        assert_eq!(
            table.table.next_deadline(),
            Some(now + IPV4_REASSEMBLY_TIMEOUT)
        );
        assert_eq!(table.sweep(now, |_| panic!("not due yet")), 0);
        let mut expired = Vec::new();
        assert_eq!(
            table.sweep(now + IPV4_REASSEMBLY_TIMEOUT, |quote| expired.push(quote)),
            1
        );
        assert_eq!(expired.len(), 1);
        let quote = &expired[0];
        assert_eq!(&quote[12..16], &packet[12..16]);
        assert_eq!(u16::from_be_bytes([quote[6], quote[7]]) & 0x3fff, 0);
        assert!(table.table.contexts.is_empty());
        assert_eq!(table.table.next_deadline(), None);
    }

    #[test]
    fn context_deadlines_follow_the_android_kernel_default_for_each_family() {
        let now = Instant::now();
        for (ipv6, timeout) in [
            (false, IPV4_REASSEMBLY_TIMEOUT),
            (true, IPV6_REASSEMBLY_TIMEOUT),
        ] {
            let packet = datagram(ipv6, 3000);
            let pieces = fragments(&packet, 1280);
            let mut table = table();
            assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
            assert_eq!(table.table.next_deadline(), Some(now + timeout));
            assert_eq!(
                table.sweep(now + timeout - Duration::from_nanos(1), |_| {}),
                0
            );
            assert_eq!(table.sweep(now + timeout, |_| {}), 1);
        }
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
