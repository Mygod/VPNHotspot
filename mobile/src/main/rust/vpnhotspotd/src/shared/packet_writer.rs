//! Packetization for the common TUN writer: final size validation, source fragmentation for both
//! families, and the size policy that decides which datagrams may clear DF.
//!
//! Platform-neutral on purpose. The writer task that owns the descriptor, its bounded queue, and the
//! `EAGAIN` wait live next to the TUN; everything here is a pure transformation of bytes and is unit
//! tested as such. The Identification allocator this consults lives beside it in
//! [crate::shared::ipv4_identification], because what an Identification may be reused for is a question
//! about wire time rather than about bytes.
//!
//! Only daemon-originated packets pass through this module, which is why the unfragmentable part is
//! always the fixed IPv6 header: nothing here emits Hop-by-Hop, Routing, or leading Destination
//! Options, and a packet that carries one is rejected rather than fragmented incorrectly.

use std::net::IpAddr;
#[cfg(test)]
use std::net::Ipv6Addr;
use std::time::Instant;

use crate::shared::ipv4_identification::{
    Denial, Guarded, Ipv4Identifications, Prepared, Terminal, Tuple,
};

use etherparse::{IpFragOffset, Ipv4Header};

pub const IPV4_HEADER_LEN: usize = 20;
pub const IPV6_HEADER_LEN: usize = 40;
pub const IPV6_FRAGMENT_HEADER_LEN: usize = 8;

/// Extension headers that belong to the unfragmentable part, so a datagram carrying one cannot be
/// fragmented by the simple split below: Hop-by-Hop, Routing, and Destination Options.
const UNFRAGMENTABLE_NEXT_HEADERS: [u8; 3] = [0, 43, 60];
const IPV6_FRAGMENT_NEXT_HEADER: u8 = 44;

/// Fragment offsets count 8-byte units, so every fragment except the last carries a multiple of 8.
const FRAGMENT_ALIGNMENT: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum WriterError {
    /// The caller built something that is not a well-formed packet of the family it claims.
    Malformed(&'static str),
    /// Would exceed the link MTU and the caller forbade fragmenting it.
    TooLarge { size: usize, mtu: usize },
    /// Correct behaviour is to drop and let path-MTU signalling handle it, not to fragment.
    Unfragmentable(&'static str),
}

/// Rejects anything whose declared length disagrees with its actual length, which is the last chance to
/// catch a packetization bug before it reaches a client, and anything larger than the link.
pub fn validate(packet: &[u8], mtu: usize) -> Result<(), WriterError> {
    match packet.first().map(|byte| byte >> 4) {
        Some(4) => {
            if packet.len() < IPV4_HEADER_LEN {
                return Err(WriterError::Malformed(
                    "IPv4 packet shorter than its header",
                ));
            }
            let declared = u16::from_be_bytes([packet[2], packet[3]]) as usize;
            if declared != packet.len() {
                return Err(WriterError::Malformed("IPv4 total length disagrees"));
            }
        }
        Some(6) => {
            if packet.len() < IPV6_HEADER_LEN {
                return Err(WriterError::Malformed(
                    "IPv6 packet shorter than its header",
                ));
            }
            let declared = u16::from_be_bytes([packet[4], packet[5]]) as usize;
            if declared != packet.len() - IPV6_HEADER_LEN {
                return Err(WriterError::Malformed("IPv6 payload length disagrees"));
            }
        }
        _ => return Err(WriterError::Malformed("not an IPv4 or IPv6 packet")),
    }
    if packet.len() > mtu {
        return Err(WriterError::TooLarge {
            size: packet.len(),
            mtu,
        });
    }
    Ok(())
}

/// Splits one oversized IPv6 datagram into fragments that each fit `mtu`.
///
/// The caller decides whether fragmenting is allowed at all: ICMPv6 errors truncate their quote instead,
/// and TCP segments to fit, so only relayed UDP, Echo Reply, and UDP virtual-DNS responses arrive here.
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
    // every fragment carries the original header plus a fragment header, so that is the fixed overhead
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
        // 13-bit offset in 8-byte units, two reserved bits, then the More Fragments flag
        fragment.extend_from_slice(&(((offset as u16) & !7) | u16::from(more)).to_be_bytes());
        fragment.extend_from_slice(&identification.to_be_bytes());
        fragment.extend_from_slice(&payload[offset..end]);
        // Handed over as it is built, so exactly one fragment exists beside the source packet at any moment.
        // The batch this replaces held every fragment of an oversized datagram at once, which for a 64 KiB
        // datagram at a 1280-byte MTU is a second copy of the whole thing in fifty-odd allocations - and how
        // large the datagram is, is a remote's choice.
        emit(fragment);
        emitted += 1;
        offset = end;
    }
    if emitted == 0 {
        return Err(WriterError::Malformed("IPv6 datagram has no payload"));
    }
    Ok(emitted)
}

/// Splits one oversized IPv4 datagram into fragments that each fit `mtu`.
///
/// This is a different case from the downstream MTU floor, and both exist. A datagram over the floor but
/// within the interface is handed to Android whole with DF clear, because Android knows the real downstream
/// and fragmenting it here would guess. This is for what will not fit the interface at all, which nothing
/// downstream can rescue: the interface is what the daemon writes through.
///
/// The Identification is whatever the caller already put in the header, so
/// [crate::shared::ipv4_identification::Ipv4Identifications] is consulted once per datagram rather than once
/// per fragment - which is also what makes the fragments reassemble into one datagram instead of several.
pub fn fragment_ipv4(
    packet: &[u8],
    mtu: usize,
    mut emit: impl FnMut(Vec<u8>),
) -> Result<usize, WriterError> {
    let (mut header, payload) =
        Ipv4Header::from_slice(packet).map_err(|_| WriterError::Malformed("not an IPv4 packet"))?;
    // Only daemon-originated packets reach here and none of them carries options, so rather than copy an
    // option chain correctly - which depends on each option's copied flag - this refuses one.
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
        // covers the length, flags, and offset just rewritten, and to_bytes serializes what is stored
        header.header_checksum = header.calc_header_checksum();
        let mut fragment = Vec::with_capacity(IPV4_HEADER_LEN + end - offset);
        fragment.extend_from_slice(&header.to_bytes());
        fragment.extend_from_slice(&payload[offset..end]);
        // One at a time - see the note in [fragment_ipv6].
        emit(fragment);
        emitted += 1;
        offset = end;
    }
    if emitted == 0 {
        return Err(WriterError::Malformed("IPv4 datagram has no payload"));
    }
    Ok(emitted)
}

/// What the size policy decides for one outbound datagram, before any header is built.
///
/// A type rather than an `Option<u16>`, because the three answers are genuinely different and collapsing two
/// of them is the defect this replaces: "no Identification" and "an Identification could not be issued" were
/// both `None`, so a datagram the allocator refused went out atomic with DF set as though it had been within
/// the floor all along. That is a fragment set a downstream must refuse, and the refusal is not a fact about
/// the client's path - it is this table being full, which the client cannot act on and would cache as though
/// it could.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sizing {
    /// Within the downstream floor, or not IPv4: atomic with DF set, and no Identification is needed.
    Atomic,
    /// Above the floor, with an Identification the downstream may fragment under.
    Fragmentable(Guarded),
    /// Above the floor, and no Identification could be issued. The datagram is dropped, quietly and counted.
    Denied(Denial),
}

/// What deciding one datagram's size policy needs to know about it.
///
/// One value rather than three parameters because the three are one question - may this datagram clear DF,
/// and under what Identification - and because the two functions that ask it must ask it the same way.
///
/// `oversized` is the caller's own comparison against the downstream floor, and `tuple` is `None` for
/// anything that is not IPv4-to-IPv4 - neither is this module's to work out, and both are what make the
/// policy testable without a socket. `now` is likewise the caller's: whether an Identification may be issued
/// is a question about elapsed wire time, and a clock read in here could not be injected.
#[derive(Debug, Clone, Copy)]
pub struct Guarding {
    pub tuple: Option<Tuple>,
    pub oversized: bool,
    pub now: Instant,
}

/// Decides one datagram's size policy, which is the one place the three answers are told apart.
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

/// Where a finished packet goes. One trait so the emit sequence below can be driven without a TUN, and so
/// that a test can count exactly what reached the wire.
pub trait Sink {
    /// Answers whether it was accepted. A refusal is the daemon's own queue being full, which is a drop
    /// rather than a retry.
    ///
    /// `guarded` travels with the packet because the writer is the only owner that learns whether it reached
    /// the wire, and the allocator is the only owner that can act on that - see
    /// [crate::shared::ipv4_identification::Terminal]. Every packet of one guarded datagram carries the same
    /// identity, and an unguarded packet carries none.
    fn packet(&mut self, packet: Vec<u8>, guarded: Option<Guarded>) -> bool;
}

/// What one datagram's emission came to.
#[derive(Debug, PartialEq, Eq)]
pub enum Emitted {
    /// Written, whole or in fragments. Answers how many reached the sink and how many the sink refused.
    Written { written: usize, blocked: usize },
    /// Above the downstream floor, IPv4, and no Identification could be issued. Nothing was built, nothing
    /// was fragmented, nothing reached the sink - and nothing is reported, because which tuples arrive is
    /// traffic. See [Sizing::Denied].
    Denied(Denial),
    /// The daemon could not build or split what it was asked for. Its own failure, and the caller's to
    /// report: distinct from a denial, which is a bounded table doing exactly what it is for.
    Unbuildable(WriterError),
}

/// The whole size policy and emission of one datagram, in the order the decisions have to happen.
///
/// One function because the ordering is the correctness property, and because the denial has to short-circuit
/// *before* the header is built. `build` is not called at all on the denied path - not with `None`, which is
/// what "no Identification, so set DF" used to mean and is a fragment set a downstream must refuse.
///
/// The other ordering it owns is the registration. A guarded packet is taken onto the allocator's books
/// *before* the sink can have it and given back if the sink refuses it, because from the moment the writer
/// owns a packet its terminal may arrive at any time - and a count incremented after the handover could be
/// decremented before it existed. A packet the sink refused was never the writer's, so it is rolled back
/// rather than settled: nothing reached the wire and nothing claims to have.
///
/// And the issuance itself is a transaction, which is the third ordering. The value has to be decided before
/// the header exists, because it goes inside the header - but a datagram that then failed to build, or whose
/// every packet the sink refused, has put nothing carrying that value anywhere. Spending the sequence
/// position anyway is safe for a receiver and bad for the sender: 65,536 attempts that never reached the
/// writer would deny a tuple its oversized output, and a client that keeps the queue full can arrange
/// exactly that. So the position is given back when, and only when, the sink accepted nothing at all. A
/// fragmented datagram of which even one fragment was accepted keeps it.
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
        // Before `build`, so no header exists and no allocation was made for one.
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
    // The one place the transaction closes, so no exit above can forget it. `accepted` rather than the
    // outcome, because a fragmentation that failed *after* the sink took a fragment is still a value that
    // packets carry: what decides this is whether anything with that Identification exists, not which answer
    // the emission came to.
    if let Some(guarded) = guarded {
        if accepted == 0 {
            identifications.unissued(guarded);
        }
    }
    emitted
}

/// Builds one datagram and hands whatever it becomes to the sink, answering with what crossed and how many
/// packets the sink actually took.
///
/// Split from [emit] only so that every way out of it - a build failure, a refused fragmentation, a sink
/// that took nothing - passes through the one place that closes the issuance transaction.
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
            // A guarded packet the allocator cannot promise to hear the ending of is dropped exactly as a
            // full writer queue would drop it. Sending it untracked would leave a value on the wire whose age
            // this daemon could never know.
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
    // Larger than the interface itself, so nothing downstream can rescue it and the split happens here. Each
    // fragment goes to the sink as it is built, so at most one exists beside the source packet.
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

/// One datagram's addressing and length, as the owner needs them.
///
/// Grouped so the emission reads as one step: what is being sent, to whom, under which protocol, and how big
/// it is. The size is the datagram's own, and the floor comparison is the owner's - a caller that passed its
/// own "oversized" would be deciding the thing the owner exists to decide.
#[derive(Debug, Clone, Copy)]
pub struct Addressed {
    pub source: IpAddr,
    pub destination: IpAddr,
    pub protocol: u8,
    pub size: usize,
}

/// Where a structured report goes. Injected so the owner's classification can be observed without a
/// reporting backend, and so a test can assert that a quiet path really is quiet.
pub trait Reporter {
    /// One packetization failure worth raising. Called for nothing else.
    fn unbuildable(&mut self, source: IpAddr, destination: IpAddr, error: &WriterError);
}

/// The one place a TUN-side datagram becomes packets, with the counters that say what happened to them.
///
/// The owner rather than a free function, because the decisions and the counters have to be one thing: the
/// floor comparison decides whether an Identification is needed at all, the identification table decides
/// whether one can be issued, and which counter moves - and whether anything is reported - follows from
/// both. Splitting them let a caller pass its own idea of "oversized" and increment its own idea of a
/// counter, which is exactly how a denial came to look like a report.
pub struct Emitter {
    /// The largest TUN-side packet the interface will carry.
    mtu: usize,
    /// The largest TUN-side IPv4 packet that may keep DF set, which is the narrowest downstream's MTU.
    floor: usize,
    identifications: Ipv4Identifications,
    /// A single 32-bit sequence for IPv6 fragmentation, unlike IPv4's per-tuple one.
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
            floor: mtu,
            identifications: Ipv4Identifications::new(prepared),
            fragment_identification: 0,
            written: 0,
            blocked: 0,
            unwritable: 0,
            identification_denied: 0,
        }
    }

    /// A floor of zero means the app measured nothing, so the interface is the only limit there is. Clamped
    /// to the interface either way: a downstream wider than the TUN cannot rescue a packet the TUN will not
    /// carry.
    pub fn set_floor(&mut self, floor: usize) {
        self.floor = if floor == 0 {
            self.mtu
        } else {
            floor.min(self.mtu)
        };
    }

    pub fn floor(&self) -> usize {
        self.floor
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

    /// Oversized IPv4 output dropped because no Identification could be issued for it, whichever of the three
    /// reasons it was - see [Denial], and [Emitter::identifications] for the breakdown.
    pub fn identification_denied(&self) -> u64 {
        self.identification_denied
    }

    /// The allocator itself, for the session's own reporting and for tests that assert about its state.
    pub fn identifications(&self) -> &Ipv4Identifications {
        &self.identifications
    }

    /// Applies one ending the TUN writer sent back for a guarded packet it owned.
    ///
    /// The only path by which a wire time enters this owner. Everything else here happens before a packet is
    /// handed over, and none of it knows whether the handover ended on the wire.
    pub fn terminal(&mut self, terminal: Terminal) {
        self.identifications.terminal(terminal);
    }

    /// Records one already-formed packet handed straight to the writer - a terminating TCP segment, or a
    /// locally originated ICMP error - whose size was settled by whoever built it, so there is no size
    /// decision to make and no Identification to issue.
    pub fn wrote(&mut self, accepted: bool) {
        if accepted {
            self.written += 1;
        } else {
            // The daemon's own queue was full, which is an admission decision: the packet is dropped rather
            // than retried.
            self.blocked += 1;
        }
    }

    /// Emits one datagram: decides its size policy against the floor, builds it, writes or splits it, and
    /// moves exactly one counter.
    ///
    /// `size` is the datagram's own length, and the comparison against the floor happens *here* - a caller
    /// that passed its own "oversized" would be deciding the thing this owner exists to decide. `protocol`
    /// keys the Identification allocator and so is only read on the IPv4 path. `now` is read on that path
    /// too, and only there: whether a value may be issued depends on when its tuple last reached the wire.
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
                // The floor comparison, made once and here.
                oversized: size > self.floor,
                now,
            },
            self.mtu,
            &mut self.fragment_identification,
            build,
            sink,
        );
        // Which counter moves, and whether anything is reported, follows from what the emission came to.
        // Collapsing the two is how a bounded table doing exactly its job becomes a report per datagram.
        match outcome(emitted) {
            Outcome::Wrote { written, blocked } => {
                self.written += written as u64;
                // The daemon's own queue was full, which is an admission decision: the packet is dropped
                // rather than retried, and nothing was charged for it that needs refunding.
                self.blocked += blocked as u64;
            }
            // Counted rather than reported. Which tuples arrive is traffic, so a report per refused datagram
            // is a flood a client drives; repeated attempts coalesce into this counter and nothing else. Its
            // own counter rather than [Emitter::unwritable], because the two mean different things: that one
            // is the daemon unable to build something it should have been able to build, and this is a
            // bounded table doing exactly what it is for.
            Outcome::Counted => self.identification_denied += 1,
            Outcome::Reported(e) => {
                self.unwritable += 1;
                reporter.unbuildable(source, destination, &e);
            }
        }
    }
}

/// What the *owner* does with an emission: which counter it moves, and whether anything is reported.
///
/// Separate from [Emitted] because the two decisions are different, and collapsing them is how a bounded
/// table doing its job becomes a structured report per datagram. A denial is traffic - which tuples arrive is
/// a client's choice - so it is counted and nothing else; a packetization failure is the daemon unable to
/// build something it should have been able to build, so it is reported.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Written, whole or in fragments, with however many the writer refused.
    Wrote { written: usize, blocked: usize },
    /// Counted quietly. No report, no allocation, nothing on the wire.
    Counted,
    /// Reported, because the daemon failed at something it asked for.
    Reported(WriterError),
}

/// Maps one emission onto what the owner does about it.
pub fn outcome(emitted: Emitted) -> Outcome {
    match emitted {
        Emitted::Written { written, blocked } => Outcome::Wrote { written, blocked },
        Emitted::Denied(_) => Outcome::Counted,
        Emitted::Unbuildable(e) => Outcome::Reported(e),
    }
}

/// Builds a minimal IPv6 header for this module's packetization tests.
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

    /// Collects a streamed fragmentation, so a test can assert about the whole sequence while production
    /// still owns one fragment at a time.
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
            // the fragment header repeats the original upper-layer protocol
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

    /// One oversized IPv4 datagram with DF already cleared, which is the only shape that reaches
    /// [fragment_ipv4].
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
            // every fragment repeats the datagram's Identification, which is what splices them back
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
        // DF set: the correct behaviour is to drop and let path-MTU signalling handle it
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
        // an option chain, whose per-option copied flag this deliberately does not implement
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

    /// A table whose opening window has already passed, prepared for `tuples` and tracking `tracked`
    /// unsettled packets. The allocator's own behaviour is proved in [crate::shared::ipv4_identification];
    /// what these tests are about is the emit sequence around it.
    fn table(tuples: usize, tracked: usize) -> (Ipv4Identifications, Instant) {
        let opened = Instant::now();
        let table = Ipv4Identifications::new(Prepared {
            tuples,
            tracked,
            opened,
        });
        (table, opened + NONREUSE_WINDOW)
    }

    /// The common size-policy input: an IPv4 tuple above the downstream floor, which is the only case that
    /// asks the allocator for anything.
    fn guarding(tuple: Tuple, now: Instant) -> Guarding {
        Guarding {
            tuple: Some(tuple),
            oversized: true,
            now,
        }
    }

    /// An emitter whose opening window has already passed, so a test that is not about the window does not
    /// have to step over it.
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

    /// Counts exactly what crossed each boundary the emit sequence has: the builder, the sink, and the
    /// caller's own report.
    #[derive(Default)]
    struct Observed {
        packets: Vec<usize>,
        /// The identity each accepted or refused packet carried, so a test can prove that every fragment of
        /// one datagram is one datagram.
        guarded: Vec<Option<Guarded>>,
        refuse: bool,
        /// Accept this many and refuse the rest, which is a queue that fills part-way through a datagram.
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

    /// A table-full denial is quiet and complete: nothing is built, nothing is fragmented, nothing reaches
    /// the writer, and the caller gets exactly one countable answer with no report in it.
    ///
    /// Both sizes, because the two used to differ: an oversized datagram fell through to fragmentation with
    /// no Identification, and one merely above the floor went out atomic as though it had been within it.
    #[test]
    fn a_denied_identification_builds_nothing_and_writes_nothing() {
        for size in [800usize, 4_000] {
            let (mut identifications, now) = table(1, 64);
            let held = Ipv4Addr::new(192, 0, 2, 1);
            let newcomer = Ipv4Addr::new(192, 0, 2, 2);
            let remote = Ipv4Addr::new(198, 51, 100, 1);
            // The one slot goes to a tuple that is already sending, and it writes, so its bucket cannot be
            // reclaimed from under it.
            let guarded = match size_policy(&mut identifications, guarding((held, remote, 17), now))
            {
                Sizing::Fragmentable(guarded) => guarded,
                other => panic!("the held tuple should have one: {other:?}"),
            };
            identifications.register(guarded).expect("tracked");
            identifications.terminal(Terminal::wrote(guarded, now));

            // One sink, and it is *the* sink production was given. An earlier version of this test asserted
            // about a second `Observed` that was never passed in, so it would have passed however many
            // packets the real one received. The builder's count lives in a `Cell` beside it so both can be
            // read while the sink is borrowed mutably.
            let built = std::cell::Cell::new(0usize);
            let mut sink = Observed::default();
            let mut fragment_id = 0u32;
            // Above the downstream floor either way, which is the only case that asks for one.
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
            // And the owner counts it rather than reporting it, which is the other half of "quiet".
            assert_eq!(outcome(emitted), Outcome::Counted, "size {size}");
        }
    }

    /// The opening window is the same quiet denial as a full table, and it is answered before the table is
    /// even consulted.
    ///
    /// This is the one thing per-tuple state cannot see: a session that started a second after its
    /// predecessor stopped would hand out values the predecessor had just written, and a receiver holding
    /// those fragments would splice the two together. Sixty seconds of denial is exactly as long as it can
    /// still be holding them.
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
        assert_eq!(built.get(), 0, "nothing was built");
        assert!(sink.packets.is_empty(), "nothing reached the writer");
        assert_eq!(fragment_id, 0, "no fragment identity was spent");
        assert!(identifications.is_empty(), "and no tuple was recorded");
        assert_eq!(identifications.quarantined(), 3);

        // At exactly the window it opens, and the sequence starts where a fresh sequence starts.
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

    /// The window is about *guarded* output and nothing else: below the floor, IPv6, and the already-formed
    /// packets a terminating TCP stack produces are all unaffected by it.
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
        emitter.set_floor(1_200);
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();

        // Within the floor: atomic with DF set, which needs no Identification and so asks for none.
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
        // IPv6, oversized: source-fragmented here, under its own 32-bit sequence.
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
        // And the TCP path, which hands over a packet it segmented itself.
        emitter.wrote(true);

        assert_eq!(emitter.identification_denied(), 0, "none of it was guarded");
        assert_eq!(emitter.identifications().quarantined(), 0);
        assert!(reports.raised.is_empty());
        assert!(sink.packets.len() > 2, "the IPv6 datagram was fragmented");
        assert!(emitter.written() >= 3);
    }

    /// Repeated denials stay quiet: one countable answer each, no allocation, and the tuples that were
    /// already here keep their monotone sequences throughout.
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
            // Written, so neither bucket can be reclaimed from under it while the newcomers arrive.
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
        assert_eq!(denied, 5_000, "every newcomer, quietly");
        assert_eq!(reports, 0, "and not one of them was reported");
        assert_eq!(built.get(), 0, "nor built a header");
        assert!(sink.packets.is_empty(), "nor reached the writer");
        assert_eq!(identifications.len(), 2, "nothing was displaced");
        assert_eq!(
            identifications.reclaimed(),
            0,
            "and nothing was taken from anyone"
        );
        assert_eq!(identifications.refused(), 5_000);
        assert_eq!(identifications.sweeps(), 1, "one scan, five thousand tries");

        // The tuples that were already here carry straight on from where they were.
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

    /// A datagram the table *can* identify is built, split and written - which is what makes the denial above
    /// a distinct answer rather than the only one this path has.
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
                assert_eq!(identification, Some(1), "the builder is given the sequence");
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
        // Every fragment of one datagram is one datagram: one identity, one Identification, and one
        // registration each - which is what a receiver reassembles them by.
        let first = sink.guarded[0].expect("guarded");
        assert_eq!(first.identification(), 1);
        assert_eq!(first.tuple(), tuple);
        assert!(sink.guarded.iter().all(|guarded| *guarded == Some(first)));
        assert_eq!(
            identifications.outstanding(),
            written,
            "each accepted fragment is one packet the writer owes an ending for"
        );

        // A writer that refuses counts as blocked rather than as a failure, and the rest of the datagram
        // still goes - and none of the refused ones is on the books, because the writer never had them.
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

    /// A queue that fills part-way through a datagram leaves exactly the accepted fragments on the books, and
    /// settling them in any order neither underflows the count nor lets the sequence restart early.
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

        // The two accepted ones settle, the later write last in time but first to arrive.
        let guarded = sink.guarded[0].expect("guarded");
        let later = now + Duration::from_secs(10);
        identifications.terminal(Terminal::wrote(guarded, later));
        identifications.terminal(Terminal::wrote(guarded, now));
        assert_eq!(identifications.outstanding(), 0);
        assert_eq!(identifications.stale(), 0, "neither was stale");
        assert_eq!(identifications.settled(), (2, 0));
    }

    /// A datagram the sink took nothing of gives its Identification back; one it took part of keeps it.
    ///
    /// Both halves matter and they are the same test because the boundary between them is the whole rule.
    /// Keeping a value nothing carries is what let a full queue spend a tuple's cycle without one packet
    /// reaching the wire; giving back a value some fragment already carries would hand it out again while
    /// the writer still had the first one.
    #[test]
    fn only_a_datagram_that_reached_nothing_gives_its_identification_back() {
        let (mut identifications, now) = table(4, 64);
        let tuple = (
            Ipv4Addr::new(192, 0, 2, 1),
            Ipv4Addr::new(198, 51, 100, 1),
            17,
        );
        let mut fragment_id = 0u32;

        // Refused outright: the value never left this table.
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
        assert_eq!(identifications.outstanding(), 0, "nothing on the books");

        // A build that fails is the same: no header exists, so nothing carries the value.
        let emitted = emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(1), "the very same value");
                Err(WriterError::Malformed("nothing was built"))
            },
            &mut refused,
        );
        assert!(matches!(emitted, Emitted::Unbuildable(_)));
        assert_eq!(identifications.unissued_count(), 2);

        // And one the sink takes part of keeps it, so the next datagram moves on.
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
                assert_eq!(identification, Some(1), "still the first value");
                Ok(ipv4_datagram(4_000))
            },
            &mut partial,
        );
        assert_eq!(identifications.unissued_count(), 2, "this one was carried");
        emit(
            &mut identifications,
            guarding(tuple, now),
            1_280,
            &mut fragment_id,
            |identification| {
                assert_eq!(identification, Some(2), "so the sequence moved on");
                Ok(ipv4_datagram(800))
            },
            &mut partial,
        );
    }

    /// A caller-requested DF that cannot be honoured is the daemon's own failure, and stays on its own
    /// answer - never collapsed into the quiet denial above.
    #[test]
    fn a_df_too_large_packet_is_unbuildable_rather_than_denied() {
        let (mut identifications, now) = table(4, 64);
        let mut sink = Observed::default();
        let mut fragment_id = 0u32;
        // Built with DF set and larger than the interface: nothing downstream can rescue it, and this daemon
        // may not split it either.
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
        assert!(sink.packets.is_empty(), "and nothing partial was written");
        // A different answer from a denial, and the owner does a different thing with it: this one is
        // reported, where a denial is only counted.
        assert_eq!(
            outcome(emitted),
            Outcome::Reported(WriterError::Unfragmentable("DF is set"))
        );
    }

    /// Everything the owner's report path was asked to raise.
    #[derive(Default)]
    struct Reports {
        raised: Vec<String>,
    }

    impl Reporter for Reports {
        fn unbuildable(&mut self, _source: IpAddr, _destination: IpAddr, error: &WriterError) {
            self.raised.push(format!("{error:?}"));
        }
    }

    /// The quiet denial, through the owner method production calls, at both sizes that ask for an
    /// Identification.
    ///
    /// The floor comparison is the owner's - the test passes a *size*, not an `oversized` flag - and the
    /// counter asserted is the owner's own, not a stand-in. Both matter: the earlier shape let the caller
    /// decide the thing this exists to decide, and count the thing this exists to count.
    #[test]
    fn the_owner_denies_quietly_at_both_sizes_that_need_an_identification() {
        // Floor 1200, interface 1500: one size between them and one above.
        for size in [1_400usize, 4_000] {
            let (mut emitter, now) = opened_emitter(1_500, 1);
            emitter.set_floor(1_200);
            assert_eq!(emitter.floor(), 1_200);
            let held: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
            let newcomer: IpAddr = Ipv4Addr::new(192, 0, 2, 2).into();
            let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();

            // The one slot goes to a tuple that is already sending oversized output.
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
                    assert_eq!(identification, Some(1), "the held tuple gets its sequence");
                    Ok(ipv4_datagram(size))
                },
                &mut sink,
                &mut reports,
            );
            assert_eq!(emitter.identification_denied(), 0);
            let wrote = emitter.written();
            assert!(wrote > 0, "size {size}: the held tuple's output went");
            // Its packets reached the wire, so its bucket cannot be given away under the newcomers below.
            for guarded in sink.guarded.iter().flatten() {
                emitter.terminal(Terminal::wrote(*guarded, now));
            }

            // A newcomer at the same size: denied, quietly.
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

    /// A datagram that does not need an Identification is unaffected by a full table - the owner's floor
    /// comparison is what decides, and it decides before the table is asked.
    #[test]
    fn the_owner_does_not_ask_for_an_identification_below_its_floor() {
        let (mut emitter, now) = opened_emitter(1_500, 1);
        emitter.set_floor(1_200);
        let held: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let newcomer: IpAddr = Ipv4Addr::new(192, 0, 2, 2).into();
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        // Fill the one slot.
        emitter.emit(
            now,
            Addressed {
                source: held,
                destination: remote,
                protocol: 17,
                size: 1_400,
            },
            |_| Ok(ipv4_datagram(1_400)),
            &mut sink,
            &mut reports,
        );

        // A newcomer *within* the floor never asks, so a full table cannot deny it.
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
                assert_eq!(identification, None, "atomic, so none is needed");
                Ok(ipv4_datagram(800))
            },
            &mut sink,
            &mut reports,
        );
        assert_eq!(built.get(), 1, "it was built and sent");
        assert_eq!(emitter.identification_denied(), 0);
        assert_eq!(
            emitter.identifications().refused(),
            0,
            "the table was never asked"
        );
        assert!(reports.raised.is_empty());
    }

    /// A caller-requested DF that cannot be honoured is the owner's own failure and stays on its own
    /// counter, with a report - which is what makes the denial above a different thing rather than the only
    /// thing this path has.
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

        assert_eq!(emitter.unwritable(), 1, "the daemon's own failure");
        assert_eq!(emitter.identification_denied(), 0, "and not a denial");
        assert_eq!(reports.raised.len(), 1, "reported exactly once");
        assert!(
            reports.raised[0].contains("DF is set"),
            "{:?}",
            reports.raised
        );
        assert!(sink.packets.is_empty(), "and nothing partial was written");
    }

    /// Repeated denials stay quiet through the owner, and the tuples already there keep their sequences.
    #[test]
    fn the_owner_stays_quiet_under_repeated_denial() {
        let (mut emitter, now) = opened_emitter(1_500, 2);
        emitter.set_floor(1_200);
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
                    size: 1_400,
                },
                |identification| {
                    assert_eq!(identification, Some(1));
                    Ok(ipv4_datagram(1_400))
                },
                &mut sink,
                &mut reports,
            );
        }
        // Both reached the wire, so neither bucket may be given to a newcomer.
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
                    size: 1_400,
                },
                |_| {
                    built.set(built.get() + 1);
                    Ok(ipv4_datagram(1_400))
                },
                &mut sink,
                &mut reports,
            );
        }
        assert_eq!(emitter.identification_denied(), 5_000, "every one, counted");
        assert_eq!(built.get(), 0, "and none of them built a header");
        assert!(reports.raised.is_empty(), "nor raised a report");
        assert_eq!(emitter.written(), written, "nor reached the writer");
        assert_eq!(emitter.identifications().sweeps(), 1, "one scan, not 5,000");

        // And the tuples that were already here carry straight on, monotonically.
        for expected in 2..=6u16 {
            for source in &held {
                emitter.emit(
                    now,
                    Addressed {
                        source: *source,
                        destination: remote,
                        protocol: 17,
                        size: 1_400,
                    },
                    |identification| {
                        assert_eq!(identification, Some(expected), "monotone");
                        Ok(ipv4_datagram(1_400))
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

    /// A tuple that has spent its whole sequence is denied through the owner exactly as a full table is:
    /// quietly, counted, and with nothing built - and it opens again only once its window has passed with
    /// every accepted packet settled.
    #[test]
    fn the_owner_denies_a_spent_sequence_until_its_window_has_passed() {
        let (mut emitter, now) = opened_emitter(1_500, 4);
        emitter.set_floor(1_200);
        let source: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let remote: IpAddr = Ipv4Addr::new(198, 51, 100, 1).into();
        let addressed = Addressed {
            source,
            destination: remote,
            protocol: 17,
            size: 1_400,
        };
        let mut sink = Observed::default();
        let mut reports = Reports::default();
        for _ in 0..65_536u32 {
            emitter.emit(
                now,
                addressed,
                |_| Ok(ipv4_datagram(1_400)),
                &mut sink,
                &mut reports,
            );
            // Settled as it goes, so the tracking bound is never the thing under test here.
            let guarded = sink.guarded.pop().flatten().expect("guarded");
            sink.packets.pop();
            emitter.terminal(Terminal::wrote(guarded, now));
        }
        assert_eq!(emitter.identification_denied(), 0, "all 65,536 went");

        let built = std::cell::Cell::new(0usize);
        emitter.emit(
            now + NONREUSE_WINDOW - Duration::from_nanos(1),
            addressed,
            |_| {
                built.set(built.get() + 1);
                Ok(ipv4_datagram(1_400))
            },
            &mut sink,
            &mut reports,
        );
        assert_eq!(emitter.identification_denied(), 1, "the 65,537th, counted");
        assert_eq!(emitter.identifications().exhausted(), 1);
        assert_eq!(built.get(), 0, "and nothing built for it");
        assert!(reports.raised.is_empty(), "nor reported");

        // At exactly the window, with nothing outstanding, the sequence may start again.
        emitter.emit(
            now + NONREUSE_WINDOW,
            addressed,
            |identification| {
                assert_eq!(identification, Some(1), "a fresh sequence");
                Ok(ipv4_datagram(1_400))
            },
            &mut sink,
            &mut reports,
        );
        assert_eq!(emitter.identification_denied(), 1, "still exactly the one");
    }

    /// A full table denies the datagram outright, whatever its size, and never falls back to sending it
    /// without an Identification.
    ///
    /// The two answers this keeps apart used to be one `None`: a datagram the allocator refused went out
    /// atomic with DF set as though it had been within the downstream floor, so a downstream that was
    /// expected to fragment it had to refuse it instead - and that refusal is not a fact about the client's
    /// path, it is this table being full.
    #[test]
    fn a_full_table_denies_the_datagram_rather_than_clearing_its_identification() {
        let (mut identifications, now) = table(1, 64);
        let held = Ipv4Addr::new(192, 0, 2, 1);
        let newcomer = Ipv4Addr::new(192, 0, 2, 2);
        let remote = Ipv4Addr::new(198, 51, 100, 1);

        // The one tuple the table can hold takes its slot, and writes, so the slot is not reclaimable.
        let Sizing::Fragmentable(guarded) =
            size_policy(&mut identifications, guarding((held, remote, 17), now))
        else {
            panic!("the held tuple should have one");
        };
        assert_eq!(guarded.identification(), 1);
        identifications.register(guarded).expect("tracked");
        identifications.terminal(Terminal::wrote(guarded, now));

        // A newcomer, oversized: denied, not made atomic.
        assert_eq!(
            size_policy(&mut identifications, guarding((newcomer, remote, 17), now)),
            Sizing::Denied(Denial::AtCapacity)
        );
        // The same newcomer, *not* oversized: atomic, because it never needed one - and this must not consume
        // a slot or count as a refusal either.
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
        // IPv6 likewise never asks.
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

        // Repeated attempts from the same refused tuple coalesce into the counter and nothing else: no tuple
        // is displaced and no sequence restarts.
        for _ in 0..10_000 {
            assert_eq!(
                size_policy(&mut identifications, guarding((newcomer, remote, 17), now)),
                Sizing::Denied(Denial::AtCapacity)
            );
        }
        assert_eq!(identifications.len(), 1);
        assert_eq!(identifications.refused(), refused + 10_000);

        // And the tuple that was already here carries straight on, monotonically, throughout.
        for expected in 2..=6u16 {
            let Sizing::Fragmentable(guarded) =
                size_policy(&mut identifications, guarding((held, remote, 17), now))
            else {
                panic!("the held tuple keeps its sequence");
            };
            assert_eq!(guarded.identification(), expected);
        }
    }

    /// A genuine packetization failure is a different answer from a denied Identification, and stays on its
    /// own path.
    #[test]
    fn a_denied_identification_is_not_a_packetization_failure() {
        // DF set on something too large is the caller's own request being impossible, which is an error the
        // builder returns - not a size-policy answer at all.
        let mut packet = ipv4_datagram(4000);
        packet[6] = 0x40;
        assert!(matches!(
            fragment_ipv4(&packet, 1280, |_| panic!("nothing may be emitted")),
            Err(WriterError::Unfragmentable("DF is set"))
        ));
        // An MTU with no room is likewise its own classified failure.
        assert!(matches!(
            fragment_ipv4(&ipv4_datagram(4000), IPV4_HEADER_LEN, |_| panic!(
                "nothing may be emitted"
            )),
            Err(WriterError::TooLarge { .. })
        ));
        // Neither of those is reachable from a denial, because a denial never reaches the builder: it is
        // answered before a header exists.
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

    /// Fragmentation hands each fragment over as it is built, so only one exists beside the source packet.
    #[test]
    fn fragmentation_owns_one_fragment_at_a_time() {
        for ipv6 in [false, true] {
            // The largest datagram there is, at the smallest MTU either family allows.
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
                // The caller consumes it - here, by dropping it - before the next is built.
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

    /// A fragmentation that cannot happen emits nothing at all, so there is no partial datagram to unwind.
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
        // An already-fragmented IPv4 packet is refused before anything is built too.
        let mut packet = ipv4_datagram(4000);
        packet[6] = 0x20;
        assert!(matches!(
            fragment_ipv4(&packet, 1280, |_| emitted += 1),
            Err(WriterError::Unfragmentable(_))
        ));
        assert_eq!(emitted, 0);
    }
}
