//! The outer UDP relay: one endpoint-independent, address-filtered mapping per TUN-visible source.
//!
//! "Endpoint-independent" is the whole shape. A mapping is keyed on where the datagram came from and not on
//! where it is going, owns one unconnected socket, and reuses it across destinations, so one client source
//! port keeps one upstream identity however many peers it talks to. "Address-filtered" is the matching
//! restriction on the way back: only a remote address this mapping actually sent to may reply.
//!
//! The table lives in the TUN ingress task, which is the only thing that reads client traffic, so there is
//! no lock on the hot path. Each mapping's replies are received by their own task, which reports them back
//! here rather than packetizing them. That is what keeps the Identification allocators *shared* - one per
//! session, in [crate::shizuku::output], reached by every producer - and puts every TUN write behind one owner. Two
//! mappings from the same client can share a reassembly tuple, so a per-mapping allocator would hand the
//! same value to both.
//!
//! A mapping is reachable only through [vpnhotspotd::shared::workers], which is what makes the refund honest:
//! the socket comes back out of it only once the receive task has run to completion, so the descriptor is
//! closed before the budget is told it is - not when retirement asked for it.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use socket2::{SockAddr, Socket};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use vpnhotspotd::shared::icmp_nat::{nat66_hop_limit, Nat66HopLimit};
use vpnhotspotd::shared::icmp_translate::{self, Correlation, Reported, Untranslatable};
use vpnhotspotd::shared::model::Network;
use vpnhotspotd::shared::send_history::{History, Resolution};
use vpnhotspotd::shared::udp_wire::Relayed;
use vpnhotspotd::shared::workers::{Ended, Terminal, Workers};

use vpnhotspotd::shared::admission::{logical_footprint, Admission, Class, Denied, Lease, Request};
use vpnhotspotd::shared::egress::RelayUpstream as Upstream;

use crate::report;
use crate::shizuku::egress::{self, Fragmentation};
use crate::shizuku::gateway::Gateways;
use crate::shizuku::output::Output;
use crate::shizuku::reply::{
    receive, reply_channel, reply_channel_bytes, Event, Gate, Sizing, ERROR_OR_READABLE,
};
use crate::shizuku::send_failure::{self, Failure};
use crate::shizuku::tun_writer::Stamp;
use vpnhotspotd::shared::icmp_error::Reason;

/// How many sends one mapping remembers for error correlation.
///
/// A behavioural bound rather than a resource cliff, and its overflow is benign: correlated translation ends at
/// the first resolution of any kind, so the history only has to span the datagrams sent between a mapping's
/// first send and the first error about one. Eight covers a query with retries and a traceroute probe with room
/// over; a burst deeper than that would usually be ambiguous anyway, and overflowing retires the history rather
/// than truncating it, which costs one optional error and no payload.
///
/// The total is bounded structurally rather than charged: at most this many fixed-size records per mapping,
/// against a mapping ceiling that is itself measured. So history can never deny admission to anything a client
/// can observe, and there is no shared pool to exhaust.
const HISTORY_DEPTH: usize = 8;

/// RFC 4787 REQ-5: a UDP mapping timer must not expire in less than two minutes, and a default of five
/// minutes or more is recommended. The recommendation is taken rather than the floor. Permitted-remote
/// records share it, since a remote whose mapping is gone has nothing left to filter.
///
/// https://www.rfc-editor.org/rfc/rfc4787#section-4.3
const MAPPING_TIMEOUT: Duration = Duration::from_secs(300);

/// How many distinct remotes one mapping's reply filter is prepared for.
///
/// The logical maximum, charged at the mapping's own admission and never grown, which is what makes a
/// permitted remote's admission honest: the bound is checked before the datagram goes out, so a client wanting
/// a sixty-fifth remote from one source port is refused the *new* remote - counted, nothing sent - rather than
/// having the send succeed and the bookkeeping find no slot behind it. An expired remote frees a slot the next
/// one takes; `with_capacity` requests the bound up front so the common case allocates nothing, and the
/// filter's own backing is count-bounded overhead rather than accounted state.
///
/// Sixty-four because endpoint-independent mapping is per source port and almost nothing uses one port for
/// more peers than that; a resolver querying every server it knows is the widest realistic case and is well
/// inside it.
const REMOTES_PREPARED: usize = 64;

/// Whether a mapping has actually started carrying traffic.
///
/// A mapping exists in the table from before its first datagram leaves, because its receive worker has to be
/// retained by then - so there is a window where the record is present and the exchange it stands for has not
/// happened. Nothing may treat such a record as a mapping: no reply matches it, no remote is permitted
/// through it, it holds no expiry deadline, and a second datagram for the same source does not send on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Admitted, built and retained; its first send has not been decided yet.
    Provisional,
    /// The first datagram left. This is a mapping.
    Live,
    /// The first send failed. The record is a tombstone waiting only for its worker's terminal, which is what
    /// closes the descriptor and releases the grant.
    RolledBack,
}

struct Mapping {
    state: State,
    /// Signalled once the first datagram has left, which is what lets the retained worker begin. Dropping it
    /// instead is the rollback: the worker wakes, has done nothing, and returns.
    commit: Option<oneshot::Sender<()>>,
    /// This mapping's share of the descriptor. The receive task holds the other, so the close happens when
    /// this is dropped after that task has been joined - see [Relay::close].
    socket: Arc<AsyncFd<Socket>>,
    /// The reply filter. One deadline each, so a remote the client stopped talking to stops being permitted
    /// even while the mapping lives on. Prepared to [REMOTES_PREPARED] and never grown.
    remotes: HashMap<IpAddr, Instant>,
    deadline: Instant,
    /// Everything this mapping owns, in one grant: its own record and its descriptor, one record per
    /// permitted remote, and the bytes of the collections above - which are prepared once and stay charged
    /// whatever expires out of them.
    ///
    /// Owned rather than counted. The arithmetic this replaces lived here, so a refund had to trust that the
    /// number and the state agreed; a lease cannot be released twice and cannot be released by a worker at
    /// all, and [Relay::close] releases it only after the task has been joined and this record dropped.
    lease: Lease,
    /// What this mapping recently sent, so an error claiming to be about one datagram can be checked against a
    /// real one. Optional state: losing it costs error translation and never payload.
    history: History,
}

/// What a mapping's reply filter has to say about one destination, before anything is sent to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Permit {
    /// Already permitted. Sending again refreshes its deadline and takes no slot, so an established
    /// conversation is never subject to the bound.
    Recorded,
    /// A destination this filter has not seen, and a free slot in [REMOTES_PREPARED] for it.
    Free,
    /// A destination this filter has not seen, and no slot left. The datagram is not sent.
    Full,
}

impl Mapping {
    /// Whether this destination may be sent to, which is the reply filter's whole admission rule.
    ///
    /// Asked *before* the send, because a datagram on the wire whose remote could not be recorded is a reply
    /// this mapping would drop. [REMOTES_PREPARED] is the logical maximum and the only condition; an expiry
    /// frees a slot - see [Mapping::expire_remotes] - and what the filter does with its own backing is not
    /// consulted.
    fn permits(&self, remote: IpAddr) -> Permit {
        if self.remotes.contains_key(&remote) {
            Permit::Recorded
        } else if self.remotes.len() < REMOTES_PREPARED {
            Permit::Free
        } else {
            Permit::Full
        }
    }

    /// Permits this destination until the mapping's own deadline, refreshing one already here.
    ///
    /// Infallible by construction: the caller asked [Mapping::permits] before it sent, so either the
    /// destination is already recorded or the bound had a slot for it.
    fn record_remote(&mut self, remote: IpAddr) {
        self.remotes.insert(remote, self.deadline);
    }

    /// Drops the destinations whose own deadlines have passed, returning how many so the caller can refund
    /// their records. Each one frees a logical slot the next destination may take.
    fn expire_remotes(&mut self, now: Instant) -> usize {
        let before = self.remotes.len();
        self.remotes.retain(|_, deadline| *deadline > now);
        before - self.remotes.len()
    }

    /// What one mapping owns *beyond the row that holds it*: its prepared reply filter, its history's deque
    /// backing, and the receive worker's ancillary buffer.
    ///
    /// The `Mapping` value itself is not here, and that is the point. A mapping lives inside a
    /// [vpnhotspotd::shared::workers::Held] row in [Relay::mappings], whose own charge already counts
    /// `(SocketAddr, Held<Mapping>)` for every row the table is prepared for - so adding `size_of::<Mapping>()`
    /// here charged the same bytes twice, once inside the row and once beside it. The same went for the
    /// `History` header, which is a field of this struct; [History::footprint] now charges only the deque's
    /// heap backing for the same reason.
    ///
    /// What remains is the storage a mapping owns that is *not* inside its row: the reply filter's logical
    /// rows, the history's deque, and the worker's error-queue ancillary buffer - fixed and small, unlike the
    /// payloads, which belong to the reply queue's own reservation.
    fn footprint() -> Option<u64> {
        logical_footprint::<(IpAddr, Instant)>(REMOTES_PREPARED)?
            .checked_add(History::footprint(HISTORY_DEPTH)?)?
            .checked_add(egress::ErrorQueue::footprint())
    }

    /// The whole composite grant a first send needs: two records - the mapping and its first remote - and
    /// every byte of the state above, in one all-or-nothing request.
    fn first_send() -> Option<Request> {
        Some(Request {
            records: 2,
            record_class: Class::General,
            bytes: Self::footprint()?,
            byte_class: Class::General,
            ..Request::default()
        })
    }
}

/// Counters rather than a report per event: this path is driven by whoever puts packets on the interface,
/// so anything printed per packet is a flood by construction. Only once-per-mapping outcomes are printed.
#[derive(Default)]
struct Counters {
    sent: u64,
    written: u64,
    denied: u64,
    no_upstream: u64,
    expired: u64,
    too_big: u64,
    blocked: u64,
    unreachable: u64,
    send_failed: u64,
    reported: u64,
    unreported: u64,
    df_failed: u64,
    open_failed: u64,
    foreign_interface: u64,
    unpermitted: u64,
    translated: u64,
    untranslated: u64,
    implausible: u64,
    ambiguous: u64,
    unsent: u64,
    stale: u64,
    swept: u64,
    /// A send that left less than the whole datagram, which is a failure rather than a partial success.
    short: u64,
    /// Refused because a prepared collection was full: the mapping table, or one mapping's reply filter.
    /// Distinct from a denial, because what ran out is a capacity that was reserved rather than the aggregate.
    unprepared: u64,
}

impl Counters {
    fn describe(&self) -> String {
        format!(
            "sent {} written {} denied {} no-upstream {} expired {} too-big {} blocked {} \
             unreachable {} send-failed {} reported {} unreported {} df-failed {} open-failed {} \
             foreign-interface {} unpermitted {} translated {} untranslated {} implausible {} \
             ambiguous {} unsent {} stale {} swept {} short {} unprepared {}",
            self.sent,
            self.written,
            self.denied,
            self.no_upstream,
            self.expired,
            self.too_big,
            self.blocked,
            self.unreachable,
            self.send_failed,
            self.reported,
            self.unreported,
            self.df_failed,
            self.open_failed,
            self.foreign_interface,
            self.unpermitted,
            self.translated,
            self.untranslated,
            self.implausible,
            self.ambiguous,
            self.unsent,
            self.stale,
            self.swept,
            self.short,
            self.unprepared
        )
    }
}

pub(crate) struct Relay {
    stamp: Stamp,
    upstream: Option<Upstream>,
    mappings: Workers<SocketAddr, Mapping>,
    /// The mapping table's own retained capacity, the reply channel's whole allocation and the payloads its
    /// slots may carry, and the error-queue scratch below. Byte-only and permanent: all of it is prepared at
    /// session start and none of it shrinks, so this is charged once and released once.
    tables: Lease,
    /// The one error-queue scratch this owner lends to its send-failure path.
    ///
    /// Held rather than built per failure: a fresh one per failed send is a second heap ancillary buffer
    /// alive beside the receive worker's own, uncharged, and as frequent as a client chooses to make sends
    /// fail. Confined to the ingress owner, which handles one datagram at a time, so one is exact.
    errors: egress::ErrorQueue,
    events: mpsc::Sender<Event<SocketAddr>>,
    counters: Counters,
    reported_send_failure: bool,
}

/// Everything the first datagram to an unmapped source needs, grouped so the transaction below reads as one
/// step rather than as eight arguments.
struct FirstSend<'a> {
    upstream: Upstream,
    packet: &'a [u8],
    datagram: Relayed<'a>,
    hop_limit: u8,
    gateways: &'a Gateways,
    output: &'a mut Output,
}

impl Relay {
    /// Prepares the table and the reply queue, charging both before a single datagram can be admitted
    /// against them.
    pub(crate) fn new(
        admission: &mut Admission,
    ) -> Result<(Self, mpsc::Receiver<Event<SocketAddr>>), Denied> {
        // Every mapping the general ceiling could admit at once, so the table's logical maximum is the same
        // number the record total is: a mapping granted a record always has a row to go in, and a row is
        // never the thing that refuses one.
        let prepared = admission.general_record_ceiling() as usize;
        // Charged before any of it is built, this owner's whole fixed state in one reserve. A task blocked
        // waiting for a reply slot stops reading its socket, so the kernel's receive buffer absorbs the burst
        // and then drops, which is the correct backpressure for a datagram nobody promised to deliver.
        let bytes = Workers::<SocketAddr, Mapping>::footprint(prepared)
            .and_then(|table| table.checked_add(reply_channel_bytes::<SocketAddr>()?))
            .and_then(|bytes| bytes.checked_add(egress::ErrorQueue::footprint()))
            .ok_or(Denied::Arithmetic)?;
        let tables = admission.reserve(Request::bytes(bytes, Class::General))?;
        // Reserved above, allocated here, and in that order deliberately - see
        // [crate::shizuku::reply::reply_channel].
        let (events, receiver) = reply_channel::<SocketAddr>();
        Ok((
            Self {
                stamp: Stamp::default(),
                upstream: None,
                mappings: Workers::with_capacity("shizuku.udp_mapping", prepared),
                tables,
                errors: egress::ErrorQueue::new(),
                events,
                counters: Counters::default(),
                reported_send_failure: false,
            },
            receiver,
        ))
    }

    /// Releases the table's own capacity, after every mapping in it has been settled.
    /// Gives this owner's retained capacity back, once everything it covers is physically gone.
    ///
    /// `events` is the caller's half of the reply channel, and it has to come here rather than outlive this
    /// call: the lease below covers that channel's whole allocation *and* every payload its slots may hold, so
    /// releasing while the receiver still owned a queued datagram would be capacity given back for memory this
    /// process was still holding. Dropping it destroys whatever it had buffered - no drain, which could only
    /// wait on senders that are already gone.
    pub(crate) fn release(
        self,
        events: mpsc::Receiver<Event<SocketAddr>>,
        admission: &mut Admission,
    ) {
        drop(self.mappings);
        drop(self.events);
        drop(events);
        drop(self.errors);
        admission.release(self.tables);
    }

    /// Adopts a config. Either axis advancing retires the whole table: the epoch because every mapping is
    /// keyed by a TUN-visible tuple, and the generation because every mapping holds a socket bound to the
    /// network that changed. So there is one retirement here rather than two.
    ///
    /// Returns only once every retired mapping's receive task has been joined and its descriptor closed,
    /// which is what makes the caller's acknowledgement mean what the design says it means.
    pub(crate) async fn apply(
        &mut self,
        stamp: Stamp,
        upstream: Option<Upstream>,
        admission: &mut Admission,
    ) {
        let retiring = stamp != self.stamp;
        // Adopted before the sweep rather than after it, so that anything the sweep writes is stamped with the
        // retirement it belongs to: a connectionless mapping writes nothing terminal, but the same ordering is
        // what lets the terminating engine's resets past the writer's gate.
        self.stamp = stamp;
        self.upstream = upstream;
        if retiring {
            self.shutdown(admission).await;
        }
    }

    /// Cancels every mapping and joins every receive task, refunding each as it completes.
    ///
    /// Replies still in the channel are left there rather than drained: every mapping in the table is being
    /// retired, so every one of them belongs to retired state and the ordinary staleness check discards them
    /// when the caller next reads. A task parked on that channel wakes on its own token instead, which is why
    /// nothing here has to drain it to make progress.
    ///
    /// Also the whole-session path, because there is no weaker version of it worth having: the session ends
    /// with every descriptor this relay opened closed and every task it started joined.
    pub(crate) async fn shutdown(&mut self, admission: &mut Admission) {
        self.mappings.cancel_all();
        while self.mappings.working() {
            let terminal = self.mappings.finished().await;
            self.close(terminal, admission);
        }
    }

    /// Sends one client datagram upstream, opening the mapping it belongs to if this is the first.
    pub(crate) fn relay(
        &mut self,
        packet: &[u8],
        datagram: Relayed<'_>,
        gateways: &Gateways,
        output: &mut Output,
        admission: &mut Admission,
    ) {
        let Some(upstream) = self.upstream else {
            // no selectable network, or one whose interface the app could not name: an operation fails, the
            // session does not
            self.counters.no_upstream += 1;
            return;
        };
        // A missing hop limit cannot reach here from a parsed packet, and is refused alongside an expired
        // one rather than named separately: neither is forwardable.
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(datagram.hop_limit)) else {
            self.counters.expired += 1;
            // A router owes Time Exceeded here, and it is also what makes this daemon visible to a traceroute
            // instead of being a hole in one.
            return self.report(packet, Reason::Expired, gateways, output);
        };
        if !self.live(&datagram.source) {
            if self.mappings.contains(&datagram.source) {
                // A record exists but is not a mapping: its first send has not been decided, or it failed and
                // the record is a tombstone waiting for its worker. Neither may carry a datagram - the first
                // would interleave with a send that has not returned, the second would use a socket that is
                // being torn down - and neither may be replaced, because its worker still holds the
                // descriptor. Counted and dropped; the client retransmits.
                self.counters.unprepared += 1;
                return;
            }
            return self.open(
                FirstSend {
                    upstream,
                    packet,
                    datagram,
                    hop_limit,
                    gateways,
                    output,
                },
                admission,
            );
        }
        let Some(mapping) = self
            .mappings
            .get_mut(&datagram.source)
            .map(|held| &mut held.record)
        else {
            return;
        };
        // Reserved before the send, not after it, and that ordering is the whole point of an admission
        // ceiling: charging afterwards means the datagram has already left the device and the ceiling only
        // refused the bookkeeping - a permitted-remote set held at its bound while traffic keeps flowing to
        // remotes it does not name, which is a reply the mapping will then drop. Denial has to be able to
        // stop the send.
        //
        // A remote already in the set is already charged, so only a new one is a transaction here. Its bytes
        // are not: the filter was prepared and charged at the mapping's own admission, so a new remote costs
        // one record. What it can run out of is logical slots, and that is a denial too - a remote counted
        // after a successful send that then had nowhere to go would be the fallible commit this ordering
        // exists to make impossible.
        let unrecorded = match mapping.permits(datagram.destination.ip()) {
            Permit::Recorded => false,
            Permit::Free => true,
            // Refused before the send, so nothing goes out that this mapping could not record.
            Permit::Full => {
                self.counters.unprepared += 1;
                return;
            }
        };
        // A record for the new remote, before the send for the same reason the bound was: the aggregate is
        // what may refuse it, and a refusal has to happen while nothing has left.
        if unrecorded
            && admission
                .grow(&mapping.lease, Request::records(1, Class::General))
                .is_err()
        {
            self.counters.denied += 1;
            return;
        }
        let socket = mapping.socket.get_ref();
        if !datagram.destination.is_ipv6() {
            // Reapplied immediately before each send and never left to another task to interleave, because
            // this one socket carries datagrams whose DF bits differ. IPv6 has no such bit: no router may
            // fragment an IPv6 packet, so there is nothing per-packet to reapply.
            if let Err(e) = egress::set_fragmentation(
                socket,
                if datagram.dont_fragment {
                    Fragmentation::Prohibited
                } else {
                    Fragmentation::Permitted
                },
            ) {
                if unrecorded {
                    admission.shrink(&mapping.lease, Request::records(1, Class::General));
                }
                self.counters.df_failed += 1;
                // The daemon's own socket refusing the daemon's own option, which nothing a client sends can
                // cause: a counter alone leaves it indistinguishable from an ordinary undeliverable datagram
                // and gives nobody the errno. The datagram's fate is unchanged - sending it under the wrong
                // fragmentation policy is exactly what must not happen.
                report::io_with_details(
                    "shizuku.udp_fragmentation",
                    e,
                    [
                        ("destination", datagram.destination.to_string()),
                        ("dont_fragment", datagram.dont_fragment.to_string()),
                    ],
                );
                return;
            }
        }
        match egress::send_to(socket, datagram.destination, datagram.payload, hop_limit) {
            Ok(_) => {
                self.counters.sent += 1;
                // Outbound activity refreshes this mapping and this remote, and nothing else. Inbound
                // traffic, rejections, and ICMP errors never refresh anything.
                mapping.deadline = Instant::now() + MAPPING_TIMEOUT;
                // Recorded only on success, because an error can only be about a datagram that left. The
                // record's own deadline is absolute and this refresh does not touch it.
                mapping.history.record(
                    datagram.destination,
                    datagram.payload,
                    hop_limit,
                    HISTORY_DEPTH,
                    Instant::now(),
                );
                // Committed only now, so the reservation above and the record it paid for come into
                // existence together. [Mapping::permits] said yes before the send, so this cannot be a
                // destination with nowhere to go.
                mapping.record_remote(datagram.destination.ip());
            }
            Err(e) => {
                // The semantic record is given back, because the remote it was for was never permitted -
                // keeping it would leak one unit of the aggregate per refused datagram, which a client can
                // drive at will. The filter's own row-state charge is *not* given back: it covers the
                // mapping's whole bound rather than the remotes in it.
                if unrecorded {
                    admission.shrink(&mapping.lease, Request::records(1, Class::General));
                }
                self.fail(e, packet, datagram, gateways, output);
            }
        }
    }

    /// The classification of one failed send. Split out only so that the reservation above has exactly one
    /// rollback point rather than one per arm.
    fn fail(
        &mut self,
        e: io::Error,
        packet: &[u8],
        datagram: Relayed<'_>,
        gateways: &Gateways,
        output: &mut Output,
    ) {
        let Some(mapping) = self.mappings.get(&datagram.source) else {
            return;
        };
        // Both taken by value, so nothing below borrows the table: the arms need the counters and the error
        // path, and the extra share of the socket is gone again before anything could be joined.
        let cancel = mapping.cancel.clone();
        let socket = Arc::clone(&mapping.record.socket);
        match send_failure::classify(&e) {
            Failure::Blocked => self.counters.blocked += 1,
            Failure::TooBig => {
                self.counters.too_big += 1;
                match egress::drain_local_error(socket.get_ref(), &mut self.errors) {
                    Ok(Some(queued)) if queued.errno == libc::EMSGSIZE => {
                        self.report(
                            packet,
                            Reason::TooBig { mtu: queued.info },
                            gateways,
                            output,
                        );
                    }
                    // No local refusal in the queue, or one about something else. Errors a router sent are
                    // drained past rather than used, because each is about whichever destination *its*
                    // packet was aimed at and this socket serves many. Without an MTU attributable to this
                    // destination there is nothing truthful to say, and a wrong one is cached for minutes.
                    Ok(_) => self.counters.unreported += 1,
                    Err(e) => {
                        self.counters.unreported += 1;
                        report::io_with_details(
                            "shizuku.udp_path_mtu",
                            e,
                            [
                                ("principal", datagram.source.ip().to_string()),
                                ("destination", datagram.destination.to_string()),
                            ],
                        );
                    }
                }
            }
            // Cancelled rather than removed: the refund belongs to the receive task finishing, which is what
            // says the descriptor is actually gone.
            Failure::NetworkGone => {
                cancel.cancel();
                self.counters.unreachable += 1;
            }
            Failure::Unreachable => self.counters.unreachable += 1,
            Failure::Unexpected => {
                if !self.reported_send_failure {
                    self.reported_send_failure = true;
                    report::io_with_details(
                        "shizuku.udp_send",
                        e,
                        [
                            ("destination", datagram.destination.to_string()),
                            ("note", "later failures are counted only".to_owned()),
                        ],
                    );
                }
                self.counters.send_failed += 1;
            }
        }
    }

    /// Whether this key names a mapping that has actually carried a datagram. A provisional or rolled-back
    /// record is not one, and nothing may reach for it as though it were.
    fn live(&self, key: &SocketAddr) -> bool {
        self.mappings
            .get(key)
            .is_some_and(|held| held.record.state == State::Live)
    }

    /// Tells the client why its packet did not go, from the interface's own address as a router would.
    fn report(&mut self, packet: &[u8], reason: Reason, gateways: &Gateways, output: &mut Output) {
        match gateways.report(packet, reason) {
            Some(error) => {
                self.counters.reported += 1;
                output.packet(self.stamp, error);
            }
            None => self.counters.unreported += 1,
        }
    }

    /// The first datagram to a source with no mapping: one transaction, from admission to published mapping.
    ///
    /// Everything the mapping will ever need exists before the datagram leaves - both records and every byte
    /// in one grant, the socket, the prepared reply filter and history, the identity, and the receive worker,
    /// *retained and blocked on a gate it has not been let through*. The commit point is the send. After it
    /// there is nothing fallible: the record is already in the table, every bound it will fill is already
    /// charged, and publishing is three field writes and a signal.
    ///
    /// Spawning the worker after a successful send would be the failure this shape removes - a fallible step
    /// after the commit point, whose failure leaves a datagram on the wire and no owner for the reply. Leaving
    /// it unspawned and racing is the same hole with a different name. Retained-but-gated is neither: the
    /// owner already holds the task, so whichever way the send goes it is settled by the one join fence.
    ///
    /// Rollback is that same fence. A precommit failure cancels the worker and marks the record
    /// [State::RolledBack]; it is not a mapping from that moment - no reply matches it, no remote is
    /// permitted, it has no deadline, and a second datagram from the same source is refused rather than sent
    /// on a socket that is being torn down. Its terminal arrives at [Relay::close] like any other, which joins
    /// the task, drops the record and its descriptor, and only then releases the grant.
    fn open(&mut self, first: FirstSend<'_>, admission: &mut Admission) {
        let FirstSend {
            upstream,
            packet,
            datagram,
            hop_limit,
            gateways,
            output,
        } = first;
        let key = datagram.source;
        // The table has to have a free slot, and no live record under this key, before anything is charged
        // for it: a mapping past the logical maximum is one the charge does not cover, and a duplicate would
        // displace a record whose worker still holds its descriptor. What the table's own backing does is not
        // part of that - it is opaque count-bounded overhead - so the bound is the whole question here.
        if self.mappings.admits(&key).is_err() {
            self.counters.unprepared += 1;
            return;
        }
        let Some(request) = Mapping::first_send() else {
            self.counters.denied += 1;
            return;
        };
        // Two records at once, not one and then another: a mapping whose first remote could not be admitted
        // is a socket that may send and a reply that will be dropped.
        let Ok(lease) = admission.reserve(request) else {
            self.counters.denied += 1;
            return;
        };
        // Only now, with the admission granted, is a descriptor opened.
        let socket = match self.bind(upstream.network, datagram.destination.is_ipv6()) {
            Ok(socket) => Arc::new(socket),
            Err(e) => {
                // Includes the kernel refusing an identity because the ephemeral range is exhausted, which
                // is the identity ceiling enforcing itself: the daemon cannot read that range, so it handles
                // the refusal rather than predicting it. Printed because it is once per mapping, not per
                // packet, and because it is the shape a resource ceiling arrives in.
                report::io_with_details("shizuku.udp_open", e, [("source", key)]);
                admission.release(lease);
                self.counters.open_failed += 1;
                return;
            }
        };
        // The one oneshot this owner has, and it is the mapping's named commit gate: the worker parks on it
        // until the first send has either committed the mapping or unwound it. One per mapping record, taken
        // after the grant and the descriptor above, and gone with the record - which is the whole of what the
        // aggregate policy bounds for it, since a oneshot's shared cell is count-bounded rather than
        // byte-charged.
        let (commit, gate) = oneshot::channel();
        // Before the worker exists and before anything is published. The socket and the grant are already in
        // hand, so a refusal here unwinds them exactly as any other precommit failure does.
        let Ok(identity) = self.mappings.identity() else {
            // Both halves of what this candidate physically holds - the descriptor and the gate's shared
            // cell - go before the grant that covered them, the same order [Relay::discard] and
            // [Relay::close] use. Left to scope exit the cell would outlive its own accounting.
            drop((socket, commit, gate));
            self.counters.denied += 1;
            return admission.release(lease);
        };
        let worker = receive(
            Arc::clone(&socket),
            key,
            identity.id,
            Sizing::Peek,
            Gate::Pending(gate),
            self.events.clone(),
            identity.cancel.clone(),
        );
        // Registered before the send, with every collection it will ever need already built at its bound. The
        // deadline is set here and means nothing until the state is Live - see [Relay::next_deadline].
        let provisional = Mapping {
            state: State::Provisional,
            commit: Some(commit),
            socket: Arc::clone(&socket),
            // Requested at its logical maximum, the number [Mapping::footprint] charged rows for, so the
            // common case allocates nothing. The bound is what a new remote is checked against before the
            // send - see [Relay::relay].
            remotes: HashMap::with_capacity(REMOTES_PREPARED),
            deadline: Instant::now() + MAPPING_TIMEOUT,
            lease,
            history: History::with_capacity(HISTORY_DEPTH),
        };
        if let Err((provisional, _)) = self.mappings.admit(key, &identity, provisional, worker) {
            // Unreachable: capacity and duplication were both checked above and this is the only admitter.
            // Unwound anyway, because the alternative to unwinding is a descriptor nothing closes.
            drop(socket);
            self.discard(provisional, admission);
            self.counters.unprepared += 1;
            return;
        }
        // From here every exit is through the rollback, which is the ordinary terminal fence.
        if !datagram.destination.is_ipv6() {
            if let Err(e) = egress::set_fragmentation(
                socket.get_ref(),
                if datagram.dont_fragment {
                    Fragmentation::Prohibited
                } else {
                    Fragmentation::Permitted
                },
            ) {
                self.counters.df_failed += 1;
                // Reported for the same reason as the established path above, and with it so that which of
                // the two ran does not decide whether the failure is visible.
                report::io_with_details(
                    "shizuku.udp_fragmentation",
                    e,
                    [
                        ("destination", datagram.destination.to_string()),
                        ("dont_fragment", datagram.dont_fragment.to_string()),
                    ],
                );
                return self.roll_back(key);
            }
        }
        let sent = match egress::send_to(
            socket.get_ref(),
            datagram.destination,
            datagram.payload,
            hop_limit,
        ) {
            Ok(sent) => sent,
            Err(e) => {
                // Read while the socket is still open, because the error queue is the only place the kernel's
                // own refusal - and with it the path MTU - can be found, and the rollback closes it.
                let queued = match send_failure::classify(&e) {
                    Failure::TooBig => {
                        match egress::drain_local_error(socket.get_ref(), &mut self.errors) {
                            Ok(queued) => queued,
                            Err(drain) => {
                                // The established path reports this and this one discarded it, so the same
                                // kernel failure was visible or invisible depending only on whether the
                                // mapping already existed. What the client is told is unchanged: with no
                                // attributable path MTU there is nothing truthful to send either way.
                                report::io_with_details(
                                    "shizuku.udp_path_mtu",
                                    drain,
                                    [
                                        ("principal", datagram.source.ip().to_string()),
                                        ("destination", datagram.destination.to_string()),
                                    ],
                                );
                                None
                            }
                        }
                    }
                    _ => None,
                };
                self.roll_back(key);
                return self.fail_unmapped(e, packet, datagram, queued, gateways, output);
            }
        };
        // A short send is a failure. The client's datagram did not leave whole, and publishing a mapping for
        // it would claim an exchange that never started. A zero-length payload sending zero bytes is not
        // short - it is the whole of what was asked for - which is why this compares against the payload
        // rather than against zero.
        if sent != datagram.payload.len() {
            self.counters.short += 1;
            return self.roll_back(key);
        }
        self.counters.sent += 1;
        // The commit. Nothing below is fallible: the record is in the table, both collections are built at
        // their bounds and charged for them, and the gate's receiver is held by a task this owner already
        // retained.
        let Some(mapping) = self.mappings.get_mut(&key).map(|held| &mut held.record) else {
            // Unreachable: nothing removes a record between the admission above and here.
            self.counters.stale += 1;
            return;
        };
        mapping.deadline = Instant::now() + MAPPING_TIMEOUT;
        mapping.history.record(
            datagram.destination,
            datagram.payload,
            hop_limit,
            HISTORY_DEPTH,
            mapping.deadline - MAPPING_TIMEOUT,
        );
        mapping
            .remotes
            .insert(datagram.destination.ip(), mapping.deadline);
        mapping.state = State::Live;
        if let Some(commit) = mapping.commit.take() {
            // A closed receiver means the worker already ended on its own, which its terminal settles; there
            // is nothing to undo and nothing to report.
            let _ = commit.send(());
        }
    }

    /// Turns a provisional mapping into a tombstone: it stops being a mapping at once, and its worker is
    /// asked to stop.
    ///
    /// Dropping the gate rather than signalling it is what the retained worker sees as a rollback - it has
    /// read nothing and allocated nothing - and cancelling covers the case where it was already past the gate.
    /// The join, the drop and the release all follow through [Relay::close], which is the one fence that can
    /// honestly say the descriptor is gone.
    fn roll_back(&mut self, key: SocketAddr) {
        if let Some(held) = self.mappings.get_mut(&key) {
            held.record.state = State::RolledBack;
            held.record.commit = None;
        }
        self.mappings.cancel(&key);
    }

    /// Unwinds a mapping that was never published: the state and both shares of the descriptor go first, and
    /// only then is the one composite lease released.
    ///
    /// That order is the whole of it. Releasing while the socket is still open would hand the descriptor's
    /// capacity to whatever asks next while the descriptor is still this process's.
    fn discard(&mut self, provisional: Mapping, admission: &mut Admission) {
        let Mapping {
            socket,
            remotes,
            lease,
            history,
            ..
        } = provisional;
        drop(socket);
        drop(remotes);
        drop(history);
        admission.release(lease);
    }

    /// The classification of a failed *first* send, which has no mapping to reach for.
    ///
    /// Split from [Relay::fail] rather than shared with it: that one reads the table to find the socket whose
    /// error queue to drain and the token to cancel, and neither exists here - the mapping was discarded
    /// before this ran, which is what keeps a failed first send from stranding a descriptor for five minutes.
    fn fail_unmapped(
        &mut self,
        e: io::Error,
        packet: &[u8],
        datagram: Relayed<'_>,
        queued: Option<egress::QueuedError>,
        gateways: &Gateways,
        output: &mut Output,
    ) {
        match send_failure::classify(&e) {
            Failure::Blocked => self.counters.blocked += 1,
            Failure::TooBig => {
                self.counters.too_big += 1;
                match queued {
                    // Taken from the error queue above, while the socket was still open.
                    Some(queued) if queued.errno == libc::EMSGSIZE => self.report(
                        packet,
                        Reason::TooBig { mtu: queued.info },
                        gateways,
                        output,
                    ),
                    // Nothing attributable, so nothing is said: a guessed MTU is cached by the client for
                    // minutes and is worse than silence.
                    _ => self.counters.unreported += 1,
                }
            }
            Failure::NetworkGone | Failure::Unreachable => self.counters.unreachable += 1,
            Failure::Unexpected => {
                if !self.reported_send_failure {
                    self.reported_send_failure = true;
                    report::io_with_details(
                        "shizuku.udp_send",
                        e,
                        [
                            ("destination", datagram.destination.to_string()),
                            ("note", "later failures are counted only".to_owned()),
                        ],
                    );
                }
                self.counters.send_failed += 1;
            }
        }
    }

    /// One unconnected socket with its local identity bound up front, so the mapping pins a port for its
    /// whole life instead of acquiring one on first send - which is what makes its budget charge real
    /// rather than anticipated.
    fn bind(&self, network: Network, ipv6: bool) -> io::Result<AsyncFd<Socket>> {
        let socket = egress::open_udp(network, ipv6)?;
        socket.bind(&SockAddr::from(SocketAddr::new(
            if ipv6 {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            } else {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            },
            0,
        )))?;
        // Registered for errors as well as readability, because a queued ICMP error raises only EPOLLERR and
        // the reply task would otherwise never wake for one.
        AsyncFd::with_interest(socket, ERROR_OR_READABLE)
    }

    /// Repeats one ICMP error a router sent. The decision and the packet belong to
    /// [vpnhotspotd::shared::icmp_translate]; what this contributes is the one fact only the mapping can
    /// answer - whether it ever sent to that destination - and the address filter is exactly that answer.
    fn translate(&mut self, key: SocketAddr, error: &Reported, output: &mut Output) {
        let Some(mapping) = self
            .mappings
            .get(&key)
            .filter(|held| held.record.state == State::Live)
        else {
            self.counters.stale += 1;
            return;
        };
        // A UDP error always names its destination; one that does not is not about this relay's traffic.
        let Some(destination) = error.destination else {
            self.counters.implausible += 1;
            return;
        };
        if !mapping.record.remotes.contains_key(&destination.ip()) {
            self.counters.unpermitted += 1;
            return;
        }
        // Address proof first, because it is what the permitted-remote set already establishes and it is all
        // the route-level errors need. Only a claim about one datagram falls through to the history, and only
        // then does asking it cost this mapping its one correlated answer.
        let refused = match icmp_translate::repeat(key, error, Correlation::Address) {
            Ok(packet) => {
                self.counters.translated += 1;
                output.packet(self.stamp, packet);
                return;
            }
            Err(refused) => refused,
        };
        if refused != Untranslatable::Uncorrelated {
            match refused {
                Untranslatable::Implausible => self.counters.implausible += 1,
                _ => self.counters.untranslated += 1,
            }
            return;
        }
        let Some(mapping) = self.mappings.get_mut(&key) else {
            self.counters.stale += 1;
            return;
        };
        let (resolution, _) =
            mapping
                .record
                .history
                .resolve(destination, error.quoted.as_slice(), Instant::now());
        let hop_limit = match resolution {
            Resolution::Matched { hop_limit } => hop_limit,
            // Each of these is its own counter because they say different things: an unmatched error is one
            // about a datagram nobody sent, an ambiguous one is a client that repeated itself, and a spent one
            // is this mapping having already used its single answer or having forgotten.
            Resolution::Ambiguous => {
                self.counters.ambiguous += 1;
                return;
            }
            Resolution::Untracked => {
                self.counters.unsent += 1;
                return;
            }
            Resolution::Spent => {
                self.counters.untranslated += 1;
                return;
            }
        };
        match icmp_translate::repeat(key, error, Correlation::Datagram { hop_limit }) {
            Ok(packet) => {
                self.counters.translated += 1;
                output.packet(self.stamp, packet);
            }
            Err(Untranslatable::Implausible) => self.counters.implausible += 1,
            Err(_) => self.counters.untranslated += 1,
        }
    }

    pub(crate) fn handle(&mut self, event: Event<SocketAddr>, output: &mut Output) {
        let (key, id, remote, hop_limit, interface, payload) = match event {
            Event::Error { key, id, error } => {
                if !self.mappings.current(&key, id) {
                    self.counters.stale += 1;
                    return;
                }
                self.translate(key, &error, output);
                return;
            }
            Event::Reply {
                key,
                id,
                remote,
                hop_limit,
                interface,
                payload,
            } => (key, id, remote, hop_limit, interface, payload),
        };
        let Some(upstream) = self.upstream else {
            self.counters.stale += 1;
            return;
        };
        match self.mappings.get(&key) {
            Some(mapping) if mapping.id == id && mapping.record.state == State::Live => {
                // Inbound UDP demultiplexes on local address and port alone, and the mark that steered the
                // send takes no part in it, so a late reply to a retired mapping can be delivered to
                // whatever socket now holds that identity. This is the only thing separating the two.
                if interface != upstream.interface {
                    self.counters.foreign_interface += 1;
                    return;
                }
                if !mapping.record.remotes.contains_key(&remote.ip()) {
                    self.counters.unpermitted += 1;
                    return;
                }
            }
            _ => {
                self.counters.stale += 1;
                return;
            }
        }
        // Relayed traffic preserves what arrived rather than substituting a local default, and this daemon
        // is one hop, so a reply whose remaining hop limit dies here dies here.
        let Nat66HopLimit::Forward(hop_limit) = nat66_hop_limit(Some(hop_limit)) else {
            self.counters.expired += 1;
            return;
        };
        output.datagram(self.stamp, remote, key, hop_limit, &payload);
        self.counters.written += 1;
    }

    /// Settles one mapping whose receive task has finished, whether it failed on its own or was retired.
    ///
    /// The order is the fence. The task is complete before this runs, so it has already dropped its share of
    /// the socket; taking the mapping out drops the other share and closes the descriptor; only then is the
    /// budget told, and only after every one of these has run may the session acknowledge the config that
    /// caused it.
    pub(crate) fn close(&mut self, terminal: Terminal<SocketAddr>, admission: &mut Admission) {
        let Terminal { key, id, ended } = terminal;
        match ended {
            Ended::Expected => {}
            // once per mapping, not per packet, so this cannot flood
            Ended::Reported(reason) => report::stdout!("udp mapping {key} closed: {reason}"),
            Ended::Failed { context, error } => {
                report::io_with_details(context, error, [("mapping", key)])
            }
        }
        match self.mappings.retire(&key, id) {
            Some(mapping) => {
                // The lease is taken out of the record and released only after everything the record owned
                // has been dropped: the task is already complete, so dropping this closes the descriptor and
                // frees both prepared collections. The table's own charge stays - it covers [Relay::tables]'
                // own bound and is not this mapping's to give back.
                let Mapping {
                    socket,
                    remotes,
                    lease,
                    history,
                    commit,
                    ..
                } = mapping;
                drop(commit);
                drop(socket);
                drop(remotes);
                drop(history);
                admission.release(lease);
            }
            // a terminal for a mapping whose key has already been reused, which its successor must survive
            None => self.counters.stale += 1,
        }
    }

    /// The next mapping to have finished, which is the only thing that retires one. Selected on by the owning
    /// task, so it waits forever while the table is empty rather than answering at once.
    pub(crate) async fn finished(&mut self) -> Terminal<SocketAddr> {
        self.mappings.finished().await
    }

    /// Retires what has timed out. Expiring a remote record only narrows the reply filter; expiring the
    /// mapping cancels its task, and the refund follows that task finishing rather than happening here.
    pub(crate) fn sweep(&mut self, admission: &mut Admission) {
        let now = Instant::now();
        for mapping in self.mappings.values_mut() {
            // Already on its way out and waiting only for its task to finish. Skipping it is what stops this
            // from being called again immediately: its deadline is in the past and would otherwise still be
            // the earliest one in the table.
            if mapping.cancel.is_cancelled() || mapping.record.state != State::Live {
                continue;
            }
            let expired = mapping.record.expire_remotes(now);
            if expired > 0 {
                // The semantic records go and their logical slots are free again; the byte charge does not
                // move, because it was taken for this filter's whole bound rather than for the rows in it.
                admission.shrink(
                    &mapping.record.lease,
                    Request::records(expired as u32, Class::General),
                );
                self.counters.swept += expired as u64;
            }
            if mapping.record.deadline <= now {
                mapping.cancel.cancel();
            }
        }
    }

    /// The earliest deadline in the table, which is what the owning task sleeps until. None means there is
    /// nothing to expire, not that expiry is off.
    ///
    /// A cancelled mapping is excluded, and that is load-bearing rather than tidy. Cancelling does not
    /// remove it - its task finishing does, so that the refund lands when the descriptor actually closes - so
    /// its expired deadline would stay the earliest one in the table and wake this task in a tight loop
    /// until its receive task got scheduled.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        self.mappings
            .values()
            .filter(|mapping| !mapping.cancel.is_cancelled() && mapping.record.state == State::Live)
            .flat_map(|mapping| {
                std::iter::once(mapping.record.deadline)
                    .chain(mapping.record.remotes.values().copied())
            })
            .min()
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} mappings, {}",
            self.mappings.len(),
            self.counters.describe()
        )
    }
}
