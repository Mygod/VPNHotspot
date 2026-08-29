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
    /// One owner, because the size comparison, the identification decision and which counter moves are one
    /// decision - see [vpnhotspotd::shared::packet_writer::Emitter].
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
    pub(crate) fn terminal(&mut self, terminal: Terminal) {
        self.emitter.terminal(terminal);
    }

    /// Emits one UDP datagram toward a client, splitting it only if the interface cannot carry it whole.
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
    /// places: the DF decision, then the source fragmentation it authorizes.
    fn emit(
        &mut self,
        stamp: Stamp,
        source: IpAddr,
        destination: IpAddr,
        protocol: u8,
        size: usize,
        build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
    ) {
        // One call, and the owner decides everything: the size comparison, whether an Identification can be
        // issued, which counter moves, and whether anything is reported. A caller that passed its own idea of
        // "oversized" or incremented its own counter would be deciding what this exists to decide.
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
    pub(crate) fn packet(&mut self, stamp: Stamp, packet: Vec<u8>) {
        self.enqueue(stamp, packet);
    }

    /// A refusal here is the daemon's own queue being full, which is an admission decision: the packet is
    /// dropped rather than retried, and nothing was charged for it that needs refunding.
    fn enqueue(&mut self, stamp: Stamp, packet: Vec<u8>) {
        let accepted = self.writer.enqueue(stamp, packet, None).is_ok();
        self.emitter.wrote(accepted);
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "mtu {} written {} blocked {} unwritable {} identification-denied {}; {}",
            self.emitter.mtu(),
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
