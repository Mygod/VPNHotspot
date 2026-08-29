use std::net::IpAddr;
#[cfg(test)]
use std::net::Ipv6Addr;
use std::time::Instant;

use crate::shared::ip_wire::{Error as IpError, Packet};
use crate::shared::ipv4_identification::{
    Denial, Guarded, Ipv4Identifications, Prepared, Terminal, Tuple,
};

use etherparse::{IpFragOffset, Ipv4Header};

pub const IPV4_HEADER_LEN: usize = 20;
pub const IPV6_HEADER_LEN: usize = 40;
pub const IPV6_FRAGMENT_HEADER_LEN: usize = 8;

const UNFRAGMENTABLE_NEXT_HEADERS: [u8; 3] = [0, 43, 60];
const IPV6_FRAGMENT_NEXT_HEADER: u8 = 44;

const FRAGMENT_ALIGNMENT: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum WriterError {
    Malformed(&'static str),
    TooLarge { size: usize, mtu: usize },
    Unfragmentable(&'static str),
}

pub fn validate(packet: &[u8], mtu: usize) -> Result<(), WriterError> {
    Packet::parse(packet).map_err(|error| {
        WriterError::Malformed(match error {
            IpError::Ipv4Header => "IPv4 packet shorter than its header",
            IpError::Ipv4Length => "IPv4 total length disagrees",
            IpError::Ipv6Header => "IPv6 packet shorter than its header",
            IpError::Ipv6Length => "IPv6 payload length disagrees",
            IpError::NotIp => "not an IPv4 or IPv6 packet",
        })
    })?;
    if packet.len() > mtu {
        return Err(WriterError::TooLarge {
            size: packet.len(),
            mtu,
        });
    }
    Ok(())
}

pub fn fragment_ipv6(
    packet: &[u8],
    mtu: usize,
    identification: u32,
    mut emit: impl FnMut(Vec<u8>),
) -> Result<usize, WriterError> {
    if packet.len() < IPV6_HEADER_LEN || packet.first().map(|byte| byte >> 4) != Some(6) {
        return Err(WriterError::Malformed("not an IPv6 packet"));
    }
    let next_header = packet[6];
    if UNFRAGMENTABLE_NEXT_HEADERS.contains(&next_header) {
        return Err(WriterError::Unfragmentable(
            "leading extension header belongs to the unfragmentable part",
        ));
    }
    if next_header == IPV6_FRAGMENT_NEXT_HEADER {
        return Err(WriterError::Unfragmentable("already fragmented"));
    }
    let overhead = IPV6_HEADER_LEN + IPV6_FRAGMENT_HEADER_LEN;
    let per_fragment = mtu
        .checked_sub(overhead)
        .filter(|room| *room >= FRAGMENT_ALIGNMENT)
        .ok_or(WriterError::TooLarge {
            size: overhead + FRAGMENT_ALIGNMENT,
            mtu,
        })?
        / FRAGMENT_ALIGNMENT
        * FRAGMENT_ALIGNMENT;
    let payload = &packet[IPV6_HEADER_LEN..];
    let mut emitted = 0usize;
    let mut offset = 0;
    while offset < payload.len() {
        let end = (offset + per_fragment).min(payload.len());
        let more = end < payload.len();
        let mut fragment = Vec::with_capacity(overhead + end - offset);
        fragment.extend_from_slice(&packet[..IPV6_HEADER_LEN]);
        fragment[4..6]
            .copy_from_slice(&((IPV6_FRAGMENT_HEADER_LEN + end - offset) as u16).to_be_bytes());
        fragment[6] = IPV6_FRAGMENT_NEXT_HEADER;
        fragment.push(next_header);
        fragment.push(0);
        fragment.extend_from_slice(&(((offset as u16) & !7) | u16::from(more)).to_be_bytes());
        fragment.extend_from_slice(&identification.to_be_bytes());
        fragment.extend_from_slice(&payload[offset..end]);
        emit(fragment);
        emitted += 1;
        offset = end;
    }
    if emitted == 0 {
        return Err(WriterError::Malformed("IPv6 datagram has no payload"));
    }
    Ok(emitted)
}

pub fn fragment_ipv4(
    packet: &[u8],
    mtu: usize,
    mut emit: impl FnMut(Vec<u8>),
) -> Result<usize, WriterError> {
    let (mut header, payload) =
        Ipv4Header::from_slice(packet).map_err(|_| WriterError::Malformed("not an IPv4 packet"))?;
    if !header.options.is_empty() {
        return Err(WriterError::Unfragmentable(
            "IPv4 options are not copied into fragments",
        ));
    }
    if header.dont_fragment {
        return Err(WriterError::Unfragmentable("DF is set"));
    }
    if header.more_fragments || header.fragment_offset != IpFragOffset::ZERO {
        return Err(WriterError::Unfragmentable("already fragmented"));
    }
    let per_fragment = mtu
        .checked_sub(IPV4_HEADER_LEN)
        .filter(|room| *room >= FRAGMENT_ALIGNMENT)
        .ok_or(WriterError::TooLarge {
            size: IPV4_HEADER_LEN + FRAGMENT_ALIGNMENT,
            mtu,
        })?
        / FRAGMENT_ALIGNMENT
        * FRAGMENT_ALIGNMENT;
    let mut emitted = 0usize;
    let mut offset = 0;
    while offset < payload.len() {
        let end = (offset + per_fragment).min(payload.len());
        header.more_fragments = end < payload.len();
        header.fragment_offset = IpFragOffset::try_new((offset / FRAGMENT_ALIGNMENT) as u16)
            .map_err(|_| WriterError::TooLarge {
                size: packet.len(),
                mtu,
            })?;
        header
            .set_payload_len(end - offset)
            .map_err(|_| WriterError::Malformed("IPv4 fragment payload does not fit"))?;
        header.header_checksum = header.calc_header_checksum();
        let mut fragment = Vec::with_capacity(IPV4_HEADER_LEN + end - offset);
        fragment.extend_from_slice(&header.to_bytes());
        fragment.extend_from_slice(&payload[offset..end]);
        emit(fragment);
        emitted += 1;
        offset = end;
    }
    if emitted == 0 {
        return Err(WriterError::Malformed("IPv4 datagram has no payload"));
    }
    Ok(emitted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sizing {
    Atomic,
    Fragmentable(Guarded),
    Denied(Denial),
}

#[derive(Debug, Clone, Copy)]
pub struct Guarding {
    pub tuple: Option<Tuple>,
    pub oversized: bool,
    pub now: Instant,
}

pub fn size_policy(identifications: &mut Ipv4Identifications, guarding: Guarding) -> Sizing {
    let Guarding {
        tuple,
        oversized,
        now,
    } = guarding;
    let Some(tuple) = tuple.filter(|_| oversized) else {
        return Sizing::Atomic;
    };
    match identifications.next(tuple, now) {
        Ok(guarded) => Sizing::Fragmentable(guarded),
        Err(denial) => Sizing::Denied(denial),
    }
}

pub trait Sink {
    fn packet(&mut self, packet: Vec<u8>, guarded: Option<Guarded>) -> bool;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Emitted {
    Written { written: usize, blocked: usize },
    Denied(Denial),
    Unbuildable(WriterError),
}

pub fn emit<S: Sink>(
    identifications: &mut Ipv4Identifications,
    guarding: Guarding,
    mtu: usize,
    fragment_identification: &mut u32,
    build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
    sink: &mut S,
) -> Emitted {
    let guarded = match size_policy(identifications, guarding) {
        Sizing::Atomic => None,
        Sizing::Fragmentable(guarded) => Some(guarded),
        Sizing::Denied(denial) => return Emitted::Denied(denial),
    };
    let (emitted, accepted) = emitting(
        identifications,
        guarded,
        mtu,
        fragment_identification,
        build,
        sink,
    );
    if let Some(guarded) = guarded {
        if accepted == 0 {
            identifications.unissued(guarded);
        }
    }
    emitted
}

fn emitting<S: Sink>(
    identifications: &mut Ipv4Identifications,
    guarded: Option<Guarded>,
    mtu: usize,
    fragment_identification: &mut u32,
    build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
    sink: &mut S,
) -> (Emitted, usize) {
    let packet = match build(guarded.map(|guarded| guarded.identification())) {
        Ok(packet) => packet,
        Err(e) => return (Emitted::Unbuildable(e), 0),
    };
    let mut written = 0usize;
    let mut blocked = 0usize;
    let mut hand = |packet: Vec<u8>| {
        if let Some(guarded) = guarded {
            if identifications.register(guarded).is_err() {
                blocked += 1;
                return;
            }
        }
        if sink.packet(packet, guarded) {
            written += 1;
        } else {
            if let Some(guarded) = guarded {
                identifications.rolled_back(guarded);
            }
            blocked += 1;
        }
    };
    if packet.len() <= mtu {
        hand(packet);
        return (Emitted::Written { written, blocked }, written);
    }
    let fragmented = if packet.first().map(|byte| byte >> 4) == Some(6) {
        *fragment_identification = fragment_identification.wrapping_add(1);
        fragment_ipv6(&packet, mtu, *fragment_identification, &mut hand)
    } else {
        fragment_ipv4(&packet, mtu, &mut hand)
    };
    match fragmented {
        Ok(_) => (Emitted::Written { written, blocked }, written),
        Err(e) => (Emitted::Unbuildable(e), written),
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Addressed {
    pub source: IpAddr,
    pub destination: IpAddr,
    pub protocol: u8,
    pub size: usize,
}

pub trait Reporter {
    fn unbuildable(&mut self, source: IpAddr, destination: IpAddr, error: &WriterError);
}

pub struct Emitter {
    mtu: usize,
    identifications: Ipv4Identifications,
    fragment_identification: u32,
    written: u64,
    blocked: u64,
    unwritable: u64,
    identification_denied: u64,
}

impl Emitter {
    pub fn new(mtu: usize, prepared: Prepared) -> Self {
        Self {
            mtu,
            identifications: Ipv4Identifications::new(prepared),
            fragment_identification: 0,
            written: 0,
            blocked: 0,
            unwritable: 0,
            identification_denied: 0,
        }
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn blocked(&self) -> u64 {
        self.blocked
    }

    pub fn unwritable(&self) -> u64 {
        self.unwritable
    }

    pub fn identification_denied(&self) -> u64 {
        self.identification_denied
    }

    pub fn identifications(&self) -> &Ipv4Identifications {
        &self.identifications
    }

    pub fn terminal(&mut self, terminal: Terminal) {
        self.identifications.terminal(terminal);
    }

    pub fn wrote(&mut self, accepted: bool) {
        if accepted {
            self.written += 1;
        } else {
            self.blocked += 1;
        }
    }

    pub fn emit<S: Sink, R: Reporter>(
        &mut self,
        now: Instant,
        addressed: Addressed,
        build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
        sink: &mut S,
        reporter: &mut R,
    ) {
        let Addressed {
            source,
            destination,
            protocol,
            size,
        } = addressed;
        let tuple = match (source, destination) {
            (IpAddr::V4(source), IpAddr::V4(destination)) => Some((source, destination, protocol)),
            _ => None,
        };
        let emitted = emit(
            &mut self.identifications,
            Guarding {
                tuple,
                oversized: size > self.mtu,
                now,
            },
            self.mtu,
            &mut self.fragment_identification,
            build,
            sink,
        );
        match outcome(emitted) {
            Outcome::Wrote { written, blocked } => {
                self.written += written as u64;
                self.blocked += blocked as u64;
            }
            Outcome::Counted => self.identification_denied += 1,
            Outcome::Reported(e) => {
                self.unwritable += 1;
                reporter.unbuildable(source, destination, &e);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Wrote { written: usize, blocked: usize },
    Counted,
    Reported(WriterError),
}

pub fn outcome(emitted: Emitted) -> Outcome {
    match emitted {
        Emitted::Written { written, blocked } => Outcome::Wrote { written, blocked },
        Emitted::Denied(_) => Outcome::Counted,
        Emitted::Unbuildable(e) => Outcome::Reported(e),
    }
}

#[cfg(test)]
pub fn ipv6_header(
    source: Ipv6Addr,
    destination: Ipv6Addr,
    next_header: u8,
    hop_limit: u8,
    payload_length: usize,
) -> Vec<u8> {
    let mut header = Vec::with_capacity(IPV6_HEADER_LEN);
    header.extend_from_slice(&[0x60, 0, 0, 0]);
    header.extend_from_slice(&(payload_length as u16).to_be_bytes());
    header.push(next_header);
    header.push(hop_limit);
    header.extend_from_slice(&source.octets());
    header.extend_from_slice(&destination.octets());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;
    use std::time::Duration;

    use crate::shared::ipv4_identification::NONREUSE_WINDOW;

    fn collect_ipv6(packet: &[u8], mtu: usize, id: u32) -> Result<Vec<Vec<u8>>, WriterError> {
        let mut fragments = Vec::new();
        let emitted = fragment_ipv6(packet, mtu, id, |fragment| fragments.push(fragment))?;
        assert_eq!(emitted, fragments.len());
        Ok(fragments)
    }

    fn collect_ipv4(packet: &[u8], mtu: usize) -> Result<Vec<Vec<u8>>, WriterError> {
        let mut fragments = Vec::new();
        let emitted = fragment_ipv4(packet, mtu, |fragment| fragments.push(fragment))?;
        assert_eq!(emitted, fragments.len());
        Ok(fragments)
    }

    const MTU: usize = 1500;
    const SOURCE: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1);
    const DESTINATION: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2);

    fn datagram(payload_length: usize) -> Vec<u8> {
        let mut packet = ipv6_header(SOURCE, DESTINATION, 17, 64, payload_length);
        packet.extend((0..payload_length).map(|index| index as u8));
        packet
    }

    #[test]
    fn validate_accepts_consistent_packets() {
        assert_eq!(validate(&datagram(64), MTU), Ok(()));
    }

    #[test]
    fn validate_rejects_length_disagreement() {
        let mut packet = datagram(64);
        packet.pop();
        assert_eq!(
            validate(&packet, MTU),
            Err(WriterError::Malformed("IPv6 payload length disagrees"))
        );
    }

    #[test]
    fn validate_rejects_oversized() {
        let packet = datagram(MTU);
        assert_eq!(
            validate(&packet, MTU),
            Err(WriterError::TooLarge {
                size: packet.len(),
                mtu: MTU
            })
        );
    }

    #[test]
    fn validate_rejects_foreign_versions() {
        assert!(matches!(
            validate(&[0x50; 40], MTU),
            Err(WriterError::Malformed(_))
        ));
        assert!(matches!(validate(&[], MTU), Err(WriterError::Malformed(_))));
    }

    #[test]
    fn fragments_reassemble_to_the_original_payload() {
        let packet = datagram(4000);
        let fragments = collect_ipv6(&packet, MTU, 0xdead_beef).unwrap();
        assert!(fragments.len() > 1);
        let mut reassembled = Vec::new();
        for (index, fragment) in fragments.iter().enumerate() {
            assert_eq!(validate(fragment, MTU), Ok(()));
            assert_eq!(fragment[6], IPV6_FRAGMENT_NEXT_HEADER);
            assert_eq!(fragment[IPV6_HEADER_LEN], 17);
            let control =
                u16::from_be_bytes([fragment[IPV6_HEADER_LEN + 2], fragment[IPV6_HEADER_LEN + 3]]);
            assert_eq!(control & 1 == 1, index + 1 < fragments.len());
            assert_eq!((control & !7) as usize, reassembled.len());
            assert_eq!(
                u32::from_be_bytes(
                    fragment[IPV6_HEADER_LEN + 4..IPV6_HEADER_LEN + 8]
                        .try_into()
                        .unwrap()
                ),
                0xdead_beef
            );
            reassembled.extend_from_slice(&fragment[IPV6_HEADER_LEN + IPV6_FRAGMENT_HEADER_LEN..]);
        }
        assert_eq!(reassembled, packet[IPV6_HEADER_LEN..]);
    }

    #[test]
    fn every_fragment_but_the_last_is_eight_byte_aligned() {
        let fragments = collect_ipv6(&datagram(4000), MTU, 1).unwrap();
        for fragment in &fragments[..fragments.len() - 1] {
            assert_eq!(
                (fragment.len() - IPV6_HEADER_LEN - IPV6_FRAGMENT_HEADER_LEN) % FRAGMENT_ALIGNMENT,
                0
            );
        }
    }

    #[test]
    fn fragmenting_rejects_unfragmentable_chains() {
        for next_header in UNFRAGMENTABLE_NEXT_HEADERS
            .iter()
            .chain([IPV6_FRAGMENT_NEXT_HEADER].iter())
        {
            let mut packet = datagram(64);
            packet[6] = *next_header;
            assert!(matches!(
                collect_ipv6(&packet, MTU, 1),
                Err(WriterError::Unfragmentable(_))
            ));
        }
    }

    #[test]
    fn fragmenting_rejects_an_mtu_with_no_room() {
        assert!(matches!(
            collect_ipv6(
                &datagram(4000),
                IPV6_HEADER_LEN + IPV6_FRAGMENT_HEADER_LEN,
                1
            ),
            Err(WriterError::TooLarge { .. })
        ));
    }

    fn ipv4_datagram(payload_length: usize) -> Vec<u8> {
        let mut header = Ipv4Header {
            identification: 0x1234,
            dont_fragment: false,
            time_to_live: 64,
            protocol: etherparse::IpNumber::UDP,
            source: [198, 51, 100, 1],
            destination: [192, 0, 2, 2],
            ..Default::default()
        };
        header.set_payload_len(payload_length).unwrap();
        header.header_checksum = header.calc_header_checksum();
        let mut packet = header.to_bytes().to_vec();
        packet.extend((0..payload_length).map(|index| index as u8));
        packet
    }

    #[test]
    fn ipv4_fragments_reassemble_to_the_original_payload() {
        let packet = ipv4_datagram(4000);
        let fragments = collect_ipv4(&packet, MTU).unwrap();
        assert!(fragments.len() > 1);
        let mut reassembled = Vec::new();
        for (index, fragment) in fragments.iter().enumerate() {
            assert_eq!(validate(fragment, MTU), Ok(()));
            let (header, payload) = Ipv4Header::from_slice(fragment).unwrap();
            assert_eq!(header.identification, 0x1234);
            assert!(!header.dont_fragment);
            assert_eq!(header.more_fragments, index + 1 < fragments.len());
            assert_eq!(
                header.fragment_offset.value() as usize * FRAGMENT_ALIGNMENT,
                reassembled.len()
            );
            assert_eq!(header.header_checksum, header.calc_header_checksum());
            reassembled.extend_from_slice(payload);
        }
        assert_eq!(reassembled, packet[IPV4_HEADER_LEN..]);
    }

    #[test]
    fn every_ipv4_fragment_but_the_last_is_eight_byte_aligned() {
        let fragments = collect_ipv4(&ipv4_datagram(4000), MTU).unwrap();
        for fragment in &fragments[..fragments.len() - 1] {
            assert_eq!((fragment.len() - IPV4_HEADER_LEN) % FRAGMENT_ALIGNMENT, 0);
        }
    }

    #[test]
    fn ipv4_fragmenting_refuses_what_it_must_not_split() {
        let mut packet = ipv4_datagram(4000);
        packet[6] |= 0x40;
        assert!(matches!(
            collect_ipv4(&packet, MTU),
            Err(WriterError::Unfragmentable(_))
        ));
        let mut packet = ipv4_datagram(4000);
        packet[6] |= 0x20;
        assert!(matches!(
            collect_ipv4(&packet, MTU),
            Err(WriterError::Unfragmentable(_))
        ));
        let mut packet = ipv4_datagram(4000);
        packet[0] = 0x46;
        assert!(matches!(
            collect_ipv4(&packet, MTU),
            Err(WriterError::Unfragmentable(_))
        ));
        assert!(matches!(
            collect_ipv4(&ipv4_datagram(4000), IPV4_HEADER_LEN),
            Err(WriterError::TooLarge { .. })
        ));
        assert!(matches!(
            collect_ipv4(&datagram(64), MTU),
            Err(WriterError::Malformed(_))
        ));
    }

    fn table(tuples: usize, tracked: usize) -> (Ipv4Identifications, Instant) {
        let opened = Instant::now();
        let table = Ipv4Identifications::new(Prepared {
            tuples,
            tracked,
            opened,
        });
        (table, opened + NONREUSE_WINDOW)
    }

    fn guarding(tuple: Tuple, now: Instant) -> Guarding {
        Guarding {
            tuple: Some(tuple),
            oversized: true,
            now,
        }
    }

    fn opened_emitter(mtu: usize, tuples: usize) -> (Emitter, Instant) {
        let opened = Instant::now();
        let emitter = Emitter::new(
            mtu,
            Prepared {
                tuples,
                tracked: 64,
                opened,
            },
        );
        (emitter, opened + NONREUSE_WINDOW)
    }

    #[derive(Default)]
    struct Observed {
        packets: Vec<usize>,
        guarded: Vec<Option<Guarded>>,
        refuse: bool,
        accept: Option<usize>,
    }

    impl Sink for Observed {
        fn packet(&mut self, packet: Vec<u8>, guarded: Option<Guarded>) -> bool {
            let accepted =
                !self.refuse && self.accept.is_none_or(|accept| self.packets.len() < accept);
            self.packets.push(packet.len());
            self.guarded.push(guarded);
            accepted
        }
    }

    #[test]
    fn a_denied_identification_builds_nothing_and_writes_nothing() {
        for size in [800usize, 4_000] {
            let (mut identifications, now) = table(1, 64);
            let held = Ipv4Addr::new(192, 0, 2, 1);
            let newcomer = Ipv4Addr::new(192, 0, 2, 2);
            let remote = Ipv4Addr::new(198, 51, 100, 1);
            let guarded = match size_policy(&mut identifications, guarding((held, remote, 17), now))
            {
                Sizing::Fragmentable(guarded) => guarded,
                other => panic!("the held tuple should have one: {other:?}"),
            };
            identifications.register(guarded).expect("tracked");
            identifications.terminal(Terminal::wrote(guarded, now));

            let built = std::cell::Cell::new(0usize);
            let mut sink = Observed::default();
            let mut fragment_id = 0u32;
            let emitted = emit(
                &mut identifications,
                guarding((newcomer, remote, 17), now),
                1_500,
                &mut fragment_id,
                |_| {
                    built.set(built.get() + 1);
                    Ok(ipv4_datagram(size))
                },
                &mut sink,
            );
            assert_eq!(emitted, Emitted::Denied(Denial::AtCapacity), "size {size}");
            assert_eq!(built.get(), 0, "size {size}: the builder was never called");
            assert!(
                sink.packets.is_empty(),
                "size {size}: nothing reached the writer"
            );
            assert_eq!(
                fragment_id, 0,
                "size {size}: no fragment identity was spent"
            );
            assert_eq!(
                identifications.outstanding(),
                0,
                "size {size}: and nothing was taken onto the books"
            );
            assert_eq!(outcome(emitted), Outcome::Counted, "size {size}");
        }
    }

    #[test]
    fn the_opening_window_denies_before_anything_is_built() {
        let opened = Instant::now();
        let mut identifications = Ipv4Identifications::new(Prepared {
            tuples: 8,
            tracked: 64,
            opened,
        });
        let source = Ipv4Addr::new(192, 0, 2, 1);
        let remote = Ipv4Addr::new(198, 51, 100, 1);
        let built = std::cell::Cell::new(0usize);
        let mut sink = Observed::default();
        let mut fragment_id = 0u32;
        for elapsed in [
            Duration::ZERO,
            Duration::from_secs(59),
            NONREUSE_WINDOW - Duration::from_nanos(1),
        ] {
            let emitted = emit(
                &mut identifications,
                guarding((source, remote, 17), opened + elapsed),
                1_500,
                &mut fragment_id,
                |_| {
                    built.set(built.get() + 1);
                    Ok(ipv4_datagram(4_000))
                },
                &mut sink,
            );
            assert_eq!(
                emitted,
                Emitted::Denied(Denial::Quarantined),
                "{elapsed:?} into the session"
            );
            assert_eq!(outcome(emitted), Outcome::Counted);
        }
        assert_eq!(built.get(), 0);
        assert!(sink.packets.is_empty());
        assert_eq!(fragment_id, 0);
        assert!(identifications.is_empty());
        assert_eq!(identifications.quarantined(), 3);

        let emitted = emit(
            &mut identifications,
            guarding((source, remote, 17), opened + NONREUSE_WINDOW),
            1_500,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(1));
                Ok(ipv4_datagram(800))
            },
            &mut sink,
        );
        assert_eq!(
            emitted,
            Emitted::Written {
                written: 1,
                blocked: 0
            }
        );
    }

    #[test]
    fn the_opening_window_leaves_everything_it_does_not_guard_alone() {
        let opened = Instant::now();
        let mut emitter = Emitter::new(
            1_500,
            Prepared {
                tuples: 8,
                tracked: 64,
                opened,
            },
        );
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();

        emitter.emit(
            opened,
            Addressed {
                source: Ipv4Addr::new(192, 0, 2, 1).into(),
                destination: remote,
                protocol: 17,
                size: 800,
            },
            |identification| {
                assert_eq!(identification, None);
                Ok(ipv4_datagram(800))
            },
            &mut sink,
            &mut reports,
        );
        emitter.emit(
            opened,
            Addressed {
                source: SOURCE.into(),
                destination: DESTINATION.into(),
                protocol: 17,
                size: 4_000,
            },
            |identification| {
                assert_eq!(identification, None);
                Ok(datagram(4_000))
            },
            &mut sink,
            &mut reports,
        );
        emitter.wrote(true);

        assert_eq!(emitter.identification_denied(), 0);
        assert_eq!(emitter.identifications().quarantined(), 0);
        assert!(reports.raised.is_empty());
        assert!(sink.packets.len() > 2);
        assert!(emitter.written() >= 3);
    }

    #[test]
    fn repeated_denials_coalesce_into_the_count_and_leave_old_tuples_alone() {
        let (mut identifications, now) = table(2, 64);
        let remote = Ipv4Addr::new(198, 51, 100, 1);
        let held: Vec<_> = (0..2u8)
            .map(|octet| Ipv4Addr::new(192, 0, 2, octet))
            .collect();
        for source in &held {
            let Sizing::Fragmentable(guarded) =
                size_policy(&mut identifications, guarding((*source, remote, 17), now))
            else {
                panic!("the held tuples should each have one");
            };
            identifications.register(guarded).expect("tracked");
            identifications.terminal(Terminal::wrote(guarded, now));
        }
        let mut fragment_id = 0u32;

        let mut denied = 0usize;
        let built = std::cell::Cell::new(0usize);
        let mut reports = 0usize;
        let mut sink = Observed::default();
        for attempt in 0..5_000u32 {
            let newcomer = Ipv4Addr::from((attempt + 1_000).to_be_bytes());
            let emitted = emit(
                &mut identifications,
                guarding((newcomer, remote, 17), now),
                1_500,
                &mut fragment_id,
                |_| {
                    built.set(built.get() + 1);
                    Ok(ipv4_datagram(800))
                },
                &mut sink,
            );
            match outcome(emitted) {
                Outcome::Counted => denied += 1,
                Outcome::Reported(_) => reports += 1,
                Outcome::Wrote { .. } => panic!("attempt {attempt} should have been refused"),
            }
        }
        assert_eq!(denied, 5_000);
        assert_eq!(reports, 0);
        assert_eq!(built.get(), 0);
        assert!(sink.packets.is_empty());
        assert_eq!(identifications.len(), 2);
        assert_eq!(
            identifications.reclaimed(),
            0,
            "and nothing was taken from anyone"
        );
        assert_eq!(identifications.refused(), 5_000);
        assert_eq!(identifications.sweeps(), 1);

        for expected in 2..=6u16 {
            for source in &held {
                let Sizing::Fragmentable(guarded) =
                    size_policy(&mut identifications, guarding((*source, remote, 17), now))
                else {
                    panic!("the held tuples keep their sequences");
                };
                assert_eq!(guarded.identification(), expected);
            }
        }
    }

    #[test]
    fn an_identified_oversized_datagram_is_built_and_fragmented() {
        let (mut identifications, now) = table(4, 64);
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut sink = Observed::default();
        let mut fragment_id = 0u32;
        let emitted = emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(1));
                Ok(ipv4_datagram(4_000))
            },
            &mut sink,
        );
        let written = sink.packets.len();
        assert!(written > 1, "{written} fragments");
        assert_eq!(
            emitted,
            Emitted::Written {
                written,
                blocked: 0
            }
        );
        assert!(sink.packets.iter().all(|length| *length <= 1_280));
        let first = sink.guarded[0].expect("guarded");
        assert_eq!(first.identification(), 1);
        assert_eq!(first.tuple(), tuple);
        assert!(sink.guarded.iter().all(|guarded| *guarded == Some(first)));
        assert_eq!(
            identifications.outstanding(),
            written,
            "each accepted fragment is one packet the writer owes an ending for"
        );

        let mut full = Observed {
            refuse: true,
            ..Observed::default()
        };
        let before = identifications.outstanding();
        let emitted = emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |_| Ok(ipv4_datagram(4_000)),
            &mut full,
        );
        let blocked = full.packets.len();
        assert_eq!(
            emitted,
            Emitted::Written {
                written: 0,
                blocked
            }
        );
        assert_eq!(
            identifications.outstanding(),
            before,
            "a refused fragment was never the writer's"
        );
    }

    #[test]
    fn a_partly_admitted_datagram_owes_an_ending_for_exactly_what_was_accepted() {
        let (mut identifications, now) = table(4, 64);
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut sink = Observed {
            accept: Some(2),
            ..Observed::default()
        };
        let mut fragment_id = 0u32;
        let emitted = emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |_| Ok(ipv4_datagram(4_000)),
            &mut sink,
        );
        let total = sink.packets.len();
        assert!(total > 2, "{total} fragments, of which two were taken");
        assert_eq!(
            emitted,
            Emitted::Written {
                written: 2,
                blocked: total - 2
            }
        );
        assert_eq!(identifications.outstanding(), 2);

        let guarded = sink.guarded[0].expect("guarded");
        let later = now + Duration::from_secs(10);
        identifications.terminal(Terminal::wrote(guarded, later));
        identifications.terminal(Terminal::wrote(guarded, now));
        assert_eq!(identifications.outstanding(), 0);
        assert_eq!(identifications.stale(), 0);
        assert_eq!(identifications.settled(), (2, 0));
    }

    #[test]
    fn only_a_datagram_that_reached_nothing_gives_its_identification_back() {
        let (mut identifications, now) = table(4, 64);
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut fragment_id = 0u32;

        let mut refused = Observed {
            refuse: true,
            ..Observed::default()
        };
        let emitted = emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(1));
                Ok(ipv4_datagram(4_000))
            },
            &mut refused,
        );
        assert!(matches!(emitted, Emitted::Written { written: 0, .. }));
        assert_eq!(identifications.unissued_count(), 1);
        assert_eq!(identifications.outstanding(), 0);

        let emitted = emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(1));
                Err(WriterError::Malformed("nothing was built"))
            },
            &mut refused,
        );
        assert!(matches!(emitted, Emitted::Unbuildable(_)));
        assert_eq!(identifications.unissued_count(), 2);

        let mut partial = Observed {
            accept: Some(1),
            ..Observed::default()
        };
        emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(1));
                Ok(ipv4_datagram(4_000))
            },
            &mut partial,
        );
        assert_eq!(identifications.unissued_count(), 2);
        emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(2));
                Ok(ipv4_datagram(800))
            },
            &mut partial,
        );
    }

    #[test]
    fn a_df_too_large_packet_is_unbuildable_rather_than_denied() {
        let (mut identifications, now) = table(4, 64);
        let mut sink = Observed::default();
        let mut fragment_id = 0u32;
        let emitted = emit(
            &mut identifications,
            guarding(
                (
                    Ipv4Addr::new(192, 0, 2, 1),
                    Ipv4Addr::new(198, 51, 100, 1),
                    17,
                ),
                now,
            ),
            1_280,
            &mut fragment_id,
            |_| {
                let mut packet = ipv4_datagram(4_000);
                packet[6] = 0x40;
                Ok(packet)
            },
            &mut sink,
        );
        assert!(sink.packets.is_empty());
        assert_eq!(
            outcome(emitted),
            Outcome::Reported(WriterError::Unfragmentable("DF is set"))
        );
    }

    #[derive(Default)]
    struct Reports {
        raised: Vec<String>,
    }

    impl Reporter for Reports {
        fn unbuildable(&mut self, _source: IpAddr, _destination: IpAddr, error: &WriterError) {
            self.raised.push(format!("{error:?}"));
        }
    }

    #[test]
    fn the_owner_denies_quietly_at_both_sizes_that_need_an_identification() {
        for size in [1_600usize, 4_000] {
            let (mut emitter, now) = opened_emitter(1_500, 1);
            assert_eq!(emitter.mtu(), 1_500);
            let held: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
            let newcomer: IpAddr = Ipv4Addr::new(192, 0, 2, 2).into();
            let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();

            let mut sink = Observed::default();
            let mut reports = Reports::default();
            emitter.emit(
                now,
                Addressed {
                    source: held,
                    destination: remote,
                    protocol: 17,
                    size,
                },
                |identification| {
                    assert_eq!(identification, Some(1));
                    Ok(ipv4_datagram(size))
                },
                &mut sink,
                &mut reports,
            );
            assert_eq!(emitter.identification_denied(), 0);
            let wrote = emitter.written();
            assert!(wrote > 0, "size {size}: the held tuple's output went");
            for guarded in sink.guarded.iter().flatten() {
                emitter.terminal(Terminal::wrote(*guarded, now));
            }

            let built = std::cell::Cell::new(0usize);
            let mut sink = Observed::default();
            let mut reports = Reports::default();
            emitter.emit(
                now,
                Addressed {
                    source: newcomer,
                    destination: remote,
                    protocol: 17,
                    size,
                },
                |_| {
                    built.set(built.get() + 1);
                    Ok(ipv4_datagram(size))
                },
                &mut sink,
                &mut reports,
            );

            assert_eq!(
                emitter.identification_denied(),
                1,
                "size {size}: exactly one, on the owner's own counter"
            );
            assert_eq!(built.get(), 0, "size {size}: the builder was never called");
            assert!(sink.packets.is_empty(), "size {size}: nothing was queued");
            assert!(
                reports.raised.is_empty(),
                "size {size}: and nothing reported"
            );
            assert_eq!(emitter.written(), wrote, "size {size}: nothing was written");
            assert_eq!(emitter.unwritable(), 0, "size {size}: not a failure either");
        }
    }

    #[test]
    fn the_owner_does_not_ask_for_an_identification_within_its_mtu() {
        let (mut emitter, now) = opened_emitter(1_500, 1);
        let held: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let newcomer: IpAddr = Ipv4Addr::new(192, 0, 2, 2).into();
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        emitter.emit(
            now,
            Addressed {
                source: held,
                destination: remote,
                protocol: 17,
                size: 1_600,
            },
            |_| Ok(ipv4_datagram(1_600)),
            &mut sink,
            &mut reports,
        );

        let built = std::cell::Cell::new(0usize);
        emitter.emit(
            now,
            Addressed {
                source: newcomer,
                destination: remote,
                protocol: 17,
                size: 800,
            },
            |identification| {
                built.set(built.get() + 1);
                assert_eq!(identification, None);
                Ok(ipv4_datagram(800))
            },
            &mut sink,
            &mut reports,
        );
        assert_eq!(built.get(), 1);
        assert_eq!(emitter.identification_denied(), 0);
        assert_eq!(
            emitter.identifications().refused(),
            0,
            "the table was never asked"
        );
        assert!(reports.raised.is_empty());
    }

    #[test]
    fn the_owner_reports_a_df_too_large_packet_and_counts_it_separately() {
        let (mut emitter, now) = opened_emitter(1_280, 8);
        let source: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        emitter.emit(
            now,
            Addressed {
                source,
                destination: remote,
                protocol: 17,
                size: 4_000,
            },
            |_| {
                let mut packet = ipv4_datagram(4_000);
                packet[6] = 0x40;
                Ok(packet)
            },
            &mut sink,
            &mut reports,
        );

        assert_eq!(emitter.unwritable(), 1);
        assert_eq!(emitter.identification_denied(), 0);
        assert_eq!(reports.raised.len(), 1);
        assert!(
            reports.raised[0].contains("DF is set"),
            "{:?}",
            reports.raised
        );
        assert!(sink.packets.is_empty());
    }

    #[test]
    fn the_owner_stays_quiet_under_repeated_denial() {
        let (mut emitter, now) = opened_emitter(1_500, 2);
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();
        let held: Vec<IpAddr> = (0..2u8)
            .map(|octet| IpAddr::from(Ipv4Addr::new(192, 0, 2, octet)))
            .collect();
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        for source in &held {
            emitter.emit(
                now,
                Addressed {
                    source: *source,
                    destination: remote,
                    protocol: 17,
                    size: 1_600,
                },
                |identification| {
                    assert_eq!(identification, Some(1));
                    Ok(ipv4_datagram(1_600))
                },
                &mut sink,
                &mut reports,
            );
        }
        for guarded in sink.guarded.iter().flatten() {
            emitter.terminal(Terminal::wrote(*guarded, now));
        }
        let written = emitter.written();

        let built = std::cell::Cell::new(0usize);
        for attempt in 0..5_000u32 {
            let newcomer = IpAddr::from(Ipv4Addr::from((attempt + 1_000).to_be_bytes()));
            emitter.emit(
                now,
                Addressed {
                    source: newcomer,
                    destination: remote,
                    protocol: 17,
                    size: 1_600,
                },
                |_| {
                    built.set(built.get() + 1);
                    Ok(ipv4_datagram(1_600))
                },
                &mut sink,
                &mut reports,
            );
        }
        assert_eq!(emitter.identification_denied(), 5_000);
        assert_eq!(built.get(), 0);
        assert!(reports.raised.is_empty());
        assert_eq!(emitter.written(), written);
        assert_eq!(emitter.identifications().sweeps(), 1);

        for expected in 2..=6u16 {
            for source in &held {
                emitter.emit(
                    now,
                    Addressed {
                        source: *source,
                        destination: remote,
                        protocol: 17,
                        size: 1_600,
                    },
                    |identification| {
                        assert_eq!(identification, Some(expected));
                        Ok(ipv4_datagram(1_600))
                    },
                    &mut sink,
                    &mut reports,
                );
            }
        }
        assert_eq!(
            emitter.identification_denied(),
            5_000,
            "still exactly those"
        );
    }

    #[test]
    fn the_owner_denies_a_spent_sequence_until_its_window_has_passed() {
        let (mut emitter, now) = opened_emitter(1_500, 4);
        let source: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();
        let addressed = Addressed {
            source,
            destination: remote,
            protocol: 17,
            size: 1_600,
        };
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        for _ in 0..65_536u32 {
            emitter.emit(
                now,
                addressed,
                |_| Ok(ipv4_datagram(1_600)),
                &mut sink,
                &mut reports,
            );
            assert!(!sink.guarded.is_empty());
            for guarded in sink.guarded.drain(..).flatten() {
                emitter.terminal(Terminal::wrote(guarded, now));
            }
            sink.packets.clear();
        }
        assert_eq!(emitter.identification_denied(), 0);

        let built = std::cell::Cell::new(0usize);
        emitter.emit(
            now + NONREUSE_WINDOW - Duration::from_nanos(1),
            addressed,
            |_| {
                built.set(built.get() + 1);
                Ok(ipv4_datagram(1_600))
            },
            &mut sink,
            &mut reports,
        );
        assert_eq!(emitter.identification_denied(), 1);
        assert_eq!(emitter.identifications().exhausted(), 1);
        assert_eq!(built.get(), 0);
        assert!(reports.raised.is_empty());

        emitter.emit(
            now + NONREUSE_WINDOW,
            addressed,
            |identification| {
                assert_eq!(identification, Some(1));
                Ok(ipv4_datagram(1_600))
            },
            &mut sink,
            &mut reports,
        );
        assert_eq!(emitter.identification_denied(), 1);
    }

    #[test]
    fn a_full_table_denies_the_datagram_rather_than_clearing_its_identification() {
        let (mut identifications, now) = table(1, 64);
        let held = Ipv4Addr::new(192, 0, 2, 1);
        let newcomer = Ipv4Addr::new(192, 0, 2, 2);
        let remote = Ipv4Addr::new(198, 51, 100, 1);

        let Sizing::Fragmentable(guarded) =
            size_policy(&mut identifications, guarding((held, remote, 17), now))
        else {
            panic!("the held tuple should have one");
        };
        assert_eq!(guarded.identification(), 1);
        identifications.register(guarded).expect("tracked");
        identifications.terminal(Terminal::wrote(guarded, now));

        assert_eq!(
            size_policy(&mut identifications, guarding((newcomer, remote, 17), now)),
            Sizing::Denied(Denial::AtCapacity)
        );
        let refused = identifications.refused();
        assert_eq!(
            size_policy(
                &mut identifications,
                Guarding {
                    oversized: false,
                    ..guarding((newcomer, remote, 17), now)
                }
            ),
            Sizing::Atomic
        );
        assert_eq!(
            identifications.refused(),
            refused,
            "a datagram that needs no Identification does not ask for one"
        );
        assert_eq!(
            size_policy(
                &mut identifications,
                Guarding {
                    tuple: None,
                    oversized: true,
                    now
                }
            ),
            Sizing::Atomic
        );

        for _ in 0..10_000 {
            assert_eq!(
                size_policy(&mut identifications, guarding((newcomer, remote, 17), now)),
                Sizing::Denied(Denial::AtCapacity)
            );
        }
        assert_eq!(identifications.len(), 1);
        assert_eq!(identifications.refused(), refused + 10_000);

        for expected in 2..=6u16 {
            let Sizing::Fragmentable(guarded) =
                size_policy(&mut identifications, guarding((held, remote, 17), now))
            else {
                panic!("the held tuple keeps its sequence");
            };
            assert_eq!(guarded.identification(), expected);
        }
    }

    #[test]
    fn a_denied_identification_is_not_a_packetization_failure() {
        let mut packet = ipv4_datagram(4000);
        packet[6] = 0x40;
        assert!(matches!(
            fragment_ipv4(&packet, 1280, |_| panic!("nothing may be emitted")),
            Err(WriterError::Unfragmentable("DF is set"))
        ));
        assert!(matches!(
            fragment_ipv4(&ipv4_datagram(4000), IPV4_HEADER_LEN, |_| panic!(
                "nothing may be emitted"
            )),
            Err(WriterError::TooLarge { .. })
        ));
        let (mut identifications, now) = table(0, 64);
        assert_eq!(
            size_policy(
                &mut identifications,
                guarding(
                    (
                        Ipv4Addr::new(192, 0, 2, 1),
                        Ipv4Addr::new(198, 51, 100, 1),
                        17
                    ),
                    now
                )
            ),
            Sizing::Denied(Denial::AtCapacity)
        );
    }

    #[test]
    fn fragmentation_owns_one_fragment_at_a_time() {
        for ipv6 in [false, true] {
            let packet = if ipv6 {
                datagram(u16::MAX as usize - IPV6_HEADER_LEN - IPV6_FRAGMENT_HEADER_LEN)
            } else {
                ipv4_datagram(u16::MAX as usize - IPV4_HEADER_LEN)
            };
            let mut live = 0usize;
            let mut peak = 0usize;
            let mut count = 0usize;
            let mut widest = 0usize;
            let mut observe = |fragment: Vec<u8>| {
                live += 1;
                peak = peak.max(live);
                count += 1;
                widest = widest.max(fragment.len());
                drop(fragment);
                live -= 1;
            };
            let emitted = if ipv6 {
                fragment_ipv6(&packet, 1280, 1, &mut observe).expect("fragmented")
            } else {
                fragment_ipv4(&packet, 1280, &mut observe).expect("fragmented")
            };
            assert!(count > 40, "ipv6 {ipv6}: {count} fragments");
            assert_eq!(emitted, count);
            assert_eq!(
                peak, 1,
                "ipv6 {ipv6}: only one fragment may exist at a time"
            );
            assert!(widest <= 1280, "ipv6 {ipv6}: {widest} exceeds the MTU");
        }
    }

    #[test]
    fn a_refused_fragmentation_emits_nothing() {
        let mut emitted = 0usize;
        assert!(matches!(
            fragment_ipv6(
                &datagram(4000),
                IPV6_HEADER_LEN + IPV6_FRAGMENT_HEADER_LEN,
                1,
                |_| emitted += 1
            ),
            Err(WriterError::TooLarge { .. })
        ));
        assert_eq!(emitted, 0);
        let mut packet = ipv4_datagram(4000);
        packet[6] = 0x20;
        assert!(matches!(
            fragment_ipv4(&packet, 1280, |_| emitted += 1),
            Err(WriterError::Unfragmentable(_))
        ));
        assert_eq!(emitted, 0);
    }
}
