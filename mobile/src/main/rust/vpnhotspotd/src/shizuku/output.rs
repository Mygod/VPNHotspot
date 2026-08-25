//! The one place a TUN-side datagram becomes packets.
//!
//! Every producer goes through here rather than building its own, and that is not tidiness. The two
//! Identification allocators this owns - the IPv4 table in [vpnhotspotd::shared::ipv4_identification] and
//! the IPv6 fragment sequence beside it - must be shared to be correct: a receiver reassembles on source,
//! destination, protocol, and Identification, and *ports are not in that tuple*, so two mappings from one
//! client - or a UDP reply and a DNS answer to the same client - collide unless one allocator hands out
//! both. A per-producer allocator would look right and mis-splice datagrams.
//!
//! It also puts the whole size policy in one function, which is otherwise the easiest thing in the design
//! to get subtly different in two places: the DF decision against the downstream floor, and source
//! fragmentation against the interface.
//!
//! The one thing it does *not* decide is when an Identification may be used again, because that is a fact
//! about the wire rather than about this owner: the TUN writer says whether and when each guarded packet
//! was written to the TUN, and [Output::terminal] is where that answer comes back. Written to the TUN is as
//! far as this process can see: what a downstream then does with it is not something the daemon observes.

use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use etherparse::{IpNumber, UdpHeader};
use vpnhotspotd::shared::echo_wire::{self, Identity, ECHO_HEADER_LEN};
use vpnhotspotd::shared::ipv4_identification::{Guarded, Prepared, Terminal};
use vpnhotspotd::shared::packet_writer::{
    Addressed, Emitter, Reporter, Sink, WriterError, IPV4_HEADER_LEN, IPV6_HEADER_LEN,
};
use vpnhotspotd::shared::udp_wire::build_reply;

use crate::report;
use crate::shizuku::tun_writer::{Stamp, Writer};

pub(crate) struct Output {
    /// The size policy, the identification table and every counter that says what happened to a datagram.
    /// One owner, because the floor comparison, the identification decision and which counter moves are one
    /// decision - see [vpnhotspotd::shared::packet_writer::Emitter].
    ///
    /// Built once per session and kept across every generation and epoch. A handover replaces sockets and
    /// retires flows; it does not replace the tuples a receiver is still holding fragments for, so an
    /// allocator rebuilt with the config would restart every sequence exactly when a client is most likely to
    /// still have the previous values in hand.
    emitter: Emitter,
    writer: Writer,
}

impl Output {
    /// The writer this output enqueues through, for the one caller that has to talk to it directly: the
    /// ingress task's handover fence, which retires the writer before it publishes a new stamp. Handed out
    /// rather than duplicated so there is still exactly one writer per session.
    pub(crate) fn writer(&self) -> &Writer {
        &self.writer
    }

    pub(crate) fn new(mtu: usize, prepared: Prepared, writer: Writer) -> Self {
        Self {
            emitter: Emitter::new(mtu, prepared),
            writer,
        }
    }

    /// Applies one ending the TUN writer sent back for a guarded packet it owned.
    ///
    /// The only path by which a wire time reaches the allocator, and the reason the ingress loop selects on
    /// the settlement channel at all: an Identification stays spent until the writer says what became of
    /// every packet carrying it.
    pub(crate) fn terminal(&mut self, terminal: Terminal) {
        self.emitter.terminal(terminal);
    }

    /// A floor of zero means the app measured nothing, so the interface is the only limit there is.
    pub(crate) fn set_floor(&mut self, floor: usize) {
        self.emitter.set_floor(floor);
    }

    /// Emits one UDP datagram toward a client, splitting it only if the interface cannot carry it whole.
    ///
    /// `stamp` is the retirement it was produced under, not the current one. The writer gates on that again
    /// when it dequeues, which is what catches a producer that built this just before a sweep.
    pub(crate) fn datagram(
        &mut self,
        stamp: Stamp,
        source: SocketAddr,
        destination: SocketAddr,
        hop_limit: u8,
        payload: &[u8],
    ) {
        let size = header_len(source.is_ipv6()) + UdpHeader::LEN + payload.len();
        self.emit(
            stamp,
            source.ip(),
            destination.ip(),
            IpNumber::UDP.0,
            size,
            |identification| build_reply(source, destination, hop_limit, identification, payload),
        );
    }

    /// Emits one Echo Reply toward a client, under the identifier and sequence it originally chose.
    ///
    /// Subject to the same size policy as a datagram rather than written straight through, because a ping is
    /// as free to be large as anything else and the downstream floor can shrink between a request and its
    /// reply - so a reply that fitted when it was asked for may not by the time it arrives.
    pub(crate) fn echo(
        &mut self,
        stamp: Stamp,
        remote: IpAddr,
        client: IpAddr,
        hop_limit: u8,
        identity: Identity,
        payload: &[u8],
    ) {
        let size = header_len(remote.is_ipv6()) + ECHO_HEADER_LEN + payload.len();
        self.emit(stamp, remote, client, IpNumber::ICMP.0, size, |id| {
            echo_wire::build_reply(remote, client, hop_limit, id, identity, payload)
        });
    }

    /// The size policy, which is otherwise the easiest thing in the design to get subtly different in two
    /// places: the DF decision against the downstream floor, then source fragmentation against the interface.
    ///
    /// `build` receives the Identification that decision produced rather than deciding for itself, because
    /// who may clear DF and what value it then carries is one question, and answering it per producer is how
    /// the producers would drift apart.
    ///
    /// `protocol` keys the Identification allocator and so is only ever read on the IPv4 path. Passing an
    /// IPv4 protocol number for a pair of IPv6 addresses is therefore harmless, which is why the two families
    /// can share one call.
    fn emit(
        &mut self,
        stamp: Stamp,
        source: IpAddr,
        destination: IpAddr,
        protocol: u8,
        size: usize,
        build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
    ) {
        // One call, and the owner decides everything: the floor comparison, whether an Identification can be
        // issued, which counter moves, and whether anything is reported. A caller that passed its own idea of
        // "oversized" or incremented its own counter would be deciding what this exists to decide.
        //
        // The clock is read here rather than inside the emitter so the packet policy remains a pure decision
        // over its inputs.
        self.emitter.emit(
            Instant::now(),
            Addressed {
                source,
                destination,
                protocol,
                size,
            },
            build,
            &mut Queue {
                writer: &self.writer,
                stamp,
            },
            &mut Structured,
        );
    }

    /// Emits one already-formed IP packet: bytes whose size was settled by whoever built them, so there is no
    /// size decision left to make and no fragmentation to consider.
    ///
    /// Not one producer but several. The terminating TCP stack is the busiest, because it segments to the
    /// advertised MTU itself, but the locally originated ICMP errors are here too - the Fragmentation Needed
    /// and the unreachables the dispatcher, the UDP relay and Echo raise - and those are built at or under
    /// the floor by construction and truncate their quote rather than growing.
    ///
    /// The invariant is the same for all of them and it is what makes the `None` below correct: **a packet
    /// that arrives here is atomic and carries no Identification this daemon issued.** Anything that might
    /// need one goes through [Output::datagram] or [Output::echo], where the floor comparison happens.
    pub(crate) fn packet(&mut self, stamp: Stamp, packet: Vec<u8>) {
        self.enqueue(stamp, packet);
    }

    /// A refusal here is the daemon's own queue being full, which is an admission decision: the packet is
    /// dropped rather than retried, and nothing was charged for it that needs refunding.
    ///
    /// Unguarded, per the invariant on [Output::packet]: there is no Identification to reuse and so no
    /// ending for the writer to report.
    fn enqueue(&mut self, stamp: Stamp, packet: Vec<u8>) {
        let accepted = self.writer.enqueue(stamp, packet, None).is_ok();
        self.emitter.wrote(accepted);
    }

    /// The size the TCP stack segments to, so it never emits something a downstream cannot carry and never
    /// needs the DF machinery above.
    pub(crate) fn floor(&self) -> usize {
        self.emitter.floor()
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "floor {} written {} blocked {} unwritable {} identification-denied {}; {}",
            self.emitter.floor(),
            self.emitter.written(),
            self.emitter.blocked(),
            self.emitter.unwritable(),
            self.emitter.identification_denied(),
            self.emitter.identifications().describe(),
        )
    }
}

/// The TUN writer, as the shared emit sequence sees it. One stamp for every packet of one datagram, because
/// they all belong to the retirement it was produced under.
struct Queue<'a> {
    writer: &'a Writer,
    stamp: Stamp,
}

/// The daemon's own reporting path, as the emitter sees it.
struct Structured;

impl Reporter for Structured {
    fn unbuildable(&mut self, source: IpAddr, destination: IpAddr, error: &WriterError) {
        report::message_with_details(
            "shizuku.tun_output",
            format!("cannot build a packet: {error:?}"),
            "packetization",
            [("source", source), ("destination", destination)],
        );
    }
}

impl Sink for Queue<'_> {
    fn packet(&mut self, packet: Vec<u8>, guarded: Option<Guarded>) -> bool {
        self.writer.enqueue(self.stamp, packet, guarded).is_ok()
    }
}

fn header_len(ipv6: bool) -> usize {
    if ipv6 {
        IPV6_HEADER_LEN
    } else {
        IPV4_HEADER_LEN
    }
}
