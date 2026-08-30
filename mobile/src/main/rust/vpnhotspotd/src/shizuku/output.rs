use std::net::{IpAddr, SocketAddr};

use etherparse::{IpNumber, UdpHeader};
use vpnhotspotd::shared::echo_wire::{self, Identity, ECHO_HEADER_LEN};
use vpnhotspotd::shared::packet_writer::{
    Addressed, Emitter, Reporter, Sink, WriterError, IPV4_HEADER_LEN, IPV6_HEADER_LEN,
};
use vpnhotspotd::shared::udp_wire::build_reply;

use crate::report;
use crate::shizuku::tun_writer::Writer;

pub(crate) struct Output {
    /// The size policy, the identification table and every packet handoff counter. One owner, because the
    /// size comparison, the identification decision and which counter moves are one decision - see
    /// [vpnhotspotd::shared::packet_writer::Emitter].
    emitter: Emitter,
    writer: Writer,
}

impl Output {
    pub(crate) fn new(mtu: usize, writer: Writer) -> Self {
        Self {
            emitter: Emitter::new(mtu),
            writer,
        }
    }

    /// Emits one UDP datagram toward a client, splitting it only if the interface cannot carry it whole.
    pub(crate) fn datagram(
        &mut self,
        source: SocketAddr,
        destination: SocketAddr,
        hop_limit: u8,
        payload: &[u8],
    ) {
        let size = header_len(source.is_ipv6()) + UdpHeader::LEN + payload.len();
        self.emit(
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
        remote: IpAddr,
        client: IpAddr,
        hop_limit: u8,
        identity: Identity,
        payload: &[u8],
    ) {
        let size = header_len(remote.is_ipv6()) + ECHO_HEADER_LEN + payload.len();
        self.emit(remote, client, IpNumber::ICMP.0, size, |id| {
            echo_wire::build_reply(remote, client, hop_limit, id, identity, payload)
        });
    }

    /// The size policy, which is otherwise the easiest thing in the design to get subtly different in two
    /// places: the DF decision, then the source fragmentation it authorizes.
    fn emit(
        &mut self,
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
            Addressed {
                source,
                destination,
                protocol,
                size,
            },
            build,
            &mut Queue {
                writer: &self.writer,
            },
            &mut Structured,
        );
    }

    /// Emits one already-formed IP packet: bytes whose size was settled by whoever built them, so there is no
    /// size decision left to make and no fragmentation to consider.
    pub(crate) fn packet(&mut self, packet: Vec<u8>) {
        self.enqueue(packet);
    }

    /// The unbounded handoff refuses only after its writer receiver has closed. This packet is then dropped;
    /// it owns no descriptor lease or persistent state to unwind, and the writer task's end is already a
    /// session-ending dataplane event.
    fn enqueue(&mut self, packet: Vec<u8>) {
        let accepted = self.writer.enqueue(vec![packet]).is_ok();
        self.emitter.wrote(accepted);
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "mtu {} queued-packets {} writer-closed-packets {} unwritable {}; {}",
            self.emitter.mtu(),
            self.emitter.written(),
            self.emitter.refused(),
            self.emitter.unwritable(),
            self.emitter.identifications().describe(),
        )
    }
}

/// The TUN writer, as the shared emit sequence sees it.
struct Queue<'a> {
    writer: &'a Writer,
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
    fn datagram(&mut self, packets: Vec<Vec<u8>>) -> bool {
        self.writer.enqueue(packets).is_ok()
    }
}

fn header_len(ipv6: bool) -> usize {
    if ipv6 {
        IPV6_HEADER_LEN
    } else {
        IPV4_HEADER_LEN
    }
}
