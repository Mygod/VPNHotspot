//! One read from the TUN, dispatched to the transport that owns it.
//!
//! Apart from the ingress task because it is the one piece of that task with no lifecycle in it: nothing here
//! admits, retires, joins or refunds. It borrows the owners the ingress task holds, decides which of them a
//! datagram belongs to, and counts what it could not place - which is why it is also where the counters live.
//!
//! It has to run twice on a single read, and that is what the grouping is for: once for the packet that
//! arrived, and once for the datagram whose last fragment it turned out to be. That second pass must be the
//! *same* path, or a reassembled datagram would be classified, parsed and admitted by rules that had drifted
//! from the ones a whole one meets.

use std::net::IpAddr;
use std::time::Instant;

use vpnhotspotd::shared::classify::{classify, Classified, Drop, Principal};
use vpnhotspotd::shared::icmp_error::Reason;
use vpnhotspotd::shared::udp_wire::{self, Reject};
use vpnhotspotd::shared::{echo_wire, extension, reassembly, tcp_wire};

use crate::echo;
use crate::gateway::Gateways;
use crate::output::Output;
use crate::tcp;
use crate::tun_writer::Stamp;
use crate::udp;
use crate::virtual_dns;
use vpnhotspotd::shared::admission::{Admission, Lease};

/// How many times one read may be unwrapped before it is refused. See [Dispatch::accept].
const PASSES: usize = 3;

/// Counters rather than per-packet logs: the input is attacker-influenced, so a report per packet would be a
/// flood by construction. They are reported when the epoch changes and once at exit.
#[derive(Default)]
pub(crate) struct Counters {
    dns: u64,
    undeliverable_dns: u64,
    relayed: u64,
    tcp: u64,
    echo: u64,
    unsupported: u64,
    fragmented: u64,
    fragments_denied: u64,
    fragments_overlapping: u64,
    fragments_expired: u64,
    fragments_unreported: u64,
    extended: u64,
    chain_refused: u64,
    unparseable: u64,
    reserved: u64,
    unroutable: u64,
    malformed: u64,
    pub(crate) unadmitted: u64,
}

impl Counters {
    pub(crate) fn describe(&self, epoch: u64) -> String {
        format!(
            "epoch {epoch}: dns {} undeliverable-dns {} relayed {} tcp {} echo {} unsupported {} \
             fragmented {} fragments-denied {} fragments-overlapping {} fragments-expired {} \
             fragments-unreported {} extended {} chain-refused {} unparseable {} reserved {} \
             unroutable {} malformed {} unadmitted {}",
            self.dns,
            self.undeliverable_dns,
            self.relayed,
            self.tcp,
            self.echo,
            self.unsupported,
            self.fragmented,
            self.fragments_denied,
            self.fragments_overlapping,
            self.fragments_expired,
            self.fragments_unreported,
            self.extended,
            self.chain_refused,
            self.unparseable,
            self.reserved,
            self.unroutable,
            self.malformed,
            self.unadmitted
        )
    }
}

/// The owners one datagram may reach, borrowed together.
///
/// Grouped rather than passed as nine arguments because the dispatch has to run twice on a single read - see
/// the module note.
pub(crate) struct Dispatch<'a> {
    pub(crate) counters: &'a mut Counters,
    pub(crate) relay: &'a mut udp::Relay,
    pub(crate) echo: &'a mut echo::Relay,
    pub(crate) dns: &'a mut virtual_dns::Handoff,
    pub(crate) tcp: &'a mut tcp::Engine,
    pub(crate) fragments: &'a mut reassembly::Table,
    pub(crate) output: &'a mut Output,
    pub(crate) admission: &'a mut Admission,
    /// The reassembly table's own aggregate owner. Passed beside the admission because the table charges
    /// through it rather than holding a pool of its own.
    pub(crate) fragment_lease: &'a Lease,
    pub(crate) gateways: &'a Gateways,
    /// The retirement a packet is dispatched under, which the writer gates on again when it dequeues.
    pub(crate) stamp: Stamp,
    pub(crate) virtual_addresses: &'a [IpAddr],
}

impl Dispatch<'_> {
    pub(crate) fn accept(&mut self, packet: &[u8], now: Instant) {
        // Up to three passes, and the bound is the number of wrappings one packet can carry rather than a
        // guess: an extension chain in front of a Fragment header is stripped first, reassembly then completes
        // the datagram, and whatever chain sat *behind* the Fragment header - in the fragmentable part, where
        // RFC 8200 allows one - is stripped from the result. Each pass strictly unwraps, so nothing loops.
        let mut rewritten: Option<Vec<u8>> = None;
        for _ in 0..PASSES {
            let produced = match rewritten.as_deref() {
                Some(current) => self.deliver(current, now),
                None => self.deliver(packet, now),
            };
            match produced {
                Some(produced) => rewritten = Some(produced),
                None => return,
            }
        }
        // A packet still asking to be unwrapped after three passes is one no conforming sender produces.
        self.counters.unparseable += 1;
    }

    /// Dispatches one whole datagram, or holds one fragment, or unwraps one extension chain. Returns a packet
    /// that still has to be dispatched, which is either the datagram a fragment completed or one with its
    /// extension headers removed.
    fn deliver(&mut self, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        match classify(packet, self.virtual_addresses) {
            // Answered rather than relayed, and it must never reach the relay: the destination is an
            // address the daemon occupies.
            Classified::Accepted {
                principal: Principal::Dns,
                provisional,
            } => {
                self.counters.dns += 1;
                match udp_wire::parse(packet) {
                    Ok(datagram) if !provisional => {
                        self.dns.submit(datagram, self.output, self.admission)
                    }
                    // A provisional fragment carries no ports yet, so it is not known to be DNS at all and goes
                    // to reassembly to find out.
                    Err(Reject::Fragmented) => return self.fragment(packet, now),
                    Err(Reject::Extended) => return self.unwrap(packet),
                    // TCP port 53 to a virtual address is the same principal over the terminating engine, which
                    // answers it from the resolver rather than from an upstream connection.
                    Err(Reject::NotUdp) => match tcp_wire::peek(packet) {
                        Ok(segment) => {
                            self.counters.tcp += 1;
                            self.tcp.accept(
                                packet,
                                segment,
                                true,
                                now,
                                self.output,
                                self.admission,
                            );
                        }
                        Err(_) => self.counters.undeliverable_dns += 1,
                    },
                    _ => self.counters.undeliverable_dns += 1,
                }
            }
            Classified::Accepted {
                principal: Principal::Ipv4 | Principal::Ipv6,
                ..
            } => match udp_wire::parse(packet) {
                Ok(datagram) => {
                    self.counters.relayed += 1;
                    self.relay
                        .relay(packet, datagram, self.gateways, self.output, self.admission);
                }
                // Each transport gets its own strict parse rather than one dispatch on the protocol byte,
                // because what "well formed" means differs per transport and the reason a packet was refused
                // is worth counting separately.
                Err(Reject::NotUdp) => match tcp_wire::peek(packet) {
                    Ok(segment) => {
                        self.counters.tcp += 1;
                        self.tcp
                            .accept(packet, segment, false, now, self.output, self.admission);
                    }
                    Err(_) => match echo_wire::parse(packet) {
                        Ok(request) => {
                            self.counters.echo += 1;
                            self.echo.relay(
                                packet,
                                request,
                                self.gateways,
                                self.output,
                                self.admission,
                            );
                        }
                        // The last parse in the chain, so this is the only place that can conclude nothing
                        // handles the packet: an ICMP type Android's own downstream link control owns, or a
                        // protocol this mode does not carry.
                        Err(Reject::NotUdp) => self.counters.unsupported += 1,
                        Err(Reject::Fragmented) => return self.fragment(packet, now),
                        Err(Reject::Extended) => return self.unwrap(packet),
                        Err(Reject::Malformed(_)) => self.counters.unparseable += 1,
                    },
                },
                Err(Reject::Fragmented) => return self.fragment(packet, now),
                Err(Reject::Extended) => return self.unwrap(packet),
                Err(Reject::Malformed(_)) => self.counters.unparseable += 1,
            },
            Classified::Dropped(Drop::Reserved) => self.counters.reserved += 1,
            Classified::Dropped(Drop::Unroutable) => self.counters.unroutable += 1,
            Classified::Dropped(Drop::Malformed) => self.counters.malformed += 1,
        }
        None
    }

    /// Removes one packet's IPv6 extension chain, and hands back what is left for the transports to parse.
    ///
    /// Removed rather than preserved because egress goes out through a datagram socket, so the kernel builds
    /// the IPv6 header and there is nowhere to carry a chain - the same reason the source address changes.
    fn unwrap(&mut self, packet: &[u8]) -> Option<Vec<u8>> {
        self.counters.extended += 1;
        match extension::walk(packet) {
            Ok(extension::Walked::Stripped(stripped)) => Some(stripped),
            // Nothing to remove after a transport already refused it as extended, which means the chain is one
            // the walk does not recognise as one.
            Ok(extension::Walked::None) => {
                self.counters.unsupported += 1;
                None
            }
            // A chain refused rather than walked: source routing, a misplaced hop-by-hop header, or one built
            // to be expensive. Counted apart from a malformed packet because it is well formed and unwelcome.
            Err(_) => {
                self.counters.chain_refused += 1;
                None
            }
        }
    }

    /// Holds one fragment, and hands back the datagram if that was the last one missing.
    fn fragment(&mut self, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.counters.fragmented += 1;
        match self
            .fragments
            .accept(packet, now, self.admission, self.fragment_lease)
        {
            Ok(reassembly::Accepted::Pending) => None,
            Ok(reassembly::Accepted::Complete(whole)) => Some(whole),
            // Counted with the reason it was refused rather than as one drop: a ceiling that is full and a
            // sender overlapping its own fragments are different problems with different fixes.
            Err(reassembly::Reject::Denied) => {
                self.counters.fragments_denied += 1;
                None
            }
            Err(reassembly::Reject::Overlap) => {
                self.counters.fragments_overlapping += 1;
                None
            }
            Err(reassembly::Reject::Malformed(_)) => {
                self.counters.unparseable += 1;
                None
            }
        }
    }

    /// Answers the reassembly timeouts a router owes, from the interface's own address.
    ///
    /// Only for contexts that received fragment zero, since the error has to quote a header the client
    /// actually sent, and never for a context dropped under resource pressure - that is the daemon's own
    /// limit, not a property of the path, and reporting it would tell the client something untrue.
    pub(crate) fn expire(&mut self, now: Instant) {
        // One quote at a time rather than a batch: however many contexts expire together, only the one being
        // reported exists, and it is dropped before the next is built.
        let counters = &mut self.counters;
        let gateways = &self.gateways;
        let output = &mut self.output;
        let stamp = self.stamp;
        let admission = &mut *self.admission;
        let fragment_lease = self.fragment_lease;
        self.fragments
            .sweep(now, admission, fragment_lease, |quote| {
                counters.fragments_expired += 1;
                match gateways.report(&quote, Reason::ReassemblyExpired) {
                    Some(error) => output.packet(stamp, error),
                    None => counters.fragments_unreported += 1,
                }
            });
    }
}
