use std::net::IpAddr;
#[cfg(test)]
use std::net::Ipv6Addr;
use std::time::Instant;

use crate::shared::ip_wire::{Error as IpError, Packet};
use crate::shared::ipv4_identification::{Denial, Guarded, Ipv4Identifications, Terminal, Tuple};
use crate::shared::tun_handoff::Batch;

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

/// What the size policy decided about one datagram before it was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sizing {
    /// Output that needs no daemon-issued Identification. RFC 6864 section 4.1 exempts atomic datagrams from
    /// the reuse constraint.
    Atomic,
    /// Oversized IPv4, so a receiver will reassemble it and the value it reassembles on has to be unique.
    Fragmentable(Guarded),
    /// Oversized IPv4 that may not be given a value yet.
    Denied(Denial),
}

#[derive(Debug, Clone, Copy)]
pub struct SizingInput {
    pub tuple: Option<Tuple>,
    pub oversized: bool,
    pub now: Instant,
}

pub fn size_policy(identifications: &mut Ipv4Identifications, input: SizingInput) -> Sizing {
    let SizingInput {
        tuple,
        oversized,
        now,
    } = input;
    // Only IPv4 output this daemon will fragment needs an Identification.
    let Some(tuple) = tuple.filter(|_| oversized) else {
        return Sizing::Atomic;
    };
    match identifications.next(tuple, now) {
        Ok(guarded) => Sizing::Fragmentable(guarded),
        Err(denial) => Sizing::Denied(denial),
    }
}

pub trait Sink {
    /// Takes every packet of one logical datagram together, or refuses all of them.
    fn datagram(&mut self, batch: Batch) -> bool;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Emitted {
    /// Whole-batch handoff result. Exactly one packet count is zero; neither records TUN writes.
    Handed {
        queued: usize,
        refused: usize,
    },
    /// Oversized IPv4 dropped because no Identification could be issued safely.
    Denied(Denial),
    Unbuildable(WriterError),
}

pub fn emit<S: Sink>(
    identifications: &mut Ipv4Identifications,
    sizing: SizingInput,
    mtu: usize,
    fragment_identification: &mut u32,
    build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
    sink: &mut S,
) -> Emitted {
    let guarded = match size_policy(identifications, sizing) {
        Sizing::Atomic => None,
        Sizing::Fragmentable(guarded) => Some(guarded),
        Sizing::Denied(denial) => return Emitted::Denied(denial),
    };
    // Every non-handoff path returns its reserved sequence position.
    let packet = match build(guarded.map(|guarded| guarded.identification())) {
        Ok(packet) => packet,
        Err(e) => return unissued(identifications, guarded, Emitted::Unbuildable(e)),
    };
    // Packetize first so the sink admits the complete datagram or none of it.
    let mut packets = Vec::new();
    if packet.len() <= mtu {
        packets.push(packet);
    } else {
        let fragmented = if packet.first().map(|byte| byte >> 4) == Some(6) {
            *fragment_identification = fragment_identification.wrapping_add(1);
            fragment_ipv6(&packet, mtu, *fragment_identification, |packet| {
                packets.push(packet)
            })
        } else {
            fragment_ipv4(&packet, mtu, |packet| packets.push(packet))
        };
        if let Err(e) = fragmented {
            return unissued(identifications, guarded, Emitted::Unbuildable(e));
        }
    }
    let count = packets.len();
    if sink.datagram(Batch::new(packets, guarded)) {
        // Register only after the synchronous handoff accepts the batch.
        if let Some(guarded) = guarded {
            identifications.accepted(guarded);
        }
        Emitted::Handed {
            queued: count,
            refused: 0,
        }
    } else {
        unissued(
            identifications,
            guarded,
            Emitted::Handed {
                queued: 0,
                refused: count,
            },
        )
    }
}

/// Gives a guarded datagram's sequence position back, for the ways out where no packet of it was accepted.
fn unissued(
    identifications: &mut Ipv4Identifications,
    guarded: Option<Guarded>,
    emitted: Emitted,
) -> Emitted {
    if let Some(guarded) = guarded {
        identifications.unissued(guarded);
    }
    emitted
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
    /// Packets admitted by the interface handoff; the serial writer separately counts TUN writes.
    queued: u64,
    /// Packets of a datagram the interface queue would not take, dropped whole with it.
    handoff_refused: u64,
    /// Datagrams the daemon could not build or fragment.
    unbuildable: u64,
    denied: u64,
}

impl Emitter {
    /// `opened` starts the Identification allocator's opening quarantine.
    pub fn new(mtu: usize, opened: Instant) -> Self {
        Self {
            mtu,
            identifications: Ipv4Identifications::new(opened),
            fragment_identification: 0,
            queued: 0,
            handoff_refused: 0,
            unbuildable: 0,
            denied: 0,
        }
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    /// Packets admitted by the handoff, not written to the TUN.
    pub fn queued(&self) -> u64 {
        self.queued
    }

    /// Packets dropped because the interface queue was full or closed.
    pub fn handoff_refused(&self) -> u64 {
        self.handoff_refused
    }

    pub fn unbuildable(&self) -> u64 {
        self.unbuildable
    }

    /// Oversized IPv4 datagrams dropped because their tuple had no Identification to give.
    pub fn denied(&self) -> u64 {
        self.denied
    }

    pub fn identifications(&self) -> &Ipv4Identifications {
        &self.identifications
    }

    /// Applies one guarded datagram's ending, which is what lets its tuple's sequence start again.
    pub fn terminal(&mut self, terminal: Terminal) {
        self.identifications.terminal(terminal);
    }

    /// Counts one already-formed packet's handoff, and answers what it was.
    pub fn handed(&mut self, accepted: bool) -> bool {
        if accepted {
            self.queued += 1;
        } else {
            self.handoff_refused += 1;
        }
        accepted
    }

    /// Whether the complete datagram reached the interface queue.
    pub fn emit<S: Sink, R: Reporter>(
        &mut self,
        now: Instant,
        addressed: Addressed,
        build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
        sink: &mut S,
        reporter: &mut R,
    ) -> bool {
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
        match outcome(emit(
            &mut self.identifications,
            SizingInput {
                tuple,
                oversized: size > self.mtu,
                now,
            },
            self.mtu,
            &mut self.fragment_identification,
            build,
            sink,
        )) {
            Outcome::Handed { queued, refused } => {
                self.queued += queued as u64;
                self.handoff_refused += refused as u64;
                queued > 0
            }
            // Denial is traffic-driven, so count it rather than emit one report per datagram.
            Outcome::Counted => {
                self.denied += 1;
                false
            }
            Outcome::Reported(e) => {
                self.unbuildable += 1;
                reporter.unbuildable(source, destination, &e);
                false
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Handed { queued: usize, refused: usize },
    Counted,
    Reported(WriterError),
}

pub fn outcome(emitted: Emitted) -> Outcome {
    match emitted {
        Emitted::Handed { queued, refused } => Outcome::Handed { queued, refused },
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

    use std::cell::Cell;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use crate::shared::ipv4_identification::MDL;

    const MTU: usize = 1_500;
    const SOURCE: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1);
    const DESTINATION: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2);
    const TUPLE: Tuple = (
        Ipv4Addr::new(198, 51, 100, 1),
        Ipv4Addr::new(192, 0, 2, 2),
        17,
    );

    fn issuing() -> (Ipv4Identifications, Instant) {
        let opened = Instant::now();
        (Ipv4Identifications::new(opened), opened + MDL)
    }

    fn datagram(payload_length: usize) -> Vec<u8> {
        let mut packet = ipv6_header(SOURCE, DESTINATION, 17, 64, payload_length);
        packet.extend((0..payload_length).map(|index| index as u8));
        packet
    }

    fn ipv4_datagram(payload_length: usize, identification: u16) -> Vec<u8> {
        let mut header = Ipv4Header {
            identification,
            dont_fragment: false,
            time_to_live: 64,
            protocol: etherparse::IpNumber::UDP,
            source: TUPLE.0.octets(),
            destination: TUPLE.1.octets(),
            ..Default::default()
        };
        header.set_payload_len(payload_length).unwrap();
        header.header_checksum = header.calc_header_checksum();
        let mut packet = header.to_bytes().to_vec();
        packet.extend((0..payload_length).map(|index| index as u8));
        packet
    }

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

    #[derive(Default)]
    struct Observed {
        batches: Vec<Batch>,
        refuse: bool,
    }

    impl Sink for Observed {
        fn datagram(&mut self, batch: Batch) -> bool {
            self.batches.push(batch);
            !self.refuse
        }
    }

    #[derive(Default)]
    struct Reports(Vec<String>);

    impl Reporter for Reports {
        fn unbuildable(&mut self, _source: IpAddr, _destination: IpAddr, error: &WriterError) {
            self.0.push(format!("{error:?}"));
        }
    }

    fn oversized(
        identifications: &mut Ipv4Identifications,
        now: Instant,
        sink: &mut Observed,
    ) -> (Emitted, Option<u16>) {
        let used = Cell::new(None);
        let mut fragment_identification = 0;
        let emitted = emit(
            identifications,
            SizingInput {
                tuple: Some(TUPLE),
                oversized: true,
                now,
            },
            1_280,
            &mut fragment_identification,
            |value| {
                used.set(value);
                Ok(ipv4_datagram(
                    4_000,
                    value.expect("oversized IPv4 is guarded"),
                ))
            },
            sink,
        );
        (emitted, used.get())
    }

    #[test]
    fn validate_accepts_only_consistent_packets_within_the_mtu() {
        assert_eq!(validate(&datagram(64), MTU), Ok(()));
        let mut short = datagram(64);
        short.pop();
        assert_eq!(
            validate(&short, MTU),
            Err(WriterError::Malformed("IPv6 payload length disagrees"))
        );
        let oversized = datagram(MTU);
        assert_eq!(
            validate(&oversized, MTU),
            Err(WriterError::TooLarge {
                size: oversized.len(),
                mtu: MTU,
            })
        );
        assert!(matches!(
            validate(&[0x50; 40], MTU),
            Err(WriterError::Malformed(_))
        ));
    }

    #[test]
    fn ipv6_fragments_reassemble_to_the_original_payload() {
        let packet = datagram(4_000);
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
        for fragment in &fragments[..fragments.len() - 1] {
            assert_eq!(
                (fragment.len() - IPV6_HEADER_LEN - IPV6_FRAGMENT_HEADER_LEN) % FRAGMENT_ALIGNMENT,
                0
            );
        }
    }

    #[test]
    fn ipv6_fragmenting_rejects_unfragmentable_packets() {
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
        assert!(matches!(
            collect_ipv6(
                &datagram(4_000),
                IPV6_HEADER_LEN + IPV6_FRAGMENT_HEADER_LEN,
                1
            ),
            Err(WriterError::TooLarge { .. })
        ));
    }

    #[test]
    fn ipv4_fragments_reassemble_to_the_original_payload() {
        let packet = ipv4_datagram(4_000, 0x1234);
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
        for fragment in &fragments[..fragments.len() - 1] {
            assert_eq!((fragment.len() - IPV4_HEADER_LEN) % FRAGMENT_ALIGNMENT, 0);
        }
    }

    #[test]
    fn ipv4_fragmenting_refuses_packets_it_must_not_split() {
        let mut packet = ipv4_datagram(4_000, 1);
        packet[6] |= 0x40;
        assert_eq!(
            collect_ipv4(&packet, MTU),
            Err(WriterError::Unfragmentable("DF is set"))
        );
        let mut packet = ipv4_datagram(4_000, 1);
        packet[6] |= 0x20;
        assert_eq!(
            collect_ipv4(&packet, MTU),
            Err(WriterError::Unfragmentable("already fragmented"))
        );
        let mut packet = ipv4_datagram(4_000, 1);
        packet[0] = 0x46;
        assert!(matches!(
            collect_ipv4(&packet, MTU),
            Err(WriterError::Unfragmentable(_))
        ));
        assert!(matches!(
            collect_ipv4(&ipv4_datagram(4_000, 1), IPV4_HEADER_LEN),
            Err(WriterError::TooLarge { .. })
        ));
    }

    #[test]
    fn only_oversized_ipv4_spends_an_identification() {
        let (mut identifications, now) = issuing();
        assert_eq!(
            size_policy(
                &mut identifications,
                SizingInput {
                    tuple: Some(TUPLE),
                    oversized: false,
                    now,
                }
            ),
            Sizing::Atomic
        );
        assert_eq!(
            size_policy(
                &mut identifications,
                SizingInput {
                    tuple: None,
                    oversized: true,
                    now,
                }
            ),
            Sizing::Atomic,
            "IPv6 carries a fragment identification of its own and never one of these"
        );
        assert!(
            identifications.is_empty(),
            "and neither creates a tuple row at all"
        );
        let Sizing::Fragmentable(first) = size_policy(
            &mut identifications,
            SizingInput {
                tuple: Some(TUPLE),
                oversized: true,
                now,
            },
        ) else {
            panic!("oversized IPv4 must be fragmentable");
        };
        assert_eq!(first.identification(), 1);
        assert_eq!(identifications.len(), 1);
    }

    #[test]
    fn an_atomic_ipv4_datagram_is_emitted_without_asking_for_a_value() {
        let (mut identifications, now) = issuing();
        let mut fragment_identification = 0;
        let mut sink = Observed::default();
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: Some(TUPLE),
                oversized: false,
                now,
            },
            1_280,
            &mut fragment_identification,
            |identification| {
                assert_eq!(identification, None, "an atomic datagram is not guarded");
                Ok(ipv4_datagram(64, 0))
            },
            &mut sink,
        );
        assert_eq!(
            emitted,
            Emitted::Handed {
                queued: 1,
                refused: 0,
            }
        );
        assert_eq!(sink.batches[0].guarded(), None);
        assert!(identifications.is_empty());
    }

    #[test]
    fn packetization_hands_all_ipv4_fragments_over_once() {
        let (mut identifications, now) = issuing();
        let mut sink = Observed::default();
        let (emitted, identification) = oversized(&mut identifications, now, &mut sink);

        assert_eq!(sink.batches.len(), 1);
        let packets = sink.batches[0].packets();
        assert!(packets.len() > 1);
        assert_eq!(
            emitted,
            Emitted::Handed {
                queued: packets.len(),
                refused: 0,
            }
        );
        assert!(packets.iter().all(|packet| packet.len() <= 1_280));
        assert!(packets.iter().all(|packet| {
            Ipv4Header::from_slice(packet).unwrap().0.identification == identification.unwrap()
        }));
        assert_eq!(
            sink.batches[0].guarded().map(|guarded| guarded.tuple()),
            Some(TUPLE),
            "the batch carries the identity the writer settles against"
        );
        assert_eq!(identifications.outstanding(), 1);
    }

    #[test]
    fn sink_refusal_refuses_the_whole_batch_and_gives_its_value_back() {
        let (mut identifications, now) = issuing();
        let mut sink = Observed {
            refuse: true,
            ..Observed::default()
        };
        let (emitted, first) = oversized(&mut identifications, now, &mut sink);
        assert_eq!(sink.batches.len(), 1);
        assert_eq!(
            emitted,
            Emitted::Handed {
                queued: 0,
                refused: sink.batches[0].packets().len(),
            }
        );
        assert_eq!(
            identifications.outstanding(),
            0,
            "a refused datagram is not one the writer owes an ending for"
        );

        sink.refuse = false;
        let (_, second) = oversized(&mut identifications, now, &mut sink);
        assert_eq!(
            second, first,
            "nothing carrying it reached the wire, so the value is issued again"
        );
    }

    #[test]
    fn unbuildable_packet_hands_nothing_to_the_sink_and_gives_its_value_back() {
        let (mut identifications, now) = issuing();
        let mut fragment_identification = 0;
        let mut sink = Observed::default();
        let used = Cell::new(None);
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: Some(TUPLE),
                oversized: true,
                now,
            },
            1_280,
            &mut fragment_identification,
            |identification| {
                used.set(identification);
                let mut packet = ipv4_datagram(4_000, identification.unwrap());
                packet[6] |= 0x40;
                Ok(packet)
            },
            &mut sink,
        );
        assert_eq!(
            emitted,
            Emitted::Unbuildable(WriterError::Unfragmentable("DF is set"))
        );
        assert!(sink.batches.is_empty());
        let (_, next) = oversized(&mut identifications, now, &mut sink);
        assert_eq!(
            next,
            used.get(),
            "a datagram that was never built cannot be on the wire under that value"
        );
    }

    #[test]
    fn a_quarantined_session_drops_oversized_ipv4_rather_than_guessing_a_value() {
        let opened = Instant::now();
        let mut identifications = Ipv4Identifications::new(opened);
        let mut sink = Observed::default();
        let mut fragment_identification = 0;
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: Some(TUPLE),
                oversized: true,
                now: opened,
            },
            1_280,
            &mut fragment_identification,
            |_| panic!("a denied datagram is never built"),
            &mut sink,
        );
        assert_eq!(emitted, Emitted::Denied(Denial::Quarantined));
        assert_eq!(outcome(emitted), Outcome::Counted);
        assert!(sink.batches.is_empty());

        // Atomic output of the same tuple is unaffected, because it carries no value to collide.
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: Some(TUPLE),
                oversized: false,
                now: opened,
            },
            1_280,
            &mut fragment_identification,
            |_| Ok(ipv4_datagram(64, 0)),
            &mut sink,
        );
        assert_eq!(
            emitted,
            Emitted::Handed {
                queued: 1,
                refused: 0,
            }
        );
    }

    #[test]
    fn an_exhausted_tuple_denies_only_oversized_output_until_its_window_passes() {
        let (mut identifications, now) = issuing();
        let mut sink = Observed::default();
        let mut last = None;
        for _ in 0..(1u32 << 16) {
            last = Some(
                identifications
                    .next(TUPLE, now)
                    .expect("a fresh cycle issues the whole space"),
            );
        }
        let last = last.expect("the cycle issued something");
        identifications.accepted(last);
        let wrote = now + Duration::from_secs(1);
        identifications.terminal(Terminal::wrote(last, wrote));

        let (emitted, _) = oversized(&mut identifications, wrote, &mut sink);
        assert_eq!(emitted, Emitted::Denied(Denial::Exhausted));
        assert!(sink.batches.is_empty());
        assert_eq!(identifications.exhausted(), 1);

        let (emitted, value) = oversized(&mut identifications, wrote + MDL, &mut sink);
        assert!(matches!(emitted, Emitted::Handed { refused: 0, .. }));
        assert_eq!(value, Some(1), "the new cycle starts at the beginning");
    }

    #[test]
    fn oversized_ipv6_is_one_batch_with_one_fragment_id() {
        let (mut identifications, now) = issuing();
        let mut fragment_identification = 0;
        let mut sink = Observed::default();
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: None,
                oversized: true,
                now,
            },
            1_280,
            &mut fragment_identification,
            |identification| {
                assert_eq!(identification, None);
                Ok(datagram(4_000))
            },
            &mut sink,
        );
        assert_eq!(fragment_identification, 1);
        assert_eq!(sink.batches.len(), 1);
        assert_eq!(
            emitted,
            Emitted::Handed {
                queued: sink.batches[0].packets().len(),
                refused: 0,
            }
        );
        assert_eq!(sink.batches[0].guarded(), None);
        assert!(sink.batches[0].packets().iter().all(|packet| {
            u32::from_be_bytes(
                packet[IPV6_HEADER_LEN + 4..IPV6_HEADER_LEN + 8]
                    .try_into()
                    .unwrap(),
            ) == 1
        }));
    }

    #[test]
    fn emitter_counts_packets_denials_and_reports_packetization_failures() {
        let opened = Instant::now();
        let now = opened + MDL;
        let source: IpAddr = TUPLE.0.into();
        let destination: IpAddr = TUPLE.1.into();
        let mut emitter = Emitter::new(1_280, opened);
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        assert!(
            emitter.emit(
                now,
                Addressed {
                    source,
                    destination,
                    protocol: 17,
                    size: 4_020,
                },
                |identification| Ok(ipv4_datagram(4_000, identification.unwrap())),
                &mut sink,
                &mut reports,
            ),
            "the whole datagram reached the interface queue"
        );
        assert_eq!(emitter.queued(), sink.batches[0].packets().len() as u64);
        assert_eq!(emitter.handoff_refused(), 0);
        let guarded = sink.batches[0].guarded().expect("guarded");
        assert_eq!(emitter.identifications().outstanding(), 1);
        emitter.terminal(Terminal::wrote(guarded, now));
        assert_eq!(emitter.identifications().outstanding(), 0);

        assert!(
            !emitter.emit(
                now,
                Addressed {
                    source,
                    destination,
                    protocol: 17,
                    size: 4_020,
                },
                |identification| {
                    let mut packet = ipv4_datagram(4_000, identification.unwrap());
                    packet[6] |= 0x40;
                    Ok(packet)
                },
                &mut sink,
                &mut reports,
            ),
            "a datagram the daemon could not split reached nothing"
        );
        assert_eq!(emitter.unbuildable(), 1);
        assert_eq!(reports.0.len(), 1);
        assert!(reports.0[0].contains("DF is set"));

        // Inside the opening quarantine nothing oversized is issued a value, and that is counted apart.
        assert!(
            !emitter.emit(
                opened,
                Addressed {
                    source,
                    destination,
                    protocol: 17,
                    size: 4_020,
                },
                |_| panic!("a denied datagram is never built"),
                &mut sink,
                &mut reports,
            ),
            "a denied datagram reached nothing either"
        );
        assert_eq!(emitter.denied(), 1);
        assert_eq!(reports.0.len(), 1, "a denial is not a daemon defect");
        assert_eq!(
            emitter.queued(),
            sink.batches[0].packets().len() as u64,
            "and neither of them was counted as queued"
        );

        assert!(!emitter.handed(false));
        assert_eq!(emitter.handoff_refused(), 1);
        assert!(emitter.handed(true));
    }

    #[test]
    fn a_refused_handoff_is_not_reported_as_queued() {
        let opened = Instant::now();
        let now = opened + MDL;
        let mut emitter = Emitter::new(1_280, opened);
        let mut sink = Observed {
            refuse: true,
            ..Observed::default()
        };
        let mut reports = Reports::default();
        assert!(
            !emitter.emit(
                now,
                Addressed {
                    source: TUPLE.0.into(),
                    destination: TUPLE.1.into(),
                    protocol: 17,
                    size: 4_020,
                },
                |identification| Ok(ipv4_datagram(4_000, identification.unwrap())),
                &mut sink,
                &mut reports,
            ),
            "a datagram the interface queue would not take reached nothing"
        );
        assert_eq!(emitter.queued(), 0);
        assert_eq!(
            emitter.handoff_refused(),
            sink.batches[0].packets().len() as u64,
            "every fragment of the one datagram is counted refused, and none queued"
        );
        assert_eq!(emitter.identifications().outstanding(), 0);
    }
}
