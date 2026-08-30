use std::net::IpAddr;
#[cfg(test)]
use std::net::Ipv6Addr;

use crate::shared::ip_wire::{Error as IpError, Packet};
use crate::shared::ipv4_identification::{Ipv4Identifications, Tuple};

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
    Fragmentable(u16),
}

#[derive(Debug, Clone, Copy)]
pub struct SizingInput {
    pub tuple: Option<Tuple>,
    pub oversized: bool,
}

pub fn size_policy(identifications: &mut Ipv4Identifications, input: SizingInput) -> Sizing {
    let SizingInput { tuple, oversized } = input;
    let Some(tuple) = tuple.filter(|_| oversized) else {
        return Sizing::Atomic;
    };
    Sizing::Fragmentable(identifications.next(tuple))
}

pub trait Sink {
    /// Takes every packet of one logical datagram together, or refuses all of them.
    fn datagram(&mut self, packets: Vec<Vec<u8>>) -> bool;
}

#[derive(Debug, PartialEq, Eq)]
pub enum Emitted {
    Written { written: usize, refused: usize },
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
    let identification = match size_policy(identifications, sizing) {
        Sizing::Atomic => None,
        Sizing::Fragmentable(identification) => Some(identification),
    };
    let packet = match build(identification) {
        Ok(packet) => packet,
        Err(e) => return Emitted::Unbuildable(e),
    };
    // Finish packetization before handing anything over. The sink therefore admits either the complete
    // logical datagram or none of it, and the serial writer cannot interleave another datagram's fragments.
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
            return Emitted::Unbuildable(e);
        }
    }
    let count = packets.len();
    if sink.datagram(packets) {
        Emitted::Written {
            written: count,
            refused: 0,
        }
    } else {
        Emitted::Written {
            written: 0,
            refused: count,
        }
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
    refused: u64,
    unwritable: u64,
}

impl Emitter {
    pub fn new(mtu: usize) -> Self {
        Self {
            mtu,
            identifications: Ipv4Identifications::new(),
            fragment_identification: 0,
            written: 0,
            refused: 0,
            unwritable: 0,
        }
    }

    pub fn mtu(&self) -> usize {
        self.mtu
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub fn refused(&self) -> u64 {
        self.refused
    }

    pub fn unwritable(&self) -> u64 {
        self.unwritable
    }

    pub fn identifications(&self) -> &Ipv4Identifications {
        &self.identifications
    }

    pub fn wrote(&mut self, accepted: bool) {
        if accepted {
            self.written += 1;
        } else {
            self.refused += 1;
        }
    }

    pub fn emit<S: Sink, R: Reporter>(
        &mut self,
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
        match outcome(emit(
            &mut self.identifications,
            SizingInput {
                tuple,
                oversized: size > self.mtu,
            },
            self.mtu,
            &mut self.fragment_identification,
            build,
            sink,
        )) {
            Outcome::Wrote { written, refused } => {
                self.written += written as u64;
                self.refused += refused as u64;
            }
            Outcome::Reported(e) => {
                self.unwritable += 1;
                reporter.unbuildable(source, destination, &e);
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Wrote { written: usize, refused: usize },
    Reported(WriterError),
}

pub fn outcome(emitted: Emitted) -> Outcome {
    match emitted {
        Emitted::Written { written, refused } => Outcome::Wrote { written, refused },
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

    const MTU: usize = 1_500;
    const SOURCE: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 1);
    const DESTINATION: Ipv6Addr = Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2);

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
        batches: Vec<Vec<Vec<u8>>>,
        refuse: bool,
    }

    impl Sink for Observed {
        fn datagram(&mut self, packets: Vec<Vec<u8>>) -> bool {
            self.batches.push(packets);
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
    fn sizing_only_allocates_for_oversized_ipv4() {
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut identifications = Ipv4Identifications::new();
        assert_eq!(
            size_policy(
                &mut identifications,
                SizingInput {
                    tuple: Some(tuple),
                    oversized: false,
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
                }
            ),
            Sizing::Atomic
        );
        let Sizing::Fragmentable(first) = size_policy(
            &mut identifications,
            SizingInput {
                tuple: Some(tuple),
                oversized: true,
            },
        ) else {
            panic!("oversized IPv4 must be fragmentable");
        };
        assert_eq!(
            size_policy(
                &mut identifications,
                SizingInput {
                    tuple: Some(tuple),
                    oversized: true,
                }
            ),
            Sizing::Fragmentable(first.wrapping_add(1))
        );
    }

    #[test]
    fn packetization_hands_all_ipv4_fragments_over_once() {
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut identifications = Ipv4Identifications::new();
        let identification = Cell::new(None);
        let mut fragment_identification = 0;
        let mut sink = Observed::default();
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: Some(tuple),
                oversized: true,
            },
            1_280,
            &mut fragment_identification,
            |value| {
                identification.set(value);
                Ok(ipv4_datagram(4_000, value.unwrap()))
            },
            &mut sink,
        );

        assert_eq!(sink.batches.len(), 1);
        let packets = &sink.batches[0];
        assert!(packets.len() > 1);
        assert_eq!(
            emitted,
            Emitted::Written {
                written: packets.len(),
                refused: 0,
            }
        );
        assert!(packets.iter().all(|packet| packet.len() <= 1_280));
        assert!(packets.iter().all(|packet| {
            Ipv4Header::from_slice(packet).unwrap().0.identification
                == identification.get().unwrap()
        }));
    }

    #[test]
    fn sink_refusal_refuses_the_whole_batch_and_does_not_reuse_its_id() {
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut identifications = Ipv4Identifications::new();
        let first = Cell::new(None);
        let mut fragment_identification = 0;
        let mut sink = Observed {
            refuse: true,
            ..Observed::default()
        };
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: Some(tuple),
                oversized: true,
            },
            1_280,
            &mut fragment_identification,
            |value| {
                first.set(value);
                Ok(ipv4_datagram(4_000, value.unwrap()))
            },
            &mut sink,
        );
        assert_eq!(sink.batches.len(), 1);
        assert_eq!(
            emitted,
            Emitted::Written {
                written: 0,
                refused: sink.batches[0].len(),
            }
        );

        let second = Cell::new(None);
        emit(
            &mut identifications,
            SizingInput {
                tuple: Some(tuple),
                oversized: true,
            },
            1_280,
            &mut fragment_identification,
            |value| {
                second.set(value);
                Ok(ipv4_datagram(4_000, value.unwrap()))
            },
            &mut sink,
        );
        assert_eq!(second.get(), Some(first.get().unwrap().wrapping_add(1)));
    }

    #[test]
    fn unbuildable_packet_hands_nothing_to_the_sink() {
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut identifications = Ipv4Identifications::new();
        let mut fragment_identification = 0;
        let mut sink = Observed::default();
        let used = Cell::new(None);
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: Some(tuple),
                oversized: true,
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
        assert_eq!(
            identifications.next(tuple),
            used.get().unwrap().wrapping_add(1),
            "failure does not roll the tuple sequence back"
        );
    }

    #[test]
    fn oversized_ipv6_is_one_batch_with_one_fragment_id() {
        let mut identifications = Ipv4Identifications::new();
        let mut fragment_identification = 0;
        let mut sink = Observed::default();
        let emitted = emit(
            &mut identifications,
            SizingInput {
                tuple: None,
                oversized: true,
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
            Emitted::Written {
                written: sink.batches[0].len(),
                refused: 0,
            }
        );
        assert!(sink.batches[0].iter().all(|packet| {
            u32::from_be_bytes(
                packet[IPV6_HEADER_LEN + 4..IPV6_HEADER_LEN + 8]
                    .try_into()
                    .unwrap(),
            ) == 1
        }));
    }

    #[test]
    fn emitter_counts_packets_and_reports_packetization_failures() {
        let source: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let destination: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();
        let mut emitter = Emitter::new(1_280);
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        emitter.emit(
            Addressed {
                source,
                destination,
                protocol: 17,
                size: 4_020,
            },
            |identification| Ok(ipv4_datagram(4_000, identification.unwrap())),
            &mut sink,
            &mut reports,
        );
        assert_eq!(emitter.written(), sink.batches[0].len() as u64);
        assert_eq!(emitter.refused(), 0);

        emitter.emit(
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
        );
        assert_eq!(emitter.unwritable(), 1);
        assert_eq!(reports.0.len(), 1);
        assert!(reports.0[0].contains("DF is set"));

        emitter.wrote(false);
        assert_eq!(emitter.refused(), 1);
    }
}
