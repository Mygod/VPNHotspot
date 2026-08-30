use std::io;
use std::net::IpAddr;
use std::time::Instant;

use vpnhotspotd::shared::classify::{classify, Classified, Drop, Principal};
use vpnhotspotd::shared::icmp_error::Reason;
use vpnhotspotd::shared::udp_wire::{self, Reject};
use vpnhotspotd::shared::{echo_wire, extension, reassembly, tcp_wire};

use crate::shizuku::echo;
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::tcp;
use crate::shizuku::udp;
use crate::shizuku::virtual_dns;
use vpnhotspotd::shared::admission::Admission;

/// Session counters avoid attacker-controlled per-packet reporting.
#[derive(Default)]
pub(crate) struct Counters {
    dns: u64,
    undeliverable_dns: u64,
    relayed: u64,
    tcp: u64,
    echo: u64,
    unsupported: u64,
    fragmented: u64,
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
    pub(crate) fn describe(&self) -> String {
        format!(
            "dns {} undeliverable-dns {} relayed {} tcp {} echo {} unsupported {} \
             fragmented {} fragments-overlapping {} fragments-expired {} \
             fragments-unreported {} extended {} chain-refused {} unparseable {} reserved {} \
             unroutable {} malformed {} unadmitted {}",
            self.dns,
            self.undeliverable_dns,
            self.relayed,
            self.tcp,
            self.echo,
            self.unsupported,
            self.fragmented,
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
pub(crate) struct Dispatch<'a> {
    pub(crate) counters: &'a mut Counters,
    pub(crate) relay: &'a mut udp::Relay,
    pub(crate) echo: &'a mut echo::Relay,
    pub(crate) dns: &'a mut virtual_dns::Handoff,
    pub(crate) tcp: &'a mut tcp::Engine,
    pub(crate) fragments: &'a mut reassembly::Table,
    pub(crate) output: &'a mut Output,
    pub(crate) admission: &'a mut Admission,
    pub(crate) gateways: &'a Gateways,
    pub(crate) virtual_addresses: &'a [IpAddr],
}

impl Dispatch<'_> {
    /// Only daemon-owned resolver-wrapper failures escape as `Err`; packet failures are handled locally.
    pub(crate) fn accept(&mut self, packet: &[u8], now: Instant) -> io::Result<()> {
        // Continue while each step makes structural progress by stripping an extension chain or completing and
        // removing a Fragment header. Both come from a finite protocol packet, so no transform-count cap is
        // needed.
        let mut rewritten: Option<Vec<u8>> = None;
        loop {
            let produced = match rewritten.as_deref() {
                Some(current) => self.deliver(current, now)?,
                None => self.deliver(packet, now)?,
            };
            match produced {
                Some(produced) => rewritten = Some(produced),
                None => return Ok(()),
            }
        }
    }

    /// Dispatches one whole datagram, or holds one fragment, or unwraps one extension chain. Returns a packet
    /// that still has to be dispatched, which is either the datagram a fragment completed or one with its
    /// extension headers removed.
    fn deliver(&mut self, packet: &[u8], now: Instant) -> io::Result<Option<Vec<u8>>> {
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
                        self.dns.submit(datagram, self.output, self.admission)?
                    }
                    // A provisional fragment carries no ports yet, so it is not known to be DNS at all and goes
                    // to reassembly to find out.
                    Err(Reject::Fragmented) => return Ok(self.fragment(packet, now)),
                    Err(Reject::Extended) => return Ok(self.unwrap(packet)),
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
                        Err(Reject::Fragmented) => return Ok(self.fragment(packet, now)),
                        Err(Reject::Extended) => return Ok(self.unwrap(packet)),
                        Err(Reject::Malformed(_)) => self.counters.unparseable += 1,
                    },
                },
                Err(Reject::Fragmented) => return Ok(self.fragment(packet, now)),
                Err(Reject::Extended) => return Ok(self.unwrap(packet)),
                Err(Reject::Malformed(_)) => self.counters.unparseable += 1,
            },
            Classified::Dropped(Drop::Reserved) => self.counters.reserved += 1,
            Classified::Dropped(Drop::Unroutable) => self.counters.unroutable += 1,
            Classified::Dropped(Drop::Malformed) => self.counters.malformed += 1,
        }
        Ok(None)
    }

    /// Removes one packet's IPv6 extension chain, and hands back what is left for the transports to parse.
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
            // A chain refused rather than walked: source routing, a misplaced hop-by-hop header, or an
            // unsupported header. Counted apart from a malformed packet because it is well formed and unwelcome.
            Err(_) => {
                self.counters.chain_refused += 1;
                None
            }
        }
    }

    /// Holds one fragment, and hands back the datagram if that was the last one missing.
    fn fragment(&mut self, packet: &[u8], now: Instant) -> Option<Vec<u8>> {
        self.counters.fragmented += 1;
        match self.fragments.accept(packet, now) {
            Ok(reassembly::Accepted::Pending) => None,
            Ok(reassembly::Accepted::Complete(whole)) => Some(whole),
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
    pub(crate) fn expire(&mut self, now: Instant) {
        // Reassembly yields one expired quote at a time; each resulting ICMP error is handed off as its own
        // logical datagram.
        let counters = &mut self.counters;
        let gateways = &self.gateways;
        let output = &mut self.output;
        self.fragments.sweep(now, |quote| {
            counters.fragments_expired += 1;
            match gateways.report(&quote, Reason::ReassemblyExpired) {
                Some(error) => output.packet(error),
                None => counters.fragments_unreported += 1,
            }
        });
    }
}
