use std::net::{IpAddr, SocketAddr};
use std::time::Instant;

use etherparse::{IpNumber, UdpHeader};
use vpnhotspotd::shared::echo_wire::{self, Identity, ECHO_HEADER_LEN};
use vpnhotspotd::shared::ipv4_identification::Terminal;
use vpnhotspotd::shared::packet_writer::{
    Addressed, Emitter, Reporter, Sink, WriterError, IPV4_HEADER_LEN, IPV6_HEADER_LEN,
};
use vpnhotspotd::shared::tcp_device::Handoff;
use vpnhotspotd::shared::tun_handoff::{Batch, Writer};
use vpnhotspotd::shared::udp_wire::build_reply;

use crate::report;

pub(crate) struct Output {
    /// Owns sizing, Identification allocation, and handoff counters as one decision.
    emitter: Emitter,
    writer: Writer,
}

impl Output {
    /// `opened` starts the Identification allocator's opening quarantine.
    pub(crate) fn new(mtu: usize, opened: Instant, writer: Writer) -> Self {
        Self {
            emitter: Emitter::new(mtu, opened),
            writer,
        }
    }

    /// Emits one UDP datagram and reports whole-datagram handoff admission.
    pub(crate) fn datagram(
        &mut self,
        now: Instant,
        source: SocketAddr,
        destination: SocketAddr,
        hop_limit: u8,
        payload: &[u8],
    ) -> bool {
        let size = header_len(source.is_ipv6()) + UdpHeader::LEN + payload.len();
        self.emit(
            now,
            source.ip(),
            destination.ip(),
            IpNumber::UDP.0,
            size,
            |identification| build_reply(source, destination, hop_limit, identification, payload),
        )
    }

    /// Emits one Echo Reply and reports whole-datagram handoff admission.
    pub(crate) fn echo(
        &mut self,
        now: Instant,
        remote: IpAddr,
        client: IpAddr,
        hop_limit: u8,
        identity: Identity,
        payload: &[u8],
    ) -> bool {
        let size = header_len(remote.is_ipv6()) + ECHO_HEADER_LEN + payload.len();
        self.emit(now, remote, client, IpNumber::ICMP.0, size, |id| {
            echo_wire::build_reply(remote, client, hop_limit, id, identity, payload)
        })
    }

    /// Applies the shared size/Identification policy and reports whole-datagram admission.
    fn emit(
        &mut self,
        now: Instant,
        source: IpAddr,
        destination: IpAddr,
        protocol: u8,
        size: usize,
        build: impl FnOnce(Option<u16>) -> Result<Vec<u8>, WriterError>,
    ) -> bool {
        self.emitter.emit(
            now,
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
        )
    }

    /// Hands off one already-formed IP packet and counts any refusal.
    pub(crate) fn packet(&mut self, packet: Vec<u8>) -> bool {
        // Only [Output::emit] can attach a daemon-issued Identification.
        self.emitter
            .handed(self.writer.enqueue(Batch::new(vec![packet], None)).is_ok())
    }

    /// Whether the handoff can take another complete datagram.
    pub(crate) fn accepting(&self) -> bool {
        self.writer.accepting()
    }

    /// Waits for handoff capacity without spinning.
    pub(crate) async fn accepted(&self) {
        self.writer.accepted().await
    }

    /// Applies one guarded datagram's Identification settlement.
    pub(crate) fn settle(&mut self, terminal: Terminal) {
        self.emitter.terminal(terminal);
    }

    /// Describes handoff results; the serial writer reports TUN writes separately.
    pub(crate) fn describe(&self) -> String {
        format!(
            "mtu {} queued-packets {} handoff-refused-packets {} unbuildable {} \
             identification-denied {}; {}",
            self.emitter.mtu(),
            self.emitter.queued(),
            self.emitter.handoff_refused(),
            self.emitter.unbuildable(),
            self.emitter.denied(),
            self.emitter.identifications().describe(),
        )
    }
}

/// The interface handoff as the client-facing TCP stack sees it.
impl Handoff for Output {
    fn accepting(&self) -> bool {
        Output::accepting(self)
    }

    fn packet(&mut self, packet: Vec<u8>) -> bool {
        Output::packet(self, packet)
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
    fn datagram(&mut self, batch: Batch) -> bool {
        self.writer.enqueue(batch).is_ok()
    }
}

fn header_len(ipv6: bool) -> usize {
    if ipv6 {
        IPV6_HEADER_LEN
    } else {
        IPV4_HEADER_LEN
    }
}
