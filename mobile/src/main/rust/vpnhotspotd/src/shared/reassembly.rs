//! Ingress reassembly: the bounded contexts that turn a client's fragments back into one datagram.
//!
//! Needed because the relays parse whole datagrams and nothing else. A client that fragments - a large ping,
//! a UDP datagram past the downstream MTU - would otherwise have every fragment counted and dropped, which
//! is silent and total loss for that traffic rather than degraded service.
//!
//! Output is a datagram with the fragmentation *removed*: the IPv4 flags and offset cleared and the header
//! checksum recomputed, or the IPv6 Fragment header spliced out and its next header promoted. That is what
//! lets the existing strict parses run on the result unchanged, rather than each learning a second shape.
//!
//! Overlapping fragments discard the whole datagram rather than being merged. RFC 5722 requires exactly that
//! for IPv6, and the reason generalises: an overlap is either a broken sender or a deliberate attempt to make
//! two readers assemble different bytes from one exchange, and there is no third case worth serving.
//!
//! The header kept is fragment zero's own, so the reassembled datagram carries the options and hop limit the
//! client actually sent instead of a reconstruction. A context that never receives fragment zero can still
//! complete its byte range, but has no header to speak from and is dropped.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
#[cfg(test)]
use std::net::Ipv6Addr;
use std::ops::Range;
use std::time::{Duration, Instant};

use crate::shared::admission::{logical_footprint, Admission, Class, Lease, Request};

use etherparse::{IpNumber, Ipv4Header, Ipv6FragmentHeaderSlice};

use crate::shared::ip_wire::Packet;
#[cfg(test)]
use crate::shared::packet_writer::{IPV4_HEADER_LEN, IPV6_FRAGMENT_HEADER_LEN, IPV6_HEADER_LEN};

/// RFC 8200 section 4.5 requires 60 seconds for IPv6, and RFC 791 allows up to it for IPv4, so one number
/// serves both. Linux itself uses 30 (`IP_FRAG_TIME`); the longer bound is taken because this sits behind a
/// downstream link whose retransmissions the daemon does not see.
///
/// https://www.rfc-editor.org/rfc/rfc8200#section-4.5
const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest a datagram of either family can be, which bounds every offset this accepts.
const MAX_DATAGRAM: usize = u16::MAX as usize;

/// The largest header a context can be asked to keep: an IPv4 header with every option, which is bigger than
/// the fixed IPv6 one this also covers. Charged up front rather than when fragment zero arrives, so a sender
/// cannot open contexts cheaply and make them expensive afterwards.
const MAX_HEADER: usize = 60;

/// What accepting one fragment did.
#[derive(Debug, PartialEq, Eq)]
pub enum Accepted {
    /// Held. Nothing to dispatch until the rest arrives.
    Pending,
    /// The whole datagram, with its fragmentation removed so an ordinary parse sees it.
    Complete(Vec<u8>),
}

/// Why a fragment was not held. Each is counted rather than logged, since the input is chosen by whoever puts
/// packets on the interface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reject {
    /// Truncated, self-inconsistent, or claiming a range no datagram could hold.
    Malformed(&'static str),
    /// Overlaps or contradicts what is already held, so the whole datagram is discarded.
    Overlap,
    /// The nested fragment ceiling is full. Payload for already-held contexts still flows.
    Denied,
}

/// One fragment, as both families reduce to.
struct Fragment<'a> {
    key: Key,
    offset: usize,
    /// Whether more fragments follow, which is what makes the total length knowable.
    more: bool,
    /// Fragment zero's header, kept only from the fragment that has one. Inline, so parsing allocates
    /// nothing - see [Header].
    header: Option<Header>,
    payload: &'a [u8],
}

/// What a receiver reassembles on.
///
/// IPv4 includes the protocol, per RFC 791; IPv6 does not, because its Fragment header carries the next
/// header and RFC 8200 keys on source, destination and Identification alone. So the field is zero there
/// rather than holding a value the specification says not to key on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    source: IpAddr,
    destination: IpAddr,
    /// IPv4's is 16 bits and IPv6's 32, widened to the one type that holds both.
    identification: u32,
    protocol: u8,
}

struct Context {
    /// Grown to the highest offset seen rather than pre-sized to the maximum, because one fragment at a high
    /// offset would otherwise reserve a whole datagram's worth per identification.
    payload: Vec<u8>,
    /// Sorted, non-overlapping, and merged where adjacent, so completion is "one range covering the whole
    /// declared length" rather than a search.
    received: Vec<Range<usize>>,
    /// Fragment zero's own header, absent until it arrives.
    ///
    /// Inline rather than a `Vec`, and that is an ordering property rather than a size one: a heap-copied
    /// header is an allocation, and it was being made by the parser - before admission had granted anything.
    /// A sender could therefore drive one copy per fragment past a table that was about to refuse it. Inline,
    /// the header is part of [Context] and so is inside the logical row [Table::footprint] already charged for
    /// it, so fragment zero costs nothing new at all.
    header: Option<Header>,
    /// Known once the fragment with no successor arrives.
    total: Option<usize>,
    /// Absolute and never refreshed by later fragments: a sender that keeps sending fragments must not be
    /// able to hold a context open forever.
    deadline: Instant,
    /// Bytes charged for what this context *holds*: its whole [Context::footprint] rather than the fragment
    /// sizes that produced it, because the two differ exactly in the cases worth bounding - one fragment at a
    /// high offset and a sparse pattern that splits the range list. The row the context sits in is not here;
    /// [Table::footprint] charged that for every row the table was prepared for.
    charged: u64,
}

impl Context {
    /// Whether every byte from zero to the declared length is present, which - because the ranges are merged
    /// as they arrive - is exactly "one range, covering the whole thing".
    fn complete(&self) -> bool {
        match (self.total, self.received.as_slice()) {
            (Some(total), [only]) => *only == (0..total),
            _ => false,
        }
    }

    /// Everything this context actually *allocates*, not just the payload span it covers.
    ///
    /// The span alone is the part a sender cannot exceed; the rest is the part it can *choose*. A sparse
    /// pattern - one eight-octet fragment at every other offset - covers a small span while growing the
    /// received-range vector to thousands of entries, and buffer capacity outruns length because `Vec` grows
    /// geometrically. Charging the footprint rather than the span is what makes that pattern cost what it
    /// costs, which the Resource Policy requires of anything traffic can drive.
    ///
    /// Only the two buffers, though. The `Context` struct and the key beside it are one logical row of the
    /// table, and [Table::footprint] charged a row for every context the table is prepared for at the moment
    /// it was built - so counting `size_of::<(Key, Context)>()` here as well charged the same bytes twice, once
    /// in the fixed lease and again in every live context's growth. What is left is exactly the storage a
    /// context owns that is not inside its row.
    fn footprint(&self) -> usize {
        self.payload.capacity() + self.received.capacity() * std::mem::size_of::<Range<usize>>()
    }

    /// What a fragment reaching `end` costs, split into what survives it and what only exists during it.
    ///
    /// The split is the correction. Growing a context is not a step from one size to another: for a moment
    /// both the old buffer and its replacement are alive, because the bytes have to be copied from one to the
    /// other, and the range list is rebuilt beside itself for the same reason. A projection that named only
    /// the larger of the two would be describing a moment that never happened - and the moment it skipped is
    /// exactly the one where memory is highest.
    ///
    /// `completing` adds the other transient: the packet [assemble] builds exists beside the payload it was
    /// built from.
    ///
    /// Every replacement below is allocated at *exact* capacity rather than by `Vec`'s geometric growth, so
    /// the projection is what is really allocated rather than a guess that an allocator may round past.
    /// `None` when the arithmetic would wrap.
    fn project(&self, end: usize, completing: bool) -> Option<Projection> {
        let range = std::mem::size_of::<Range<usize>>() as u64;
        let payload = self.payload.capacity();
        // Exact: [insert] builds the replacement with `with_capacity(end)` when it has to grow at all.
        let grows = end > payload;
        let next_payload = if grows { end } else { payload } as u64;
        // [insert] rebuilds the range list into an exactly sized vector each time, so its capacity after this
        // fragment is at most one entry more than there are now - fewer if this one merges with a neighbour.
        let next_ranges = (self.received.len() as u64)
            .checked_add(1)?
            .checked_mul(range)?;
        let retained = next_payload.checked_add(next_ranges)?;
        // What is alive *in addition*, at the peak: the old payload while its replacement is filled, the old
        // range list while the merged one is built beside it, and - on the fragment that completes the
        // datagram - the assembled packet beside the payload it was assembled from.
        let mut peak = (self.received.capacity() as u64).checked_mul(range)?;
        if grows {
            peak = peak.checked_add(payload as u64)?;
        }
        if completing {
            peak = peak
                .checked_add(MAX_HEADER as u64)?
                .checked_add(next_payload)?;
        }
        Some(Projection { retained, peak })
    }
}

/// What one fragment costs: what the context keeps, and what exists only while it is being taken in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Projection {
    /// What the context owns once the fragment is in and the buffers it replaced are gone.
    retained: u64,
    /// What is alive *in addition to* [Projection::retained] at the moment both an old buffer and its
    /// replacement exist - and, on a completing fragment, the assembled packet beside its source.
    peak: u64,
}

/// Fragment zero's header, inline and copyable.
///
/// [MAX_HEADER] bytes because an IPv4 header with every option is the largest either family can present, and
/// IPv6's fixed forty fits inside it with room over. Storing it by value is what lets the parser hand a header
/// over without allocating: a borrow would tie the fragment to the packet buffer the caller is about to reuse,
/// and a `Vec` would be the allocation this exists to remove.
#[derive(Clone, Copy)]
struct Header {
    bytes: [u8; MAX_HEADER],
    length: u8,
}

impl Header {
    /// `None` for anything longer than a header can be, which is a packet this cannot describe rather than
    /// one to truncate.
    fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_HEADER {
            return None;
        }
        let mut kept = [0u8; MAX_HEADER];
        kept[..bytes.len()].copy_from_slice(bytes);
        Some(Header {
            bytes: kept,
            // The check above is what makes this fit; MAX_HEADER is well inside a u8.
            length: bytes.len() as u8,
        })
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.length as usize]
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Header({} bytes)", self.length)
    }
}

#[derive(Default)]
struct Counters {
    held: u64,
    completed: u64,
    malformed: u64,
    overlapping: u64,
    denied: u64,
    expired: u64,
    headless: u64,
    /// A reconciliation that found more allocated than was reserved, which the projection is supposed to make
    /// impossible. Counted rather than saturated away: the reservation is kept, so the aggregate still
    /// describes real memory, and a nonzero value here says the projection is wrong.
    undercharged: u64,
}

pub struct Table {
    contexts: HashMap<Key, Context>,
    /// Bytes this table currently owes its aggregate lease. Not a ceiling of its own: there is no second pool
    /// here any more, only a record of how much of the one aggregate this owner is holding, so that a shrink
    /// gives back exactly what a context really cost.
    charged: u64,
    /// The logical maximum: how many contexts may be held at once. Nothing is admitted past it, because a
    /// context the aggregate did not charge for is the fail-open case admission exists to prevent.
    prepared: usize,
    peak: u64,
    counters: Counters,
}

impl Table {
    /// Prepares for `contexts` incomplete datagrams at once.
    ///
    /// The caller charges [Table::footprint] for the same number once and keeps it charged: that charge is
    /// for the bound rather than for the contexts in it, so it outlives whatever completes or expires.
    pub fn with_capacity(contexts: usize) -> Self {
        Self {
            // Requested at its logical maximum, the same number [Table::footprint] charged rows for, so the
            // common case allocates nothing. What the map does with its backing from there is its own
            // business and is not accounted state.
            contexts: HashMap::with_capacity(contexts),
            charged: 0,
            prepared: contexts,
            peak: 0,
            counters: Counters::default(),
        }
    }

    /// What the table's own rows cost at that capacity, before any context is held in one.
    ///
    /// The rows and this struct; whatever the map keeps around them is count-bounded rather than charged -
    /// see [logical_footprint]. This is the *only* charge for a `(Key, Context)` row: what a live context adds
    /// on top is the two buffers it allocates, charged separately and exactly, as fragments arrive, against
    /// the nested fragment cap - see [Context::footprint].
    pub fn footprint(contexts: usize) -> Option<u64> {
        logical_footprint::<(Key, Context)>(contexts)?
            .checked_add(std::mem::size_of::<Self>() as u64)
    }

    /// Takes one fragment, against the one aggregate rather than against a pool of its own.
    ///
    /// `lease` is this table's compatibility owner: a single grant covering every context it holds, grown
    /// before an allocation and shrunk after one is really gone. Every growth is preflighted from
    /// [Context::projected], which bounds the real cost from over, and reconciled *downward* to what was
    /// actually allocated - never upward, because an upward reconciliation is an allocation that already
    /// happened being asked for permission afterwards.
    ///
    /// `now` is passed rather than read so the caller's sweep and this share one clock.
    pub fn accept(
        &mut self,
        packet: &[u8],
        now: Instant,
        admission: &mut Admission,
        lease: &Lease,
    ) -> Result<Accepted, Reject> {
        let fragment = match parse(packet) {
            Ok(fragment) => fragment,
            Err(e) => {
                self.counters.malformed += 1;
                return Err(e);
            }
        };
        let end = fragment.offset + fragment.payload.len();
        if end > MAX_DATAGRAM {
            self.counters.malformed += 1;
            return Err(Reject::Malformed("fragment reaches past a datagram"));
        }
        // The last fragment declares the total, so an earlier one reaching past it is a contradiction rather
        // than a fragment to hold.
        let total = (!fragment.more).then_some(end);
        // Checked before a vacant entry is created, because a new context is a row the aggregate was told
        // about. One condition, and only for a *new* key: the prepared bound, which is the logical maximum
        // [Table::footprint] charged row state for. A context that completes or expires frees its slot for the
        // next identification. A fragment for a context already held takes no slot, so payload for a datagram
        // already being reassembled is never refused by this at all.
        if !self.contexts.contains_key(&fragment.key) && self.contexts.len() >= self.prepared {
            self.counters.denied += 1;
            return Err(Reject::Denied);
        }
        let context = match self.contexts.entry(fragment.key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(Context {
                payload: Vec::new(),
                received: Vec::new(),
                header: None,
                total: None,
                deadline: now + REASSEMBLY_TIMEOUT,
                charged: 0,
            }),
        };
        if let Some(known) = context.total {
            if total.is_some_and(|total| total != known) || end > known {
                self.discard(fragment.key, admission, lease);
                self.counters.overlapping += 1;
                return Err(Reject::Overlap);
            }
        }
        // Charged before anything is allocated, and on the whole peak rather than on the fragment: a single
        // fragment at a high offset costs the span it opens up, a sparse pattern costs the ranges it splits
        // the context into, and taking either in costs the old buffer alive beside its replacement - all of
        // which a per-fragment charge would miss.
        //
        // Whether this fragment *completes* the datagram is known here, before the insert, because the
        // assembled packet exists beside the payload it is built from and that peak has to be granted before
        // either is allocated.
        let previous = context.charged;
        let completing = total.or(context.total).is_some_and(|total| {
            // Completion needs one range covering the whole declared length. This fragment produces that only
            // if what is already held, plus this fragment, is exactly that - which the merge below decides;
            // over-projecting here is conservative in the right direction.
            end == total || context.received.iter().any(|range| range.end == total)
        });
        let Some(projection) = context.project(end, completing) else {
            self.counters.denied += 1;
            if context.received.is_empty() {
                self.contexts.remove(&fragment.key);
            }
            return Err(Reject::Denied);
        };
        let growth = projection.retained.saturating_sub(previous);
        let Some(reserve) = growth.checked_add(projection.peak) else {
            self.counters.denied += 1;
            if context.received.is_empty() {
                self.contexts.remove(&fragment.key);
            }
            return Err(Reject::Denied);
        };
        // Nested inside the aggregate rather than beside it: naming the same bytes as fragment bytes is what
        // makes the reassembly cap a check within the byte total instead of a second pool that could promise
        // memory the total had already promised elsewhere.
        if reserve > 0
            && admission
                .grow(
                    lease,
                    Request {
                        bytes: reserve,
                        byte_class: Class::General,
                        fragment_bytes: reserve,
                        ..Request::default()
                    },
                )
                .is_err()
        {
            self.counters.denied += 1;
            // Left in place rather than discarded: what is already held is still worth completing, and the
            // Resource Policy refuses new work instead of evicting established state. A context that has held
            // nothing yet is one this fragment created, so it goes with the refusal - and because the header
            // is inline and nothing was allocated for it, a denied fragment zero leaves nothing behind at all.
            if context.received.is_empty() {
                self.contexts.remove(&fragment.key);
            }
            return Err(Reject::Denied);
        }
        self.charged += reserve;
        let context = self
            .contexts
            .get_mut(&fragment.key)
            .expect("just inserted or found");
        // The context owes the whole reservation from the moment it is granted. Recorded before the insertion
        // rather than after, because the insertion can fail: a context discarded while its own row still
        // named the pre-growth figure would strand the growth in the aggregate for the session.
        context.charged = previous + reserve;
        if !insert(context, fragment.offset, fragment.payload) {
            self.discard(fragment.key, admission, lease);
            self.counters.overlapping += 1;
            return Err(Reject::Overlap);
        }
        if let Some(header) = fragment.header {
            context.header = Some(header);
        }
        if let Some(total) = total {
            context.total = Some(total);
        }
        // The transient half of the reservation is over for everything except a completing fragment, whose
        // assembled packet is built below and released with the rest of the context. Reconciled *downward*
        // only: the projection bounds the real allocation from over, so an actual above the reservation is an
        // undercharge rather than something to saturate away, and it is answered by keeping the reservation
        // and counting the violation rather than by pretending the memory is not there.
        let reserved = previous + reserve;
        let actual = context.footprint() as u64;
        let keep = if context.complete() {
            // The assembled packet's share stays held until it has been built and handed over.
            actual.checked_add(projection.peak)
        } else {
            Some(actual)
        };
        let Some(excess) = keep.and_then(|keep| reconcile(reserved, keep)) else {
            return Err(self.undercharged(fragment.key, admission, lease));
        };
        if excess > 0 {
            admission.shrink(
                lease,
                Request {
                    bytes: excess,
                    byte_class: Class::General,
                    fragment_bytes: excess,
                    ..Request::default()
                },
            );
            self.charged -= excess;
        }
        let context = self
            .contexts
            .get_mut(&fragment.key)
            .expect("just reconciled");
        context.charged = reserved - excess;
        self.peak = self.peak.max(self.charged);
        if !context.complete() {
            self.counters.held += 1;
            return Ok(Accepted::Pending);
        }
        let context = self.contexts.remove(&fragment.key).expect("just held");
        let Some(header) = context.header else {
            // Every byte arrived but fragment zero never did, so there is no header the client sent to speak
            // from and nothing honest to hand on. Nothing is assembled, so the whole reservation goes back.
            let charged = context.charged;
            drop(context);
            self.release(charged, admission, lease);
            self.counters.headless += 1;
            return Err(Reject::Malformed("reassembled without fragment zero"));
        };
        // Assembled while the context is still charged, and the charge released only afterwards - an ordering
        // rather than a formality. For the moment between them the payload and the packet built from it both
        // exist; releasing first would describe a moment that never happened, and it is exactly the moment
        // where this path holds the most. [complete] cannot get that wrong, because it does not release: it
        // hands back what is owed, and only its caller can give that back.
        let (assembled, charged) = complete(context, &header);
        self.release(charged, admission, lease);
        self.counters.completed += 1;
        Ok(Accepted::Complete(assembled))
    }

    /// Drops every context, which is what an epoch change requires: each is keyed by a TUN-visible tuple, so
    /// after one the same key means a different client. Nothing is awaited and no error is sent - a context
    /// retired because the session moved on is not a path property the client should hear about.
    pub fn retire(&mut self, admission: &mut Admission, lease: &Lease) {
        self.contexts.clear();
        let held = self.charged;
        self.release(held, admission, lease);
    }

    fn discard(&mut self, key: Key, admission: &mut Admission, lease: &Lease) {
        if let Some(context) = self.contexts.remove(&key) {
            self.release(context.charged, admission, lease);
        }
    }

    /// Fails closed on a context whose real capacity exceeds what was ever reserved for it.
    ///
    /// Unreachable while [Context::project] bounds every allocation from above - and it does, because every
    /// buffer this path builds is allocated at exactly the capacity the projection named, and the projection
    /// reads the real capacities rather than guessing at them. Written anyway, and written as a discard
    /// rather than as a saturating subtraction, because the alternative is silence: a context carried on with
    /// an undercharge holds capacity nothing granted, and every further fragment widens the gap by an amount
    /// a client chooses. Discarding gives the whole reservation back with the context, so the aggregate stays
    /// exactly true, and the counter says the projection is what needs fixing.
    fn undercharged(&mut self, key: Key, admission: &mut Admission, lease: &Lease) -> Reject {
        self.counters.undercharged += 1;
        self.discard(key, admission, lease);
        Reject::Denied
    }

    /// Gives context bytes back, after the state they stood for has actually been dropped.
    ///
    /// The table's own charge stays either way - it covers the logical maximum rather than the contexts in it
    /// - so what is released here is only ever what a context allocated, never the row state that held it.
    fn release(&mut self, bytes: u64, admission: &mut Admission, lease: &Lease) {
        if bytes == 0 {
            return;
        }
        admission.shrink(
            lease,
            Request {
                bytes,
                byte_class: Class::General,
                fragment_bytes: bytes,
                ..Request::default()
            },
        );
        self.charged -= bytes;
    }

    /// Retires contexts whose deadline passed, returning fragment zero for each so the caller can answer the
    /// Time Exceeded a router owes. A context that never received fragment zero yields nothing: the error
    /// would have to quote a header the daemon never saw.
    /// Retires every context whose deadline has passed, handing each quotable one to [quote] as it is built.
    ///
    /// Streamed rather than batched, and that is a bound rather than a style: the batch this replaces was a
    /// `Vec<Vec<u8>>` sized by however many contexts happened to expire in the same instant, each holding a
    /// reassembled packet. A client that opens many contexts and lets them all time out together chose that
    /// allocation. Handing them over one at a time means exactly one assembled packet exists at a time,
    /// whatever expires together, and the caller's own bounded output path is what it has to fit through.
    ///
    /// Answers how many contexts were retired, quotable or not.
    pub fn sweep(
        &mut self,
        now: Instant,
        admission: &mut Admission,
        lease: &Lease,
        mut quote: impl FnMut(Vec<u8>),
    ) -> u64 {
        let mut freed = 0;
        let mut retired = 0u64;
        let mut quoted = 0u64;
        self.contexts.retain(|_, context| {
            if context.deadline > now {
                return true;
            }
            freed += context.charged;
            retired += 1;
            if let Some(header) = &context.header {
                quoted += 1;
                quote(assemble(header.as_slice(), &context.payload));
            }
            false
        });
        self.release(freed, admission, lease);
        // Every retirement is counted, not just the quotable ones: a context that timed out without fragment
        // zero is still a timeout, and leaving it out would make held-minus-completed look like a leak.
        self.counters.expired += retired;
        self.counters.headless += retired - quoted;
        retired
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.contexts.values().map(|context| context.deadline).min()
    }

    pub fn describe(&self) -> String {
        format!(
            "{} contexts of {} prepared holding {} bytes, peak {}, held {} completed {} malformed {} \
             overlapping {} denied {} expired {} headless {} undercharged {}",
            self.contexts.len(),
            self.prepared,
            self.charged,
            self.peak,
            self.counters.held,
            self.counters.completed,
            self.counters.malformed,
            self.counters.overlapping,
            self.counters.denied,
            self.counters.expired,
            self.counters.headless,
            self.counters.undercharged
        )
    }
}

/// Merges one new range into a sorted, non-overlapping list, into an exactly sized replacement.
///
/// Built from the old list *by reference* rather than by pushing into it first, and that is the allocation
/// order the charge depends on. Pushing into `received` and merging afterwards meant the list grew
/// geometrically for an instant - a `Vec` at capacity doubles - so the real allocation could sit above
/// whatever had been projected and granted, and the reconciliation afterwards would find more than was
/// reserved. Here the replacement is allocated once, at the exact capacity the projection named, and the old
/// list is still intact beside it - which is the peak [Context::project] charges.
///
/// The list is kept merged as fragments arrive rather than at completion, which is what keeps the completion
/// test a single comparison and the list proportional to the gaps rather than to the fragments.
fn merge_ranges(old: &[Range<usize>], new: Range<usize>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(old.len() + 1);
    let mut placed = false;
    for range in old {
        if !placed && new.start <= range.start {
            push_merged(&mut merged, new.clone());
            placed = true;
        }
        push_merged(&mut merged, range.clone());
    }
    if !placed {
        push_merged(&mut merged, new);
    }
    merged
}

/// Appends one range, coalescing it with the last when they touch.
fn push_merged(merged: &mut Vec<Range<usize>>, range: Range<usize>) {
    match merged.last_mut() {
        Some(last) if last.end == range.start => last.end = range.end,
        _ => merged.push(range),
    }
}

/// Places one fragment's bytes, or reports that it contradicts what is already there.
fn insert(context: &mut Context, offset: usize, payload: &[u8]) -> bool {
    let end = offset + payload.len();
    if context
        .received
        .iter()
        .any(|range| offset < range.end && range.start < end)
    {
        return false;
    }
    if context.payload.len() < end {
        if end > context.payload.capacity() {
            // An exactly sized replacement rather than `resize`, whose geometric growth would leave the real
            // capacity above whatever was projected and charged. Both buffers are alive across this move,
            // which is the peak [Context::project] names.
            let mut next = Vec::with_capacity(end);
            next.extend_from_slice(&context.payload);
            next.resize(end, 0);
            context.payload = next;
        } else {
            context.payload.resize(end, 0);
        }
    }
    context.payload[offset..end].copy_from_slice(payload);
    // Built beside the old list at exactly the projected capacity, then swapped in - the old one is dropped
    // by the assignment, after the new one exists.
    context.received = merge_ranges(&context.received, offset..end);
    true
}

/// What a reconciliation may give back, once the real retained capacity is known.
///
/// `None` is the fail-closed answer: more is allocated than was ever reserved for it. Checked rather than
/// saturating, because saturation is what turns an undercharge into silence - the caller carries on holding a
/// context whose real capacity is above anything granted for it, and every further fragment widens the gap by
/// an amount a client chooses.
fn reconcile(reserved: u64, keep: u64) -> Option<u64> {
    reserved.checked_sub(keep)
}

/// Builds the completed datagram while its context is still charged, and answers what that context owed.
///
/// Split out so the ordering cannot be written the other way round. The assembled packet exists beside the
/// payload it was built from, so the context's charge has to cover both at that instant - which the
/// completion peak in [Context::project] is reserved for. This takes the context by value and returns the
/// amount owed, so releasing it is necessarily something the caller does *after* this has returned, with the
/// packet already in hand: there is no way to spell "release, then assemble" through this function.
fn complete(context: Context, header: &Header) -> (Vec<u8>, u64) {
    let assembled = assemble(header.as_slice(), &context.payload);
    let charged = context.charged;
    // Explicit: the source is gone before the caller is told what may be given back.
    drop(context);
    (assembled, charged)
}

/// Rebuilds the datagram fragment zero's header describes, with the fragmentation taken back out.
fn assemble(header: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(header.len() + payload.len());
    packet.extend_from_slice(header);
    packet.extend_from_slice(payload);
    match packet.first().map(|byte| byte >> 4) {
        Some(4) => {
            let length = u16::try_from(packet.len()).unwrap_or(u16::MAX);
            packet[2..4].copy_from_slice(&length.to_be_bytes());
            // flags and offset both cleared: this is no longer a fragment, and leaving either would make the
            // strict parses reject what was just reassembled
            packet[6] = 0;
            packet[7] = 0;
            packet[10] = 0;
            packet[11] = 0;
            if let Ok((parsed, _)) = Ipv4Header::from_slice(&packet) {
                let checksum = parsed.calc_header_checksum();
                packet[10..12].copy_from_slice(&checksum.to_be_bytes());
            }
        }
        _ => {
            let length = u16::try_from(payload.len()).unwrap_or(u16::MAX);
            packet[4..6].copy_from_slice(&length.to_be_bytes());
        }
    }
    packet
}

/// What both specifications say a fragment's payload may look like, which is a validity question rather than
/// a resource one and therefore belongs in the parse.
///
/// RFC 791 section 3.2 and RFC 8200 section 4.5: the offset field counts eight-octet units, so every fragment
/// but the last has to end on one - a non-final fragment of some other length makes the next fragment's
/// offset unrepresentable, and no conforming sender produces one. The last fragment may be any length, since
/// nothing follows it to be addressed.
///
/// An empty non-atomic fragment is refused for the reason it is useless: it contributes no bytes, so its only
/// effect is to create or hold open a reassembly context. Per the security posture that is a resource any
/// local app can drive, and admitting it would mean charging for state that can never complete.
fn shape(offset: usize, more: bool, payload: usize) -> Result<(), Reject> {
    if more {
        if payload == 0 {
            return Err(Reject::Malformed("a non-final fragment carries no payload"));
        }
        if !payload.is_multiple_of(8) {
            return Err(Reject::Malformed(
                "a non-final fragment is not a multiple of eight octets",
            ));
        }
    } else if offset != 0 && payload == 0 {
        return Err(Reject::Malformed("a final fragment carries no payload"));
    }
    Ok(())
}

fn parse(packet: &[u8]) -> Result<Fragment<'_>, Reject> {
    match Packet::parse(packet).map_err(|error| Reject::Malformed(error.message()))? {
        Packet::Ipv4 { header, payload } => {
            let offset = usize::from(header.fragments_offset().byte_offset());
            let more = header.more_fragments();
            if !more && offset == 0 {
                return Err(Reject::Malformed("IPv4 packet is not a fragment"));
            }
            shape(offset, more, payload.len())?;
            Ok(Fragment {
                key: Key {
                    source: IpAddr::V4(header.source_addr()),
                    destination: IpAddr::V4(header.destination_addr()),
                    identification: u32::from(header.identification()),
                    protocol: header.protocol().0,
                },
                offset,
                more,
                header: if offset == 0 {
                    // A header longer than one can be was already refused by the length check above, so this
                    // cannot be `None` here - and if it ever were, refusing the fragment is the right answer.
                    Some(Header::new(header.slice()).ok_or(Reject::Malformed(
                        "IPv4 header is longer than a header can be",
                    ))?)
                } else {
                    None
                },
                payload,
            })
        }
        Packet::Ipv6 { header, payload } => {
            if header.next_header() != IpNumber::IPV6_FRAGMENTATION_HEADER {
                return Err(Reject::Malformed("IPv6 packet is not a fragment"));
            }
            let fragment = Ipv6FragmentHeaderSlice::from_slice(payload)
                .map_err(|_| Reject::Malformed("IPv6 fragment header does not fit"))?;
            let offset = usize::from(fragment.fragment_offset().byte_offset());
            let more = fragment.more_fragments();
            let payload = &payload[fragment.slice().len()..];
            shape(offset, more, payload.len())?;
            let mut zero = None;
            if offset == 0 {
                // The Fragment header is spliced out and the header it pointed at promoted, which is what
                // makes the result an ordinary IPv6 packet rather than one every parse has to special-case.
                // Patched in the inline copy rather than in a heap one: the fixed IPv6 header is well inside
                // [MAX_HEADER], so this cannot fail.
                let mut first = Header::new(header.slice())
                    .ok_or(Reject::Malformed("IPv6 header does not fit"))?;
                first.bytes[6] = fragment.next_header().0;
                zero = Some(first);
            }
            Ok(Fragment {
                key: Key {
                    source: IpAddr::V6(header.source_addr()),
                    destination: IpAddr::V6(header.destination_addr()),
                    identification: fragment.identification(),
                    protocol: 0,
                },
                offset,
                more,
                header: zero,
                payload,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::packet_writer::{fragment_ipv4, fragment_ipv6};
    use crate::shared::udp_wire::{self, build_reply};
    use std::net::SocketAddr;

    const CLIENT4: &str = "192.0.2.1:40000";
    const REMOTE4: &str = "198.51.100.7:443";
    const CLIENT6: &str = "[2001:db8:1::2]:40000";
    const REMOTE6: &str = "[2606:4700::1111]:443";

    /// A whole datagram of `payload` bytes, as the client would have sent it unfragmented.
    fn datagram(ipv6: bool, payload: usize) -> Vec<u8> {
        let (client, remote): (SocketAddr, SocketAddr) = if ipv6 {
            (CLIENT6.parse().unwrap(), REMOTE6.parse().unwrap())
        } else {
            (CLIENT4.parse().unwrap(), REMOTE4.parse().unwrap())
        };
        // Some identification, cleared again by reassembly, so the round trip is exact either way.
        build_reply(client, remote, 64, Some(0x1234), &vec![0x5au8; payload]).unwrap()
    }

    /// The production fragmenter's output, so the test exercises the real shape rather than a hand-rolled one.
    fn fragments(packet: &[u8], mtu: usize) -> Vec<Vec<u8>> {
        let mut pieces = Vec::new();
        if packet[0] >> 4 == 6 {
            fragment_ipv6(packet, mtu, 0x9abcdef0, |piece| pieces.push(piece)).unwrap();
        } else {
            fragment_ipv4(packet, mtu, |piece| pieces.push(piece)).unwrap();
        }
        pieces
    }

    /// A table beside the one admission it charges against, which is the production shape: there is no pool
    /// here any more, so a test that does not carry the aggregate is not testing what runs.
    struct Fixture {
        admission: Admission,
        lease: Lease,
        table: Table,
    }

    impl Fixture {
        fn with_cap(fragment_cap: u64) -> Self {
            let mut admission = Admission::new(crate::shared::admission::Totals {
                admission_id: 1,
                record_total: 64,
                dns_record_floor: 0,
                byte_total: 8 << 20,
                reserved_byte_floor: 1 << 20,
                fragment_cap,
                dns_token_cap: 0,
                byte_only_owners: 4,
            })
            .expect("the fixture totals hold their own accounting");
            let lease = admission
                .reserve(Request::bytes(
                    Table::footprint(64).expect("fits"),
                    Class::General,
                ))
                .expect("granted");
            Self {
                admission,
                lease,
                table: Table::with_capacity(64),
            }
        }

        fn accept(&mut self, packet: &[u8], now: Instant) -> Result<Accepted, Reject> {
            self.table
                .accept(packet, now, &mut self.admission, &self.lease)
        }

        fn sweep(&mut self, now: Instant, quote: impl FnMut(Vec<u8>)) -> u64 {
            self.table
                .sweep(now, &mut self.admission, &self.lease, quote)
        }

        fn retire(&mut self) {
            self.table.retire(&mut self.admission, &self.lease);
        }
    }

    fn table() -> Fixture {
        Fixture::with_cap(1 << 20)
    }

    #[test]
    fn fragments_reassemble_to_the_original_datagram() {
        for ipv6 in [false, true] {
            let packet = datagram(ipv6, 3000);
            let pieces = fragments(&packet, 1280);
            assert!(pieces.len() > 2, "{} pieces", pieces.len());
            let mut table = table();
            let now = Instant::now();
            let mut completed = None;
            for piece in &pieces {
                match table.accept(piece, now).unwrap() {
                    Accepted::Pending => {}
                    Accepted::Complete(whole) => completed = Some(whole),
                }
            }
            // byte for byte, including the client's own hop limit and ports, and parseable as what it was
            assert_eq!(completed.as_deref(), Some(packet.as_slice()), "ipv6 {ipv6}");
            assert!(udp_wire::parse(&completed.unwrap()).is_ok(), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0);
            assert!(table.table.contexts.is_empty());
        }
    }

    #[test]
    fn order_does_not_matter() {
        for ipv6 in [false, true] {
            let packet = datagram(ipv6, 2500);
            let mut pieces = fragments(&packet, 1280);
            pieces.reverse();
            let mut table = table();
            let now = Instant::now();
            let mut completed = None;
            for piece in &pieces {
                if let Accepted::Complete(whole) = table.accept(piece, now).unwrap() {
                    completed = Some(whole);
                }
            }
            assert_eq!(completed.as_deref(), Some(packet.as_slice()), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0);
        }
    }

    #[test]
    fn an_overlapping_fragment_discards_the_whole_datagram() {
        for ipv6 in [false, true] {
            let packet = datagram(ipv6, 3000);
            let pieces = fragments(&packet, 1280);
            let mut table = table();
            let now = Instant::now();
            assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
            // the same fragment again, which is the degenerate overlap
            assert_eq!(table.accept(&pieces[0], now), Err(Reject::Overlap));
            // and the context is gone rather than left holding half a datagram
            assert!(table.table.contexts.is_empty(), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0);
            // so the rest can no longer complete it
            for piece in &pieces[1..] {
                assert_eq!(table.accept(piece, now), Ok(Accepted::Pending));
            }
        }
    }

    #[test]
    fn a_fragment_reaching_past_a_declared_total_is_refused() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let tail = pieces.last().unwrap();
        let mut held = table();
        let now = Instant::now();
        // the tail carries no successor, so accepting it is what makes the total known
        assert_eq!(held.accept(tail, now), Ok(Accepted::Pending));
        let flags = u16::from_be_bytes([tail[6], tail[7]]);
        let header = ((tail[0] & 0xf) as usize) * 4;
        let total = ((flags & 0x1fff) as usize) * 8 + (tail.len() - header);
        // placed exactly where the datagram ended and claiming more to follow: no overlap with anything held,
        // so this exercises the contradiction rather than the overlap rule
        let mut beyond = pieces[0].clone();
        beyond[6..8].copy_from_slice(&(0x2000 | (total / 8) as u16).to_be_bytes());
        assert_eq!(held.accept(&beyond, now), Err(Reject::Overlap));
        assert!(held.table.contexts.is_empty());
        assert_eq!(held.table.charged, 0);
    }

    #[test]
    fn an_atomic_ipv6_fragment_completes_at_once() {
        // Offset zero with no successor: RFC 8200 says treat it as unfragmented, so it must come straight
        // back out with the Fragment header spliced away.
        let packet = datagram(true, 100);
        let pieces = fragments(&packet, 1500);
        assert_eq!(pieces.len(), 1);
        let mut table = table();
        assert_eq!(
            table.accept(&pieces[0], Instant::now()),
            Ok(Accepted::Complete(packet))
        );
        assert!(table.table.contexts.is_empty());
    }

    #[test]
    fn an_unfragmented_packet_is_not_mistaken_for_one() {
        let mut table = table();
        let now = Instant::now();
        assert!(matches!(
            table.accept(&datagram(false, 100), now),
            Err(Reject::Malformed(_))
        ));
        assert!(matches!(
            table.accept(&datagram(true, 100), now),
            Err(Reject::Malformed(_))
        ));
        assert!(matches!(table.accept(&[], now), Err(Reject::Malformed(_))));
    }

    #[test]
    fn a_datagram_missing_fragment_zero_is_refused() {
        let packet = datagram(false, 2000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        // everything but the first, so the byte range completes only if fragment zero is not required
        for piece in &pieces[1..] {
            let _ = table.accept(piece, now);
        }
        assert!(!table.table.contexts.is_empty());
        assert!(table.table.charged > 0);
    }

    #[test]
    fn the_ceiling_refuses_growth_and_the_charge_is_returned() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let now = Instant::now();
        // Sized from what one fragment actually costs rather than from its payload span, because the charge
        // is the context's whole footprint: room for the first and nothing like the whole datagram.
        let mut probe = table();
        assert_eq!(probe.accept(&pieces[0], now), Ok(Accepted::Pending));
        let mut table = Fixture::with_cap(probe.table.charged);
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        let held = table.table.charged;
        assert!(held > 0);
        assert_eq!(table.accept(&pieces[1], now), Err(Reject::Denied));
        // the refusal changed nothing: what was already held is untouched, per the Resource Policy
        assert_eq!(table.table.charged, held);
        assert_eq!(table.table.contexts.len(), 1);
    }

    /// An IPv4 fragment with an arbitrary offset, more-fragments bit and payload length, so the shapes no
    /// conforming sender produces can be presented at all - the production fragmenter cannot build them.
    fn ipv4_fragment(offset: usize, more: bool, payload: usize) -> Vec<u8> {
        let mut packet = vec![0u8; IPV4_HEADER_LEN + payload];
        packet[0] = 0x45;
        let total = u16::try_from(packet.len()).unwrap();
        packet[2..4].copy_from_slice(&total.to_be_bytes());
        packet[4..6].copy_from_slice(&0x1234u16.to_be_bytes());
        let flags = u16::try_from(offset / 8).unwrap() | if more { 0x2000 } else { 0 };
        packet[6..8].copy_from_slice(&flags.to_be_bytes());
        packet[8] = 64;
        packet[9] = IpNumber::UDP.0;
        packet[12..16].copy_from_slice(&[192, 0, 2, 1]);
        packet[16..20].copy_from_slice(&[198, 51, 100, 7]);
        packet
    }

    fn ipv6_fragment(offset: usize, more: bool, payload: usize) -> Vec<u8> {
        let mut packet = vec![0u8; IPV6_HEADER_LEN + IPV6_FRAGMENT_HEADER_LEN + payload];
        packet[0] = 0x60;
        let length = u16::try_from(packet.len() - IPV6_HEADER_LEN).unwrap();
        packet[4..6].copy_from_slice(&length.to_be_bytes());
        packet[6] = IpNumber::IPV6_FRAGMENTATION_HEADER.0;
        packet[7] = 64;
        packet[8..24].copy_from_slice(&Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2).octets());
        packet[24..40]
            .copy_from_slice(&Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111).octets());
        packet[40] = IpNumber::UDP.0;
        let control = u16::try_from(offset).unwrap() | if more { 1 } else { 0 };
        packet[42..44].copy_from_slice(&control.to_be_bytes());
        packet[44..48].copy_from_slice(&0x9abcdef0u32.to_be_bytes());
        packet
    }

    /// A fragment that carries nothing can never complete anything, so its only effect would be to open or
    /// hold a context - which is state a local app can drive at will.
    #[test]
    fn an_empty_non_atomic_fragment_is_refused_in_both_families() {
        let mut table = table();
        let now = Instant::now();
        for packet in [
            ipv4_fragment(0, true, 0),
            ipv4_fragment(1480, false, 0),
            ipv6_fragment(0, true, 0),
            ipv6_fragment(1448, false, 0),
        ] {
            assert!(matches!(
                table.accept(&packet, now),
                Err(Reject::Malformed(_))
            ));
        }
        assert!(table.table.contexts.is_empty());
        assert_eq!(table.table.charged, 0);
    }

    /// The offset field counts eight-octet units, so a non-final fragment of any other length makes the next
    /// fragment's offset unrepresentable.
    #[test]
    fn a_misaligned_non_final_fragment_is_refused_in_both_families() {
        let mut table = table();
        let now = Instant::now();
        for payload in [1usize, 7, 9, 15, 1449] {
            for packet in [
                ipv4_fragment(0, true, payload),
                ipv6_fragment(0, true, payload),
            ] {
                assert!(
                    matches!(table.accept(&packet, now), Err(Reject::Malformed(_))),
                    "{payload} bytes"
                );
            }
        }
        // A *final* fragment may be any nonzero length, since nothing follows it to be addressed.
        assert_eq!(
            table.accept(&ipv4_fragment(8, false, 9), now),
            Ok(Accepted::Pending)
        );
        assert_eq!(
            table.accept(&ipv6_fragment(8, false, 9), now),
            Ok(Accepted::Pending)
        );
    }

    /// The case a payload-span charge misses entirely: a sparse pattern covers few bytes while splitting the
    /// context into many ranges, so the charge has to grow with the ranges too.
    #[test]
    fn a_sparse_pattern_is_charged_for_the_ranges_it_opens() {
        let mut sparse = table();
        let now = Instant::now();
        // eight octets at every other eight-octet boundary, so no two ranges ever merge
        for slot in 0..64 {
            assert_eq!(
                sparse.accept(&ipv4_fragment(slot * 16, true, 8), now),
                Ok(Accepted::Pending)
            );
        }
        let context = sparse.table.contexts.values().next().unwrap();
        assert_eq!(context.received.len(), 64);
        // one contiguous fragment covering the same span, which is what the old accounting would have charged
        let mut dense = table();
        assert_eq!(
            dense.accept(&ipv4_fragment(0, true, 63 * 16 + 8), now),
            Ok(Accepted::Pending)
        );
        assert!(
            sparse.table.charged > dense.table.charged,
            "sparse {} dense {}",
            sparse.table.charged,
            dense.table.charged
        );
        // and the table's total is still exactly the sum of its contexts
        assert_eq!(
            sparse.table.charged,
            sparse
                .table
                .contexts
                .values()
                .map(|c| c.charged)
                .sum::<u64>()
        );
    }

    /// A live context is charged for the storage it allocated, and the row it sits in is charged once - by the
    /// table's fixed lease rather than again by every context in it.
    ///
    /// An equality on both halves, so either charge growing back into the other fails here. The defect this
    /// closes was the second one: the per-context projection began at `size_of::<(Key, Context)>()`, which
    /// [Table::footprint] had already charged for every row the table was prepared for, so every live context
    /// paid for its own row a second time.
    #[test]
    fn a_context_is_charged_for_its_buffers_and_its_row_only_once() {
        let now = Instant::now();
        let mut fixture = table();
        assert_eq!(
            fixture.accept(&ipv4_fragment(0, true, 1200), now),
            Ok(Accepted::Pending)
        );
        let context = fixture.table.contexts.values().next().expect("held");
        assert_eq!(
            context.charged,
            context.payload.capacity() as u64
                + (context.received.capacity() * std::mem::size_of::<Range<usize>>()) as u64,
            "a context holds its two buffers and nothing else"
        );
        // The other half of the pair: what the table's own lease covers is one logical row per prepared
        // context plus the struct, with nothing per-context left in it to pay for twice.
        assert_eq!(
            Table::footprint(fixture.table.prepared).expect("chargeable"),
            (fixture.table.prepared * std::mem::size_of::<(Key, Context)>()) as u64
                + std::mem::size_of::<Table>() as u64,
            "the fixed lease owns the rows"
        );
    }

    /// Which is what makes the ceiling bind on that pattern rather than on the bytes it delivers.
    #[test]
    fn the_ceiling_binds_on_a_sparse_pattern() {
        let now = Instant::now();
        let mut probe = table();
        assert_eq!(
            probe.accept(&ipv4_fragment(0, true, 8), now),
            Ok(Accepted::Pending)
        );
        let mut table = Fixture::with_cap(probe.table.charged);
        assert_eq!(
            table.accept(&ipv4_fragment(0, true, 8), now),
            Ok(Accepted::Pending)
        );
        // The span would grow by eight bytes; the ranges by one entry. Either way there is no room.
        assert_eq!(
            table.accept(&ipv4_fragment(16, true, 8), now),
            Err(Reject::Denied)
        );
        assert_eq!(table.table.contexts.len(), 1);
    }

    /// Retirement and sweeping both have to return the metadata charge, not only the payload one.
    #[test]
    fn every_cleanup_path_returns_the_whole_charge() {
        let now = Instant::now();
        for retire in [true, false] {
            let mut table = table();
            for slot in 0..8 {
                assert_eq!(
                    table.accept(&ipv4_fragment(slot * 16, true, 8), now),
                    Ok(Accepted::Pending)
                );
            }
            assert!(table.table.charged > 0);
            if retire {
                table.retire();
            } else {
                table.sweep(now + REASSEMBLY_TIMEOUT, |_| {});
            }
            assert!(table.table.contexts.is_empty());
            assert_eq!(table.table.charged, 0);
        }
    }

    /// The merged range list is allocated once, at exactly the projected capacity, from the old list beside
    /// it - never by growing the old one first.
    ///
    /// The failure this closes is an allocation the projection did not name: pushing into `received` and
    /// merging afterwards doubles a `Vec` at capacity, so for an instant the real allocation sits above what
    /// was granted, and the reconciliation that follows finds more than was reserved.
    #[test]
    fn a_merge_allocates_once_at_the_projected_capacity() {
        for old_len in [0usize, 1, 2, 3, 4, 7, 8, 9, 16, 33] {
            // A sparse, non-adjacent list, so nothing coalesces and the count is exactly what it looks like.
            let old: Vec<Range<usize>> = (0..old_len).map(|i| (i * 100)..(i * 100 + 8)).collect();
            // A new range past the end, which cannot merge with anything.
            let new = (old_len * 100 + 500)..(old_len * 100 + 508);
            let merged = merge_ranges(&old, new.clone());
            assert_eq!(merged.len(), old_len + 1);
            assert_eq!(
                merged.capacity(),
                old_len + 1,
                "old {old_len}: the replacement is exactly the projected size, not a doubling"
            );
            // Sorted and non-overlapping, which is what the completion test depends on.
            for pair in merged.windows(2) {
                assert!(pair[0].end < pair[1].start, "old {old_len}: not disjoint");
            }
            assert!(merged.contains(&new));
        }

        // A range that touches its neighbours coalesces rather than growing the list, so the capacity is
        // still the bound and the length is below it.
        let old = vec![0..8, 16..24];
        let merged = merge_ranges(&old, 8..16);
        assert_eq!(merged, vec![0..24]);
        assert_eq!(merged.capacity(), 3, "still allocated once, at the bound");

        // Inserted before, between and after, in every position.
        let before = merge_ranges(&[], 16..24);
        assert_eq!(merge_ranges(&before, 0..8), vec![0..8, 16..24]);
        let after = merge_ranges(&[], 0..8);
        assert_eq!(merge_ranges(&after, 16..24), vec![0..8, 16..24]);
        assert_eq!(
            merge_ranges(&[0..8, 32..40], 16..24),
            vec![0..8, 16..24, 32..40]
        );
    }

    /// Completion assembles the packet while the context is still charged, and the charge is only given back
    /// afterwards.
    ///
    /// Observed through the shape rather than asserted about a moment: [complete] takes the context by value
    /// and *returns* what it owed, so its caller cannot have released anything before it ran - there is no
    /// way to spell "release, then assemble" through it. The test checks that the amount comes back nonzero
    /// with the packet in hand, which is exactly the state the release is deferred until.
    #[test]
    fn completion_holds_its_charge_until_the_packet_exists() {
        let payload = vec![0x5au8; 2_000];
        let header = Header::new(&[0x45u8; 20]).expect("a header");
        let context = Context {
            payload: payload.clone(),
            received: merge_ranges(&[], 0..payload.len()),
            header: Some(header),
            total: Some(payload.len()),
            deadline: Instant::now(),
            charged: 4_096,
        };
        let (assembled, charged) = complete(context, &header);
        // The packet exists...
        assert_eq!(assembled.len(), 20 + payload.len());
        // ...and only now is the caller told what may be given back, which is the whole of what was owed.
        assert_eq!(charged, 4_096);

        // And end to end: the charge is still held while the packet is being built, and falls to zero only
        // once the source is gone.
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        let mut completed = None;
        for piece in &pieces {
            match table.accept(piece, now).expect("accepted") {
                Accepted::Pending => assert!(
                    table.table.charged > 0,
                    "a held context is charged for what it holds"
                ),
                Accepted::Complete(whole) => completed = Some(whole),
            }
        }
        assert_eq!(completed.as_deref(), Some(packet.as_slice()));
        assert_eq!(table.table.charged, 0, "and nothing is owed once it is out");
        assert_eq!(table.table.counters.undercharged, 0);
    }

    /// The reconciliation rule itself: give back the difference, or refuse - never saturate.
    #[test]
    fn a_reconciliation_that_would_saturate_fails_closed_instead() {
        assert_eq!(reconcile(1_000, 400), Some(600));
        assert_eq!(reconcile(1_000, 1_000), Some(0));
        // One byte more allocated than reserved is an undercharge, not a zero.
        assert_eq!(reconcile(1_000, 1_001), None);
        assert_eq!(reconcile(0, 1), None);
        assert_eq!(reconcile(u64::MAX, u64::MAX), Some(0));
    }

    /// A context whose real capacity turns out to exceed its reservation is discarded rather than carried on
    /// with, because carrying on is an undercharge that grows with every further fragment.
    ///
    /// Driven directly at the transition rather than through a faked allocator, because the condition is
    /// genuinely unreachable through `accept`: every buffer is allocated at exactly the capacity
    /// [Context::project] named, and the projection reads real capacities. What is checked here is that the
    /// branch does what it claims - counts it, gives the whole reservation back, removes the context, and
    /// leaves the table usable - and the matrix test below is what checks the condition never arises.
    #[test]
    fn an_undercharged_context_is_discarded_rather_than_continued() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        table.accept(&pieces[0], now).expect("the first fits");
        table.accept(&pieces[1], now).expect("and the second");
        assert_eq!(table.table.contexts.len(), 1);
        let held = table.table.charged;
        assert!(held > 0);
        let charged = table.admission.bytes_charged();
        let key = *table.table.contexts.keys().next().expect("held");

        let refused = table
            .table
            .undercharged(key, &mut table.admission, &table.lease);
        assert_eq!(refused, Reject::Denied);
        assert_eq!(
            table.table.counters.undercharged, 1,
            "the violation is counted rather than absorbed"
        );
        assert!(
            table.table.contexts.is_empty(),
            "and the context is gone rather than left holding more than was granted"
        );
        assert_eq!(
            table.table.charged, 0,
            "with its whole reservation given back"
        );
        assert_eq!(table.admission.bytes_charged(), charged - held);
        assert_eq!(table.admission.invariant_violations(), 0);

        // The table still works afterwards: this refuses one context, not the session.
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        assert!(table.table.charged > 0);
    }

    /// A completed context frees its slot and the next identification takes it, while every context already
    /// held carries on with its own fragments.
    ///
    /// The ordinary shape: a client fragments, a datagram completes, another identification arrives. The
    /// prepared bound is the only admission condition, so the slot a completion gives back is one the newcomer
    /// gets - see [Table::accept].
    #[test]
    fn a_completed_context_frees_its_slot_for_the_next_identification() {
        let mut fixture = table();
        let prepared = fixture.table.prepared;
        let packet = datagram(true, 3_000);
        let now = Instant::now();
        let pieces = |identification: u32| -> Vec<Vec<u8>> {
            let mut pieces = Vec::new();
            fragment_ipv6(&packet, 1_280, identification, |piece| pieces.push(piece))
                .expect("a fragmentable datagram");
            assert!(pieces.len() > 2, "{} pieces", pieces.len());
            pieces
        };

        // Fill the bound with contexts that are open but incomplete.
        let mut live: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut identification = 1u32;
        while fixture.table.contexts.len() < prepared {
            let mut fragments = pieces(identification);
            identification += 1;
            assert_eq!(fixture.accept(&fragments[0], now), Ok(Accepted::Pending));
            fragments.remove(0);
            live.push(fragments);
        }
        assert_eq!(fixture.table.contexts.len(), prepared);

        // One more identification is refused, and the refusal leaves every held context alone.
        let extra = pieces(identification);
        identification += 1;
        assert_eq!(fixture.accept(&extra[0], now), Err(Reject::Denied));
        assert_eq!(
            fixture.table.contexts.len(),
            prepared,
            "nothing was evicted"
        );

        // Completing one frees its row, and the next identification takes that row.
        let finishing = live.remove(0);
        let mut completed = false;
        for piece in &finishing {
            match fixture
                .accept(piece, now)
                .expect("a held context takes its own pieces")
            {
                Accepted::Pending => {}
                Accepted::Complete(_) => completed = true,
            }
        }
        assert!(
            completed,
            "the datagram was reassembled and its row released"
        );
        assert_eq!(
            fixture.table.contexts.len(),
            prepared - 1,
            "the slot came back"
        );
        let newcomer = pieces(identification);
        assert_eq!(
            fixture.accept(&newcomer[0], now),
            Ok(Accepted::Pending),
            "and the next identification takes it"
        );
        assert_eq!(fixture.table.contexts.len(), prepared);
        assert_eq!(
            fixture.table.prepared, prepared,
            "with the bound the charge covers where it was"
        );
        // And what was already held is still able to take its own fragments.
        for fragments in &live {
            for piece in fragments {
                assert!(
                    fixture.accept(piece, now).is_ok(),
                    "a held context takes its own pieces"
                );
            }
        }

        fixture.retire();
        assert_eq!(fixture.table.charged, 0);
        assert_eq!(fixture.admission.invariant_violations(), 0);
    }

    /// A denied fragment zero leaves nothing at all: no context, no charge, and - because the header is
    /// inline - no allocation made before the refusal.
    ///
    /// The failure this closes was in the parser: it heap-copied fragment zero's header before admission had
    /// granted anything, so a sender could drive one copy per fragment past a table that was about to refuse
    /// every one of them.
    #[test]
    fn a_denied_fragment_zero_allocates_and_retains_nothing() {
        for ipv6 in [false, true] {
            // A cap that cannot hold the first fragment's payload, so the very first fragment is refused.
            let mut table = Fixture::with_cap(8);
            let packet = datagram(ipv6, 3000);
            let pieces = fragments(&packet, 1280);
            let charged = table.admission.bytes_charged();
            let now = Instant::now();
            assert_eq!(table.accept(&pieces[0], now), Err(Reject::Denied));
            assert!(table.table.contexts.is_empty(), "ipv6 {ipv6}");
            assert_eq!(table.table.charged, 0, "ipv6 {ipv6}");
            assert_eq!(
                table.admission.bytes_charged(),
                charged,
                "a refusal charges nothing, ipv6 {ipv6}"
            );
            assert_eq!(table.table.counters.undercharged, 0);
        }
    }

    /// A denial leaves an existing context and its charge exactly as they were, rather than evicting it to
    /// make room for the fragment that could not fit.
    #[test]
    fn a_denial_preserves_the_context_it_could_not_grow() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        // Enough for the first fragment and nothing like enough for the second.
        let mut probe = Fixture::with_cap(1 << 20);
        let now = Instant::now();
        probe.accept(&pieces[0], now).expect("the first fits");
        let held = probe.table.charged;

        let mut table = Fixture::with_cap(held);
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        assert_eq!(table.table.charged, held);
        let contexts = table.table.contexts.len();
        assert_eq!(table.accept(&pieces[1], now), Err(Reject::Denied));
        assert_eq!(table.table.contexts.len(), contexts, "nothing was evicted");
        assert_eq!(table.table.charged, held, "and nothing was refunded");
        // The context is still usable: what it held is still worth completing.
        assert_eq!(table.table.contexts.values().next().unwrap().charged, held);
    }

    /// Whatever the offsets and however sparse the pattern, the reservation covers everything that was really
    /// allocated - including the moment an old buffer and its replacement are both alive.
    #[test]
    fn no_offset_pattern_ever_allocates_past_its_reservation() {
        for ipv6 in [false, true] {
            for mtu in [576usize, 1280, 1500] {
                for reverse in [false, true] {
                    for size in [8usize, 100, 3000, 20_000] {
                        let packet = datagram(ipv6, size);
                        let mut pieces = fragments(&packet, mtu);
                        if reverse {
                            pieces.reverse();
                        }
                        let mut table = Fixture::with_cap(1 << 22);
                        let now = Instant::now();
                        for piece in &pieces {
                            let _ = table.accept(piece, now);
                            // The invariant the checked reconciliation enforces: nothing was ever found
                            // allocated beyond what had been reserved for it.
                            assert_eq!(
                                table.table.counters.undercharged, 0,
                                "ipv6 {ipv6} mtu {mtu} reverse {reverse} size {size}"
                            );
                            // And the table's own total is exactly the sum of its contexts' charges.
                            assert_eq!(
                                table.table.charged,
                                table
                                    .table
                                    .contexts
                                    .values()
                                    .map(|context| context.charged)
                                    .sum::<u64>(),
                                "ipv6 {ipv6} mtu {mtu} reverse {reverse} size {size}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A sparse pattern pays for the range list it forces *and* for the merge that rebuilds it beside itself.
    #[test]
    fn a_sparse_pattern_charges_the_old_and_new_range_storage_at_once() {
        let packet = datagram(false, 4000);
        let pieces = fragments(&packet, 600);
        let mut table = Fixture::with_cap(1 << 22);
        let now = Instant::now();
        // Every other fragment, so each one opens a new range rather than merging with a neighbour.
        for piece in pieces.iter().step_by(2) {
            assert_eq!(table.accept(piece, now), Ok(Accepted::Pending));
        }
        let context = table.table.contexts.values().next().expect("held");
        assert!(
            context.received.len() > 1,
            "the pattern must actually be sparse: {} ranges",
            context.received.len()
        );
        // The projection for the next fragment names both the list it will build and the one it will build it
        // beside, so the peak is the sum rather than the larger.
        let range = std::mem::size_of::<Range<usize>>() as u64;
        let projection = context.project(context.payload.len(), false).expect("fits");
        assert!(
            projection.peak >= context.received.capacity() as u64 * range,
            "the peak covers the range list being replaced"
        );
        assert_eq!(table.table.counters.undercharged, 0);
    }

    /// Growing the payload charges the old buffer and its replacement at once, and the fragment that
    /// completes the datagram charges the assembled packet beside the payload it came from.
    #[test]
    fn payload_growth_and_completion_charge_both_buffers_at_once() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = Fixture::with_cap(1 << 22);
        let now = Instant::now();
        table.accept(&pieces[0], now).expect("the first fits");

        let context = table.table.contexts.values().next().expect("held");
        let held = context.payload.capacity() as u64;
        assert!(held > 0);
        // A fragment that grows the payload names the buffer it is replacing as well as the replacement.
        let growing = context
            .project(context.payload.capacity() + 1, false)
            .expect("fits");
        assert!(
            growing.peak >= held,
            "the peak covers the payload being replaced: {} < {held}",
            growing.peak
        );
        // A fragment that also completes names the assembled packet on top of that.
        let completing = context
            .project(context.payload.capacity() + 1, true)
            .expect("fits");
        assert!(
            completing.peak > growing.peak,
            "completion costs more than growth alone"
        );

        // And the whole datagram really does complete, with the accounting back to zero afterwards.
        for piece in &pieces[1..] {
            if let Ok(Accepted::Complete(whole)) = table.accept(piece, now) {
                assert_eq!(whole, packet);
            }
        }
        assert_eq!(table.table.charged, 0);
        assert!(table.table.contexts.is_empty());
        assert_eq!(table.table.counters.undercharged, 0);
    }

    /// However many contexts expire in the same instant, only one assembled packet exists at a time.
    ///
    /// The batch this replaces allocated one reassembled packet per expiring context, and how many expire
    /// together is a client's choice: open many contexts, send fragment zero to each, and let them all time
    /// out. Streaming makes the peak one packet rather than one per context.
    #[test]
    fn simultaneous_expiries_never_hold_more_than_one_quote() {
        let mut table = table();
        let now = Instant::now();
        let mut opened = 0u64;
        // As many contexts as the table will hold, all with the same deadline.
        for id in 0..64u16 {
            let mut packet = datagram(false, 3000);
            packet[4..6].copy_from_slice(&id.to_be_bytes());
            let pieces = fragments(&packet, 1280);
            if table.accept(&pieces[0], now) != Ok(Accepted::Pending) {
                break;
            }
            opened += 1;
        }
        assert!(opened > 1, "the table must hold several contexts at once");

        let mut live = 0usize;
        let mut peak = 0usize;
        let mut quoted = 0u64;
        let retired = table.sweep(now + REASSEMBLY_TIMEOUT, |quote| {
            // The handler owns exactly one at a time and drops it before the next is built.
            live += 1;
            peak = peak.max(live);
            quoted += 1;
            assert!(!quote.is_empty());
            drop(quote);
            live -= 1;
        });
        assert_eq!(retired, opened);
        assert_eq!(quoted, opened);
        assert_eq!(peak, 1, "only one assembled packet may exist at a time");
        assert_eq!(table.table.charged, 0);
        assert!(table.table.contexts.is_empty());
    }

    #[test]
    fn an_expired_context_yields_fragment_zero_and_frees_its_bytes() {
        let packet = datagram(false, 3000);
        let pieces = fragments(&packet, 1280);
        let mut table = table();
        let now = Instant::now();
        assert_eq!(table.accept(&pieces[0], now), Ok(Accepted::Pending));
        assert_eq!(table.table.next_deadline(), Some(now + REASSEMBLY_TIMEOUT));
        assert_eq!(table.sweep(now, |_| panic!("not due yet")), 0);
        let mut expired = Vec::new();
        assert_eq!(
            table.sweep(now + REASSEMBLY_TIMEOUT, |quote| expired.push(quote)),
            1
        );
        assert_eq!(expired.len(), 1);
        // what comes back is quotable: fragment zero's own header, with the fragmentation removed
        let quote = &expired[0];
        assert_eq!(&quote[12..16], &packet[12..16]);
        assert_eq!(u16::from_be_bytes([quote[6], quote[7]]) & 0x3fff, 0);
        assert_eq!(table.table.charged, 0);
        assert!(table.table.contexts.is_empty());
        assert_eq!(table.table.next_deadline(), None);
    }

    #[test]
    fn contexts_for_different_identifications_do_not_mix() {
        let packet = datagram(false, 2000);
        let first = fragments(&packet, 1280);
        let mut second = first.clone();
        for piece in &mut second {
            piece[4..6].copy_from_slice(&0x7777u16.to_be_bytes());
        }
        let mut table = table();
        let now = Instant::now();
        assert_eq!(table.accept(&first[0], now), Ok(Accepted::Pending));
        assert_eq!(table.accept(&second[0], now), Ok(Accepted::Pending));
        assert_eq!(table.table.contexts.len(), 2);
        // and each completes on its own
        assert!(matches!(
            table.accept(&first[1], now),
            Ok(Accepted::Complete(_))
        ));
        assert_eq!(table.table.contexts.len(), 1);
    }
}
