//! The one admission owner for traffic-driven dataplane state: what may exist, in two currencies that do not
//! substitute for one another.
//!
//! Everything the Resource Policy calls a reserve, a grow, a transfer or a release passes through here.
//! Nothing else in the daemon decides whether state may exist, and nothing else mutates the totals - which is
//! the whole point, because the failure this replaces was accounting spread across owners that each believed
//! their own arithmetic.
//!
//! # Two totals, not one
//!
//! A descriptor is not a byte and neither one is spare capacity for the other. A UDP mapping costs one
//! descriptor and a few hundred bytes; a terminated TCP flow costs one descriptor and two 64 KiB buffers.
//! Counting the second as "one record" let memory run out long before descriptors did, and counting the first
//! in bytes would let a flood of forged sources exhaust the process's descriptors while the byte total said
//! there was room. So there are two totals, every reserve names both, and a request that fits one but not the
//! other is denied.
//!
//! # What the byte total is, and what it is not
//!
//! It is a conservative policy *share*, derived from the kernel's own `MemAvailable` estimate at session
//! start - not a process ceiling, not an RSS limit, and not a promise that exceeding it would fail. What it
//! counts is Rust-visible owned heap this daemon chooses the size of: the `size_of` of owner records, the
//! *capacity* of the contiguous collections they hold, the row state a bounded table is prepared for, and the
//! fixed reservations for queues and scratch that exist whether or not anything is in them.
//!
//! Two kinds of thing are outside it, for different reasons. Allocator-private metadata - arena headers, size
//! classes, per-thread caches, fragmentation - because this process cannot see it. And a hash container's own
//! indexing, because `std` documents none of it: what a requested capacity allocates, what is kept beside the
//! rows to find a key, and when the container reorganises are all its own business. Row state is charged as a
//! policy figure through [logical_footprint] and the row *count* is what is enforced; the backing those rows
//! sit in, including any temporary peak while it is rebuilt, is deliberately unquantified. A number that
//! pretended otherwise would be worse than one that says what it excludes.
//!
//! Capacity, not length, is the unit that matters. A byte charge is taken for the capacity or bound an owner
//! prepared, not for what is in it, so expiring an entry refunds the entry's *record*, frees its logical slot,
//! and refunds bytes only when the owner says an allocation really went - through [Admission::shrink] or its
//! own release. Nothing about a container's own memory is inferred either way.
//!
//! # Leases, and why they do not refund themselves
//!
//! A grant is a [Lease]: not `Clone`, carrying nothing but an identity, and inert. Only [Admission] can
//! change what a lease is charged for, and only [Admission::release] gives capacity back. Workers never
//! refund - not on cancellation, not in a `Drop`, not on the way out of an error path - because every one of
//! those runs before the thing being accounted for is actually gone. Releasing on cancellation is how a task
//! that has been *told* to stop, and has not yet stopped, hands its budget to the work that will race it.
//!
//! [Lease] therefore has no `Drop` refund at all. A lease dropped without being released leaks its capacity
//! for the rest of the session, which is fail-closed rather than fail-open, and is visible in
//! [Admission::describe] as an outstanding entry nobody released.
//!
//! # Reclaim belongs to the owner
//!
//! Nothing here evicts anything. A denial is a denial; what to do about it - process due expiry, retire the
//! requester's own oldest optional history, retire the globally oldest optional history, ask the fragment
//! owner to drop the requester's oldest non-reassembly context, and only then give up - is an ordering the
//! ingress owner knows and this module does not. Established transport state is never evicted to admit new
//! work, at any step.

use std::collections::HashMap;
use std::fmt;

/// Which side of a floor a request is on.
///
/// The two floors below - descriptors held back for DNS, bytes held back for essential work - are *inside*
/// their totals rather than subtracted from them or added to them. General work cannot reach into them;
/// DNS-class and essential-class work can. That is what makes "the DNS floor is part of the total" true
/// rather than a way of saying the total is smaller than advertised.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Class {
    /// Ordinary relayed traffic: mappings, remotes, flows, echo state.
    #[default]
    General,
    /// Work the relay cannot degrade without breaking name resolution or dropping a packet it already
    /// accepted: resolver transactions, and the headroom one maximum query plus one output packetization
    /// peak needs.
    Reserved,
}

/// One all-or-nothing request across every dimension a grant can span.
///
/// One struct rather than four calls because the atomicity is the requirement: a mapping that reserved its
/// record and then failed to reserve its bytes would have to unwind accounting that a concurrent denial may
/// already have read. Every dimension is checked, and then every dimension is charged, or nothing is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Request {
    /// Record/descriptor units.
    pub records: u32,
    /// Which floor the records are measured against.
    pub record_class: Class,
    /// Rust-visible owned heap bytes.
    pub bytes: u64,
    /// Which floor the bytes are measured against.
    pub byte_class: Class,
    /// Bytes that are additionally inside the nested reassembly cap. Never additive: these are part of
    /// [Request::bytes], and naming them here is what makes the fragment cap a *nested* check rather than a
    /// second pool.
    pub fragment_bytes: u64,
    /// Logical DNS transaction tokens, a dimension of their own because what bounds them is the platform's
    /// per-UID resolver limiter rather than anything this process owns.
    pub dns_tokens: u32,
}

impl Request {
    pub fn records(records: u32, class: Class) -> Self {
        Self {
            records,
            record_class: class,
            ..Self::default()
        }
    }

    pub fn bytes(bytes: u64, class: Class) -> Self {
        Self {
            bytes,
            byte_class: class,
            ..Self::default()
        }
    }

    /// Checked throughout: a sum that would wrap is a request that cannot be granted rather than one that
    /// silently becomes small.
    fn checked_add(self, other: Self) -> Option<Self> {
        if self.records != 0 && other.records != 0 && self.record_class != other.record_class {
            return None;
        }
        if self.bytes != 0 && other.bytes != 0 && self.byte_class != other.byte_class {
            return None;
        }
        Some(Self {
            records: self.records.checked_add(other.records)?,
            record_class: if self.records == 0 {
                other.record_class
            } else {
                self.record_class
            },
            bytes: self.bytes.checked_add(other.bytes)?,
            byte_class: if self.bytes == 0 {
                other.byte_class
            } else {
                self.byte_class
            },
            fragment_bytes: self.fragment_bytes.checked_add(other.fragment_bytes)?,
            dns_tokens: self.dns_tokens.checked_add(other.dns_tokens)?,
        })
    }

    /// The inverse, and class-checked for exactly the same reason the sum is.
    ///
    /// A subtraction naming a different class than the grant it is taken from is refused rather than applied,
    /// and that is not pedantry: [Admission::unapply] decrements the category the *request* names, while the
    /// entry it is taken from was charged to the category the *entry* names. Letting the two differ takes a
    /// general grant off the reserved ledger, so the categories drift apart from the aggregate and the floor
    /// stops meaning anything. Only dimensions that actually move are checked - subtracting zero records says
    /// nothing about which class they would have been.
    fn checked_sub(self, other: Self) -> Option<Self> {
        if other.records != 0 && self.record_class != other.record_class {
            return None;
        }
        if (other.bytes != 0 || other.fragment_bytes != 0) && self.byte_class != other.byte_class {
            return None;
        }
        Some(Self {
            records: self.records.checked_sub(other.records)?,
            record_class: self.record_class,
            bytes: self.bytes.checked_sub(other.bytes)?,
            byte_class: self.byte_class,
            fragment_bytes: self.fragment_bytes.checked_sub(other.fragment_bytes)?,
            dns_tokens: self.dns_tokens.checked_sub(other.dns_tokens)?,
        })
    }
}

/// Which dimension refused, so a denial is actionable rather than a boolean.
///
/// The owner reads this to decide what to reclaim: a byte denial and a record denial call for completely
/// different retirement, and a token denial calls for neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    Records,
    Bytes,
    FragmentBytes,
    DnsTokens,
    /// The ledger itself is full. Pre-sized from a derived bound, so this means the derivation was wrong
    /// rather than that an attacker grew it - see [Admission::new].
    Ledger,
    /// Owner identities exhausted. Fails closed rather than wrapping onto a live identity.
    Identities,
    /// Two ends of a move named the same grant. Refused rather than applied, because the second write would
    /// land on the first and leave the entry claiming more than the totals ever charged.
    Aliased,
    /// The request's own arithmetic would wrap, or it mixed two classes in one dimension.
    Arithmetic,
}

/// An accounting identity. Not `Clone`, inert, and meaningless outside the [Admission] that issued it.
///
/// Everything correctness-bearing about a grant lives in the ledger, not here, so there is exactly one place
/// that can change what is charged - and a lease that reaches the wrong `Admission`, or one that outlives its
/// entry, is refused rather than believed.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Lease {
    admission: u64,
    id: u64,
}

/// One grant, as the ledger holds it.
#[derive(Debug, Clone, Copy)]
struct Entry {
    granted: Request,
}

/// The dimensions, their floors, and every grant outstanding against them.
#[derive(Debug)]
pub struct Admission {
    /// Distinguishes leases issued by this instance from leases issued by any other.
    admission: u64,
    next_id: u64,

    /// Every record this process may hold, derived from `RLIMIT_NOFILE` less what is already open. The DNS
    /// floor is part of this, not subtracted from it.
    record_total: u32,
    /// Records only DNS-class work may reach. Inside [Admission::record_total].
    dns_record_floor: u32,
    general_records: u32,
    reserved_records: u32,

    /// The conservative session share of measurably available memory - policy, not a process ceiling.
    byte_total: u64,
    /// Bytes only essential work may reach: one maximum supported query plus one supported output
    /// packetization peak. Inside [Admission::byte_total].
    reserved_byte_floor: u64,
    general_bytes: u64,
    reserved_bytes: u64,

    /// Nested inside the byte total: what incomplete reassembly contexts may hold between them.
    fragment_cap: u64,
    fragment_bytes: u64,

    /// Logical resolver transactions in flight, bounded by the platform's per-UID limiter rather than by
    /// anything here.
    dns_token_cap: u32,
    dns_tokens: u32,

    /// Bounded by [Admission::ledger_slots] rows, which is the figure its own charge was computed from.
    /// Never grown: exhaustion is [Denied::Ledger].
    ledger: HashMap<u64, Entry>,
    ledger_slots: u32,

    peak_records: u32,
    peak_bytes: u64,
    peak_fragment_bytes: u64,
    peak_dns_tokens: u32,
    denied: u64,
    /// Releases and transfers naming an entry this admission does not hold. Counted rather than trusted, and
    /// never turned into capacity.
    invariant_violations: u64,
}

impl Admission {
    /// Builds the owner from measured totals.
    ///
    /// `ledger_slots` is derived rather than chosen: the record total bounds how many record-backed owners
    /// can exist at once, and to that are added the statically known byte-only owners - the retained tables,
    /// the fixed queues, the engine scratch - plus one slot for the single owner-confined split that may be
    /// in flight. That is the ledger's logical maximum, it is what its own bytes are charged
    /// for here, and it is the one condition [Admission::take_slot] refuses on - so no grant is ever recorded
    /// against a row the accounting did not allow for.
    ///
    /// Returns the ledger's self-charge alongside the owner, so the caller can see what the accounting cost
    /// rather than having it disappear into the number it is accounting for.
    pub fn new(totals: Totals) -> Result<Self, Misconfigured> {
        let ledger_slots = Self::ledger_slots(totals.record_total, totals.byte_only_owners).ok_or(
            Misconfigured::LedgerSlots {
                record_total: totals.record_total,
                byte_only_owners: totals.byte_only_owners,
            },
        )?;
        let ledger_bytes = Self::ledger_bytes(ledger_slots).ok_or(Misconfigured::LedgerSlots {
            record_total: totals.record_total,
            byte_only_owners: totals.byte_only_owners,
        })?;
        // Checked, never clamped. Clamping the ledger's own charge to whatever happened to be left would
        // report a configured ledger that fits while allocating one that does not, and every later denial
        // would be blamed on traffic. The floors are the caller's to size - see [Admission::ledger_bytes] -
        // so a share too small for the accounting is a configuration answered before any packet arrives.
        if totals.dns_record_floor > totals.record_total {
            return Err(Misconfigured::RecordFloor {
                floor: totals.dns_record_floor,
                total: totals.record_total,
            });
        }
        if totals.reserved_byte_floor > totals.byte_total {
            return Err(Misconfigured::ByteFloor {
                floor: totals.reserved_byte_floor,
                total: totals.byte_total,
            });
        }
        if totals.fragment_cap > totals.byte_total {
            return Err(Misconfigured::FragmentCap {
                cap: totals.fragment_cap,
                total: totals.byte_total,
            });
        }
        // Inside the reserved floor rather than merely inside the total: the ledger is not optional and it is
        // not traffic, so charging it against general bytes would quietly shrink what the relay may hold, and
        // charging it against a reserved floor sized without it would quietly shrink the essential headroom
        // the floor exists to guarantee.
        if ledger_bytes > totals.reserved_byte_floor {
            return Err(Misconfigured::Ledger {
                ledger_bytes,
                reserved_byte_floor: totals.reserved_byte_floor,
            });
        }
        let mut admission = Self {
            admission: totals.admission_id,
            next_id: 1,
            record_total: totals.record_total,
            dns_record_floor: totals.dns_record_floor,
            general_records: 0,
            reserved_records: 0,
            byte_total: totals.byte_total,
            reserved_byte_floor: totals.reserved_byte_floor,
            general_bytes: 0,
            reserved_bytes: 0,
            fragment_cap: totals.fragment_cap,
            fragment_bytes: 0,
            dns_token_cap: totals.dns_token_cap,
            dns_tokens: 0,
            // Requested at its logical maximum, the same number [Admission::ledger_bytes] charged rows for,
            // so the common case allocates nothing. An initial reservation rather than a bound the container
            // owes anything to: it may allocate or reorganise its own backing whenever it likes, which is
            // count-bounded overhead here rather than accounted state.
            ledger: HashMap::with_capacity(ledger_slots as usize),
            ledger_slots,
            peak_records: 0,
            peak_bytes: 0,
            peak_fragment_bytes: 0,
            peak_dns_tokens: 0,
            denied: 0,
            invariant_violations: 0,
        };
        // The ledger's own capacity, charged before anything can be admitted against it. Reserved-class,
        // because it is not optional: the accounting has to exist for the traffic to be accounted for.
        admission.reserved_bytes = ledger_bytes;
        admission.peak_bytes = ledger_bytes;
        Ok(admission)
    }

    /// How many rows the ledger is built for: one per record-backed owner that can exist at once, plus the
    /// statically known byte-only owners, plus one for the single owner-confined split in flight. `None` when
    /// that sum does not fit, which is a configuration this cannot record and therefore
    /// must not accept.
    pub fn ledger_slots(record_total: u32, byte_only_owners: u32) -> Option<u32> {
        record_total.checked_add(byte_only_owners)?.checked_add(1)
    }

    /// What a ledger of that many rows costs, so the caller sizing the reserved floor charges the same number
    /// this will.
    ///
    /// Through the same figure as every other collection it accounts for: the rows themselves, at the size of
    /// what the map really stores. The ledger is a `HashMap<u64, Entry>`, so its rows are `(u64, Entry)` -
    /// and, like every other map here, whatever the container keeps beside those rows is count-bounded rather
    /// than charged. See [logical_footprint].
    pub fn ledger_bytes(ledger_slots: u32) -> Option<u64> {
        logical_footprint::<(u64, Entry)>(ledger_slots as usize)
    }

    /// Every record, including the DNS floor. The number a total is honestly called.
    pub fn record_total(&self) -> u32 {
        self.record_total
    }

    /// What general work may reach: the total less the floor DNS keeps inside it. Smaller than the total, and
    /// deliberately not called one.
    pub fn general_record_ceiling(&self) -> u32 {
        self.record_total.saturating_sub(self.dns_record_floor)
    }

    pub fn dns_record_floor(&self) -> u32 {
        self.dns_record_floor
    }

    pub fn byte_total(&self) -> u64 {
        self.byte_total
    }

    /// What general work may still take, in both currencies at once.
    ///
    /// The number a prepared bound has to be solved against, and not one a caller can reconstruct correctly
    /// from the totals: "total minus charged" counts the reserved floor as though general work could reach
    /// it, so a bound derived that way inflates ordinary traffic's share with capacity that is protected for
    /// name resolution and for packets already accepted. What is left for general work is its own ceiling
    /// less what general work has already taken, and nothing else.
    pub fn general_headroom(&self) -> Headroom {
        Headroom {
            records: self
                .general_record_ceiling()
                .saturating_sub(self.general_records),
            bytes: self
                .general_byte_ceiling()
                .saturating_sub(self.general_bytes),
        }
    }

    pub fn general_byte_ceiling(&self) -> u64 {
        self.byte_total.saturating_sub(self.reserved_byte_floor)
    }

    pub fn records_charged(&self) -> u32 {
        self.general_records + self.reserved_records
    }

    pub fn bytes_charged(&self) -> u64 {
        self.general_bytes + self.reserved_bytes
    }

    pub fn fragment_bytes_charged(&self) -> u64 {
        self.fragment_bytes
    }

    /// How many logical resolver transactions may be in flight, which is also how many a table sized against
    /// it needs room for.
    pub fn dns_token_cap(&self) -> u32 {
        self.dns_token_cap
    }

    pub fn dns_tokens_charged(&self) -> u32 {
        self.dns_tokens
    }

    pub fn outstanding_leases(&self) -> usize {
        self.ledger.len()
    }

    pub fn invariant_violations(&self) -> u64 {
        self.invariant_violations
    }

    pub fn denials(&self) -> u64 {
        self.denied
    }

    /// Grants everything or nothing.
    ///
    /// Every dimension is checked before any is charged, so a request that fails leaves usage exactly as it
    /// was - which is what lets an owner treat a denial as a fact about capacity rather than as something it
    /// has to undo.
    pub fn reserve(&mut self, request: Request) -> Result<Lease, Denied> {
        self.check_capacity(request)?;
        let id = self.take_slot()?;
        self.apply(request);
        let lease = Lease {
            admission: self.admission,
            id,
        };
        self.ledger.insert(id, Entry { granted: request });
        Ok(lease)
    }

    /// Reserves a row and the identity that names it, for a grant that needs one of its own.
    ///
    /// Separate from [Admission::check_capacity] because growing an existing grant needs the capacity check
    /// and no row: a ledger that is full must not be able to refuse a resize of something it is already
    /// recording, and a resize must not consume an identity it will never use.
    ///
    /// One condition: this many rows and no more. [Admission::ledger_slots] is the logical maximum, it is what
    /// [Admission::ledger_bytes] charged row state for, and a row released gives its slot straight back to the
    /// next caller.
    ///
    /// The map's own `capacity()` is deliberately not consulted. Hash backing is opaque count-bounded overhead
    /// under this policy rather than byte-attributed state, so a reorganisation inside the container is not
    /// something the accounting has an opinion about - and `capacity()` is only a current lower bound anyway,
    /// so gating on it bought nothing but refusals a well-behaved caller had not earned.
    fn take_slot(&mut self) -> Result<u64, Denied> {
        if self.ledger.len() as u32 >= self.ledger_slots {
            self.denied += 1;
            return Err(Denied::Ledger);
        }
        let Some(next) = self.next_id.checked_add(1) else {
            self.denied += 1;
            return Err(Denied::Identities);
        };
        let id = self.next_id;
        self.next_id = next;
        Ok(id)
    }

    /// Checks every dimension against the totals, the floors and the nested caps, mutating no usage.
    fn check_capacity(&mut self, request: Request) -> Result<(), Denied> {
        let refuse = |admission: &mut Self, why: Denied| {
            admission.denied += 1;
            Err(why)
        };
        // Records: general work stops at the floor, reserved work may enter it, and neither may pass the
        // total.
        let Some(records) = self.records_charged().checked_add(request.records) else {
            return refuse(self, Denied::Arithmetic);
        };
        if records > self.record_total {
            return refuse(self, Denied::Records);
        }
        if request.record_class == Class::General {
            let Some(general) = self.general_records.checked_add(request.records) else {
                return refuse(self, Denied::Arithmetic);
            };
            if general > self.general_record_ceiling() {
                return refuse(self, Denied::Records);
            }
        }
        let Some(bytes) = self.bytes_charged().checked_add(request.bytes) else {
            return refuse(self, Denied::Arithmetic);
        };
        if bytes > self.byte_total {
            return refuse(self, Denied::Bytes);
        }
        if request.byte_class == Class::General {
            let Some(general) = self.general_bytes.checked_add(request.bytes) else {
                return refuse(self, Denied::Arithmetic);
            };
            if general > self.general_byte_ceiling() {
                return refuse(self, Denied::Bytes);
            }
        }
        // Nested, not additive: these bytes were already counted above.
        if request.fragment_bytes > request.bytes {
            return refuse(self, Denied::Arithmetic);
        }
        let Some(fragment) = self.fragment_bytes.checked_add(request.fragment_bytes) else {
            return refuse(self, Denied::Arithmetic);
        };
        if fragment > self.fragment_cap {
            return refuse(self, Denied::FragmentBytes);
        }
        let Some(tokens) = self.dns_tokens.checked_add(request.dns_tokens) else {
            return refuse(self, Denied::Arithmetic);
        };
        if tokens > self.dns_token_cap {
            return refuse(self, Denied::DnsTokens);
        }
        Ok(())
    }

    /// Charges a request that [Admission::check] has already proven fits.
    fn apply(&mut self, request: Request) {
        match request.record_class {
            Class::General => self.general_records += request.records,
            Class::Reserved => self.reserved_records += request.records,
        }
        match request.byte_class {
            Class::General => self.general_bytes += request.bytes,
            Class::Reserved => self.reserved_bytes += request.bytes,
        }
        self.fragment_bytes += request.fragment_bytes;
        self.dns_tokens += request.dns_tokens;
        self.peak_records = self.peak_records.max(self.records_charged());
        self.peak_bytes = self.peak_bytes.max(self.bytes_charged());
        self.peak_fragment_bytes = self.peak_fragment_bytes.max(self.fragment_bytes);
        self.peak_dns_tokens = self.peak_dns_tokens.max(self.dns_tokens);
    }

    /// Uncharges a request the ledger says is outstanding.
    fn unapply(&mut self, request: Request) {
        match request.record_class {
            Class::General => {
                self.general_records = self.general_records.saturating_sub(request.records)
            }
            Class::Reserved => {
                self.reserved_records = self.reserved_records.saturating_sub(request.records)
            }
        }
        match request.byte_class {
            Class::General => self.general_bytes = self.general_bytes.saturating_sub(request.bytes),
            Class::Reserved => {
                self.reserved_bytes = self.reserved_bytes.saturating_sub(request.bytes)
            }
        }
        self.fragment_bytes = self.fragment_bytes.saturating_sub(request.fragment_bytes);
        self.dns_tokens = self.dns_tokens.saturating_sub(request.dns_tokens);
    }

    /// Adds to an existing grant, atomically and checked. Usage is unchanged if it does not fit.
    ///
    /// Extends the row the lease already owns rather than adding one, so it needs no ledger slot and no
    /// identity: a full ledger cannot refuse a resize of something the ledger is already recording, and a
    /// resize cannot consume an identity nothing will ever name. The delta is checked as a fresh request
    /// would be, so the floors, the nested caps and the class agreement see exactly the same arithmetic.
    pub fn grow(&mut self, lease: &Lease, more: Request) -> Result<(), Denied> {
        let Some(entry) = self.entry(lease) else {
            self.invariant_violations += 1;
            return Err(Denied::Ledger);
        };
        // Class agreement first: a growth in a class the row was not charged in would leave the category
        // ledgers describing something the row does not.
        let Some(grown) = entry.granted.checked_add(more) else {
            self.denied += 1;
            return Err(Denied::Arithmetic);
        };
        self.check_capacity(more)?;
        self.apply(more);
        // Replaces the value under a key the map already holds, so no row is added and nothing has to grow.
        self.ledger.insert(lease.id, Entry { granted: grown });
        Ok(())
    }

    /// Gives back part of a grant, and only once the underlying capacity has actually shrunk or dropped.
    ///
    /// The caller's ordering, not this module's: shrinking here while the allocation is still held would be
    /// the same lie as refunding on cancellation.
    pub fn shrink(&mut self, lease: &Lease, less: Request) {
        let Some(entry) = self.entry(lease) else {
            self.invariant_violations += 1;
            return;
        };
        let Some(remaining) = entry.granted.checked_sub(less) else {
            // More than was ever granted. Counted, and no capacity invented.
            self.invariant_violations += 1;
            return;
        };
        self.unapply(less);
        self.ledger.insert(lease.id, Entry { granted: remaining });
    }

    /// Moves accounting from one live grant to another. Totals are unchanged; only who owes what moves.
    ///
    /// For real ownership moves and nothing else - a closed TCP-DNS transport handing its logical token to the
    /// query still in flight, a completed resolver handing its result bytes to whoever will frame them. Using
    /// it as a refund-and-reserve would be two lies that happen to cancel.
    pub fn transfer(&mut self, from: &Lease, to: &Lease, moved: Request) -> Result<(), Denied> {
        // Both ends the same row is refused before anything is read, not merely before anything is written.
        // The two writes below would otherwise land on one key, the second overwriting the first, and the row
        // would end up claiming the moved amount twice while the totals were charged once - so releasing it
        // later would hand back capacity that was never taken. Nothing about a grant moving to itself is a
        // real ownership move, so there is no correct amount to apply.
        if from.id == to.id {
            self.invariant_violations += 1;
            return Err(Denied::Aliased);
        }
        let (Some(source), Some(target)) = (self.entry(from), self.entry(to)) else {
            self.invariant_violations += 1;
            return Err(Denied::Ledger);
        };
        let Some(remaining) = source.granted.checked_sub(moved) else {
            self.invariant_violations += 1;
            return Err(Denied::Arithmetic);
        };
        let Some(grown) = target.granted.checked_add(moved) else {
            self.invariant_violations += 1;
            return Err(Denied::Arithmetic);
        };
        self.ledger.insert(from.id, Entry { granted: remaining });
        self.ledger.insert(to.id, Entry { granted: grown });
        Ok(())
    }

    /// Splits part of a grant into a lease of its own, consuming one pre-reserved ledger slot.
    ///
    /// Totals are unchanged. Slot exhaustion leaves usage exactly as it was.
    pub fn split(&mut self, from: &Lease, taken: Request) -> Result<Lease, Denied> {
        let Some(source) = self.entry(from) else {
            self.invariant_violations += 1;
            return Err(Denied::Ledger);
        };
        let Some(remaining) = source.granted.checked_sub(taken) else {
            self.invariant_violations += 1;
            return Err(Denied::Arithmetic);
        };
        let id = self.take_slot()?;
        self.ledger.insert(from.id, Entry { granted: remaining });
        self.ledger.insert(id, Entry { granted: taken });
        Ok(Lease {
            admission: self.admission,
            id,
        })
    }

    /// Gives a grant back, and only after everything it accounted for is actually gone: the task retired, the
    /// record erased, the allocation dropped.
    ///
    /// Consuming the lease is what makes a double release unrepresentable. A *stale* one - a lease from
    /// another admission, or one whose row is already gone - is counted as an invariant violation and creates
    /// no capacity.
    pub fn release(&mut self, lease: Lease) {
        if lease.admission != self.admission {
            self.invariant_violations += 1;
            return;
        }
        match self.ledger.remove(&lease.id) {
            Some(entry) => self.unapply(entry.granted),
            None => self.invariant_violations += 1,
        }
    }

    fn entry(&mut self, lease: &Lease) -> Option<Entry> {
        if lease.admission != self.admission {
            return None;
        }
        self.ledger.get(&lease.id).copied()
    }

    /// What a grant is currently charged, for a diagnostic or a test. `None` for a lease this admission does
    /// not hold.
    pub fn granted(&self, lease: &Lease) -> Option<Request> {
        if lease.admission != self.admission {
            return None;
        }
        self.ledger.get(&lease.id).map(|entry| entry.granted)
    }

    /// The line a session prints on the way out. Outstanding leases are the leak report: a lease dropped
    /// without release keeps its row, so a nonzero count here names capacity nothing gave back.
    pub fn describe(&self) -> String {
        format!(
            "{} of {} records ({} general of {}, {} reserved, floor {}), peak {}; \
             {} of {} bytes ({} general of {}, {} reserved), peak {}; \
             {} of {} fragment bytes, peak {}; {} of {} dns tokens, peak {}; \
             {} leases outstanding of {} slots; {} denied; {} invariant violations",
            self.records_charged(),
            self.record_total,
            self.general_records,
            self.general_record_ceiling(),
            self.reserved_records,
            self.dns_record_floor,
            self.peak_records,
            self.bytes_charged(),
            self.byte_total,
            self.general_bytes,
            self.general_byte_ceiling(),
            self.reserved_bytes,
            self.peak_bytes,
            self.fragment_bytes,
            self.fragment_cap,
            self.peak_fragment_bytes,
            self.dns_tokens,
            self.dns_token_cap,
            self.peak_dns_tokens,
            self.ledger.len(),
            self.ledger_slots,
            self.denied,
            self.invariant_violations,
        )
    }
}

/// What the platform measured, handed over once so that policy here and measurement there cannot disagree
/// about it.
#[derive(Debug, Clone, Copy)]
pub struct Totals {
    /// Distinguishes this admission's leases from any other's.
    pub admission_id: u64,
    /// `RLIMIT_NOFILE` less already-open descriptors, less whatever cleanup provably needs. The DNS floor is
    /// inside this.
    pub record_total: u32,
    /// Records only DNS-class work may reach, inside [Totals::record_total].
    pub dns_record_floor: u32,
    /// The conservative session share of measurably available memory.
    pub byte_total: u64,
    /// Bytes only essential work may reach, inside [Totals::byte_total].
    pub reserved_byte_floor: u64,
    /// Nested inside [Totals::byte_total].
    pub fragment_cap: u64,
    pub dns_token_cap: u32,
    /// Owners that hold bytes and no record: retained tables, fixed queues, engine scratch. Used only to
    /// derive the ledger's own size.
    pub byte_only_owners: u32,
}

/// The largest prepared count whose per-item cost and its own tables both fit what is left.
///
/// The alternative this replaces is a constant, and a constant is wrong in both directions at once: too large
/// on a device whose measured share cannot hold that many, so tables are charged for capacity nothing can
/// ever reach, and too small on one that could hold more. Solving for it makes the bound a fact about the
/// device rather than a number someone picked.
///
/// `per_item` is what one admitted item costs on its own - a TCP flow's two stack buffers, say - and `tables`
/// is what the retained collections indexing `n` of them cost together. Both are counted, because preparing
/// tables consumes the very bytes the items would need: a bound that ignored its own tables would prepare for
/// a count the byte total could not then admit.
///
/// Monotone in `n`, so a binary search finds the exact largest fit. Answers zero when not even one fits,
/// which is a dataplane that can carry no flows rather than one that pretends it can.
pub fn largest_fitting(
    headroom: Headroom,
    per_item: u64,
    tables: impl Fn(usize) -> Option<u64>,
) -> usize {
    let Headroom { records, bytes } = headroom;
    let fits = |n: usize| -> bool {
        let Some(items) = u64::try_from(n).ok().and_then(|n| n.checked_mul(per_item)) else {
            return false;
        };
        let Some(tables) = tables(n) else {
            return false;
        };
        items
            .checked_add(tables)
            .is_some_and(|total| total <= bytes)
    };
    let ceiling = records as usize;
    if !fits(1) {
        return 0;
    }
    let (mut low, mut high) = (1usize, ceiling);
    if fits(high) {
        return high;
    }
    // Invariant: `low` fits and `high` does not, so the answer is `low` when they meet.
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if fits(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    low
}

/// What general-class work may still take. Answered as a pair because a prepared bound has to fit both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headroom {
    pub records: u32,
    pub bytes: u64,
}

/// What one collection's worth of `entries` rows of `Row` logically costs.
///
/// The row state and nothing else: `entries * size_of::<Row>()`, where `Row` is what the collection really
/// stores - `(K, V)` for a `HashMap<K, V>`, `T` for a `HashSet<T>`. Naming the type rather than passing a byte
/// count is deliberate, because a sum of two field sizes is not the size of the pair: `HashMap<IpAddr,
/// Instant>` in the UDP relay's reply filter sums to 33 where `(IpAddr, Instant)` is 40.
///
/// **What this is not.** It is not the allocation a `std` hash container makes for those rows. How many slots
/// it allocates for a requested capacity, what it keeps beside them to find a key, and when it reorganises are
/// all the container's own business, and `std` documents none of it - so no figure here could be honest about
/// it, and none is offered. Container backing is therefore *count-bounded* rather than byte-charged, exactly
/// like the runtime cells in the aggregate's other opaque category: what the daemon states is how many rows
/// can exist, which it enforces, and what those rows cost as state, which it can compute.
///
/// Which is what makes the admission rule as simple as it is. Every long-lived table has an explicit logical
/// maximum, that maximum is the only thing a new row is refused on, and a row removed frees a slot the next
/// caller may have. `with_capacity(maximum)` is how each one is built, but as an ordinary initial reservation
/// so the common case allocates nothing - not as an oracle: the container may reorganise its backing whenever
/// it likes, including while rows are being replaced, and nothing here consults `HashMap::capacity` to decide
/// whether an insertion is allowed.
///
/// The consequence is stated rather than hidden. The byte total is not process RSS; for a table-shaped owner
/// the real allocation exceeds the charge by whatever that overhead is, and a temporary rebuild peak inside a
/// container is outside the model too. What the daemon promises is the cardinality bound, which is what makes
/// clients bounded rather than unbounded - it does not promise anything about how a container behaves under
/// pathological insert/remove patterns, because clients here are apps on the same device and are assumed
/// reasonably well behaved rather than adversarial.
///
/// Contiguous collections are different and stay byte-charged in full - see [linear_footprint] - because
/// `Vec`'s documented guarantee is one allocation of exactly `capacity` elements.
///
/// `None` when the arithmetic would wrap, which is a capacity that cannot be accounted for and therefore must
/// not be prepared.
pub fn logical_footprint<Row>(entries: usize) -> Option<u64> {
    u64::try_from(entries)
        .ok()?
        .checked_mul(std::mem::size_of::<Row>() as u64)
}

/// A conservative upper bound on a contiguous collection prepared for `slots` of `slot_bytes`.
///
/// Doubled, and unlike [logical_footprint] this really is the allocation: `Vec` guarantees one contiguous
/// block of exactly `capacity` elements, so the only slack to cover is that `Vec` and `VecDeque` may round a
/// requested capacity up and a growth doubles. `None` when the arithmetic would wrap.
pub fn linear_footprint(slots: usize, slot_bytes: u64) -> Option<u64> {
    u64::try_from(slots)
        .ok()?
        .checked_mul(slot_bytes)?
        .checked_mul(2)
}

/// A set of totals the accounting cannot honestly record, answered at construction rather than as a denial
/// once traffic is flowing.
///
/// Every one of these is a sizing mistake in the measurement that produced [Totals], and every one of them
/// would otherwise show up as a denial blamed on traffic. Construction failing is what keeps the daemon from
/// starting a dataplane whose accounting does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Misconfigured {
    /// The derived row count, or its byte cost, does not fit its own arithmetic.
    LedgerSlots {
        record_total: u32,
        byte_only_owners: u32,
    },
    /// The ledger costs more than the floor that has to hold it.
    Ledger {
        ledger_bytes: u64,
        reserved_byte_floor: u64,
    },
    RecordFloor {
        floor: u32,
        total: u32,
    },
    ByteFloor {
        floor: u64,
        total: u64,
    },
    FragmentCap {
        cap: u64,
        total: u64,
    },
}

impl fmt::Display for Misconfigured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LedgerSlots {
                record_total,
                byte_only_owners,
            } => write!(
                f,
                "a ledger for {record_total} records and {byte_only_owners} byte-only owners does not fit"
            ),
            Self::Ledger {
                ledger_bytes,
                reserved_byte_floor,
            } => write!(
                f,
                "the admission ledger needs {ledger_bytes} bytes and the reserved floor is \
                 {reserved_byte_floor}"
            ),
            Self::RecordFloor { floor, total } => {
                write!(f, "a DNS floor of {floor} records is not inside a total of {total}")
            }
            Self::ByteFloor { floor, total } => {
                write!(f, "a reserved floor of {floor} bytes is not inside a total of {total}")
            }
            Self::FragmentCap { cap, total } => {
                write!(f, "a fragment cap of {cap} bytes is not inside a total of {total}")
            }
        }
    }
}

impl std::error::Error for Misconfigured {}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::IpAddr;
    use std::time::Instant;

    /// A row cost is the rows times what one row is, and it refuses rather than wrapping.
    ///
    /// What it deliberately does *not* claim is what a `std` hash container allocates for those rows: that is
    /// count-bounded, so the figure below is smaller than the real allocation by whatever the container spends
    /// on its own indexing. The pair matters, though, and is the one arithmetic mistake this signature exists
    /// to prevent - `size_of::<K>() + size_of::<V>()` is not `size_of::<(K, V)>()` when the pair has padding
    /// between its fields.
    #[test]
    fn a_logical_row_cost_is_the_rows_times_one_row() {
        assert_eq!(logical_footprint::<(u64, u64)>(0), Some(0));
        assert_eq!(logical_footprint::<(u64, u64)>(1_000), Some(16_000));
        // The UDP relay's reply filter, where a sum of the two field sizes would lose seven bytes a row.
        let pair = std::mem::size_of::<(IpAddr, Instant)>() as u64;
        assert!(
            pair > std::mem::size_of::<IpAddr>() as u64 + std::mem::size_of::<Instant>() as u64 - 8
        );
        assert_eq!(logical_footprint::<(IpAddr, Instant)>(64), Some(64 * pair));
        // Monotone, which is what the solvers walk over.
        assert!(logical_footprint::<(u64, u64)>(11) > logical_footprint::<(u64, u64)>(10));
        // And a count whose cost cannot be stated is refused rather than wrapped.
        assert_eq!(logical_footprint::<[u8; 1024]>(usize::MAX), None);
    }

    fn totals() -> Totals {
        Totals {
            admission_id: 1,
            record_total: 100,
            dns_record_floor: 32,
            // Large enough that the ledger's own charge is a rounding error rather than the whole total,
            // which is the production shape: the byte total is a share of measured memory.
            byte_total: 1_000_000,
            reserved_byte_floor: 200_000,
            fragment_cap: 4_000,
            dns_token_cap: 32,
            byte_only_owners: 8,
        }
    }

    /// The ledger charges itself before anything else can be admitted, so the byte total already reflects it.
    fn admission() -> Admission {
        Admission::new(totals()).expect("the fixture totals hold their own accounting")
    }

    fn general(records: u32, bytes: u64) -> Request {
        Request {
            records,
            record_class: Class::General,
            bytes,
            byte_class: Class::General,
            ..Request::default()
        }
    }

    #[test]
    fn a_request_is_granted_in_every_dimension_or_in_none() {
        let mut admission = admission();
        let before = (
            admission.records_charged(),
            admission.bytes_charged(),
            admission.fragment_bytes_charged(),
            admission.dns_tokens_charged(),
        );
        // The bytes fit and the records do not: nothing is charged, in any dimension.
        let denied = admission
            .reserve(general(1_000, 10))
            .expect_err("a request over the record ceiling must be denied");
        assert_eq!(denied, Denied::Records);
        assert_eq!(
            before,
            (
                admission.records_charged(),
                admission.bytes_charged(),
                admission.fragment_bytes_charged(),
                admission.dns_tokens_charged()
            )
        );
        // The records fit and the bytes do not: likewise.
        assert_eq!(
            admission.reserve(general(1, 10_000_000)),
            Err(Denied::Bytes)
        );
        assert_eq!(
            before,
            (
                admission.records_charged(),
                admission.bytes_charged(),
                admission.fragment_bytes_charged(),
                admission.dns_tokens_charged()
            )
        );
        // And when every dimension fits, every dimension is charged at once.
        let lease = admission
            .reserve(Request {
                records: 2,
                record_class: Class::General,
                bytes: 500,
                byte_class: Class::General,
                fragment_bytes: 100,
                dns_tokens: 1,
            })
            .expect("a request within every dimension is granted");
        assert_eq!(admission.records_charged(), before.0 + 2);
        assert_eq!(admission.bytes_charged(), before.1 + 500);
        assert_eq!(admission.fragment_bytes_charged(), 100);
        assert_eq!(admission.dns_tokens_charged(), 1);
        admission.release(lease);
    }

    /// Arithmetic that would wrap is a denial, not a small number.
    #[test]
    fn arithmetic_that_would_wrap_leaves_usage_unchanged() {
        let mut admission = admission();
        let lease = admission.reserve(general(1, 10)).expect("granted");
        let charged = (admission.records_charged(), admission.bytes_charged());
        assert_eq!(
            admission.grow(&lease, Request::records(u32::MAX, Class::General)),
            Err(Denied::Arithmetic)
        );
        assert_eq!(
            admission.grow(&lease, Request::bytes(u64::MAX, Class::General)),
            Err(Denied::Arithmetic)
        );
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            charged
        );
        admission.release(lease);
    }

    /// The floor is inside the total: general work cannot reach it, DNS work can, and neither passes the
    /// total. Which is what makes the total an honest total and the general ceiling a smaller, separate
    /// number.
    #[test]
    fn the_dns_floor_is_inside_the_total_and_only_dns_may_enter_it() {
        let mut admission = admission();
        assert_eq!(admission.record_total(), 100);
        assert_eq!(admission.general_record_ceiling(), 68);
        assert_eq!(admission.dns_record_floor(), 32);

        let general_fill = admission
            .reserve(Request::records(68, Class::General))
            .expect("general work fills its ceiling");
        // One more general record would enter the floor.
        assert_eq!(
            admission.reserve(Request::records(1, Class::General)),
            Err(Denied::Records)
        );
        // DNS-class work may, and exactly to the total and no further.
        let dns = admission
            .reserve(Request::records(32, Class::Reserved))
            .expect("DNS work may use the floor");
        assert_eq!(admission.records_charged(), 100);
        assert_eq!(
            admission.reserve(Request::records(1, Class::Reserved)),
            Err(Denied::Records)
        );
        admission.release(general_fill);
        admission.release(dns);
        assert_eq!(admission.records_charged(), 0);
    }

    /// The fragment cap is a nested check inside the byte total, not a pool beside it.
    #[test]
    fn fragment_bytes_are_capped_within_the_aggregate() {
        let mut admission = admission();
        let lease = admission
            .reserve(Request {
                bytes: 4_000,
                byte_class: Class::General,
                fragment_bytes: 4_000,
                ..Request::default()
            })
            .expect("the fragment cap is reachable");
        // The nested cap refuses before the aggregate would.
        assert_eq!(
            admission.reserve(Request {
                bytes: 1,
                byte_class: Class::General,
                fragment_bytes: 1,
                ..Request::default()
            }),
            Err(Denied::FragmentBytes)
        );
        // And fragment bytes were counted in the aggregate rather than beside it.
        assert!(admission.bytes_charged() >= 4_000);
        // A request naming more fragment bytes than bytes is incoherent rather than generous.
        assert_eq!(
            admission.reserve(Request {
                bytes: 1,
                byte_class: Class::General,
                fragment_bytes: 2,
                ..Request::default()
            }),
            Err(Denied::Arithmetic)
        );
        admission.release(lease);
        assert_eq!(admission.fragment_bytes_charged(), 0);
    }

    /// Expiring a record does not refund the bytes the collection it lived in still owns. Shrinking is the
    /// caller's word that the allocation really went.
    #[test]
    fn retained_collection_capacity_keeps_its_charge() {
        let mut admission = admission();
        // One table owner: its retained capacity, charged once.
        let table = admission
            .reserve(Request::bytes(1_024, Class::General))
            .expect("granted");
        // One semantic record living in it.
        let entry = admission
            .reserve(Request::records(1, Class::General))
            .expect("granted");
        let bytes = admission.bytes_charged();
        // The entry expires: its record goes, the table's retained capacity does not.
        admission.release(entry);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.bytes_charged(), bytes);
        // Only an actual shrink gives bytes back.
        admission.shrink(&table, Request::bytes(512, Class::General));
        assert_eq!(admission.bytes_charged(), bytes - 512);
        admission.release(table);
    }

    /// A release naming a row this admission does not hold is counted and creates nothing.
    #[test]
    fn a_stale_release_is_counted_and_creates_no_capacity() {
        let mut admission = admission();
        let mut other = Admission::new(Totals {
            admission_id: 2,
            ..totals()
        })
        .expect("granted");
        let foreign = other.reserve(general(1, 10)).expect("granted");
        let charged = (admission.records_charged(), admission.bytes_charged());
        admission.release(foreign);
        assert_eq!(admission.invariant_violations(), 1);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            charged
        );

        // And a lease whose row is gone - which only a bug can produce, since release consumes it - is
        // likewise counted rather than believed.
        let lease = admission.reserve(general(1, 10)).expect("granted");
        let ghost = Lease {
            admission: lease.admission,
            id: lease.id,
        };
        admission.release(lease);
        let charged = (admission.records_charged(), admission.bytes_charged());
        admission.release(ghost);
        assert_eq!(admission.invariant_violations(), 2);
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            charged
        );
        // A shrink against a missing row is the same.
        let honest = other.reserve(general(1, 10)).expect("granted");
        other.release(honest);
        assert_eq!(other.invariant_violations(), 0);
    }

    /// A UDP mapping's first send is one grant covering the mapping and its first remote. Either both records
    /// exist or neither does.
    #[test]
    fn a_composite_two_record_reserve_is_atomic() {
        let mut admission = Admission::new(Totals {
            record_total: 33,
            dns_record_floor: 32,
            ..totals()
        })
        .expect("granted");
        // Room for exactly one general record, and the mapping needs two.
        assert_eq!(admission.general_record_ceiling(), 1);
        assert_eq!(admission.reserve(general(2, 10)), Err(Denied::Records));
        assert_eq!(admission.records_charged(), 0);
        // One fits, and then the pair does not.
        let one = admission.reserve(general(1, 10)).expect("granted");
        assert_eq!(admission.reserve(general(2, 10)), Err(Denied::Records));
        admission.release(one);
    }

    /// Every dimension returns to zero when every grant is released, and the class sums are the totals.
    #[test]
    fn full_release_zeroes_both_totals_and_the_class_sums_agree() {
        let mut admission = admission();
        let ledger_self_charge = admission.bytes_charged();
        let mut leases = Vec::new();
        for _ in 0..5 {
            leases.push(
                admission
                    .reserve(Request {
                        records: 1,
                        record_class: Class::General,
                        bytes: 100,
                        byte_class: Class::General,
                        fragment_bytes: 10,
                        dns_tokens: 0,
                    })
                    .expect("granted"),
            );
        }
        leases.push(
            admission
                .reserve(Request {
                    records: 2,
                    record_class: Class::Reserved,
                    bytes: 50,
                    byte_class: Class::Reserved,
                    fragment_bytes: 0,
                    dns_tokens: 3,
                })
                .expect("granted"),
        );
        // The class sums are the totals, by construction rather than by coincidence.
        assert_eq!(
            admission.general_records + admission.reserved_records,
            admission.records_charged()
        );
        assert_eq!(
            admission.general_bytes + admission.reserved_bytes,
            admission.bytes_charged()
        );
        assert_eq!(admission.records_charged(), 7);
        assert_eq!(admission.dns_tokens_charged(), 3);
        assert_eq!(admission.fragment_bytes_charged(), 50);

        for lease in leases {
            admission.release(lease);
        }
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.general_records, 0);
        assert_eq!(admission.reserved_records, 0);
        // The ledger's own charge is what remains, and it is not traffic.
        assert_eq!(admission.bytes_charged(), ledger_self_charge);
        assert_eq!(admission.fragment_bytes_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.outstanding_leases(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The logical token dimension is its own: exhausting it denies the token and nothing else.
    #[test]
    fn dns_tokens_are_their_own_dimension() {
        let mut admission = admission();
        let mut held = Vec::new();
        for _ in 0..32 {
            held.push(
                admission
                    .reserve(Request {
                        dns_tokens: 1,
                        ..Request::default()
                    })
                    .expect("granted"),
            );
        }
        assert_eq!(
            admission.reserve(Request {
                dns_tokens: 1,
                ..Request::default()
            }),
            Err(Denied::DnsTokens)
        );
        // Records and bytes are untouched by a token denial.
        let ordinary = admission.reserve(general(1, 10)).expect("granted");
        admission.release(ordinary);
        for lease in held {
            admission.release(lease);
        }
        assert_eq!(admission.dns_tokens_charged(), 0);
    }

    /// A transfer moves who owes what without changing what is owed - which is what a closed TCP-DNS
    /// transport handing its token to the query still in flight needs, and what a refund-and-reserve would
    /// get wrong by briefly making capacity available.
    #[test]
    fn a_transfer_moves_ownership_without_changing_totals() {
        let mut admission = admission();
        let transport = admission
            .reserve(Request {
                records: 1,
                record_class: Class::General,
                dns_tokens: 1,
                ..Request::default()
            })
            .expect("granted");
        let debt = admission
            .reserve(Request::records(1, Class::Reserved))
            .expect("granted");
        let before = (admission.records_charged(), admission.dns_tokens_charged());
        admission
            .transfer(
                &transport,
                &debt,
                Request {
                    dns_tokens: 1,
                    ..Request::default()
                },
            )
            .expect("the token moves");
        assert_eq!(
            (admission.records_charged(), admission.dns_tokens_charged()),
            before
        );
        assert_eq!(admission.granted(&transport).expect("held").dns_tokens, 0);
        assert_eq!(admission.granted(&debt).expect("held").dns_tokens, 1);
        // Releasing the transport now cannot take the token with it.
        admission.release(transport);
        assert_eq!(admission.dns_tokens_charged(), 1);
        admission.release(debt);
        assert_eq!(admission.dns_tokens_charged(), 0);
    }

    /// A split takes a pre-reserved slot; exhausting slots leaves usage unchanged rather than growing an
    /// attacker-controlled ledger.
    #[test]
    fn ledger_slot_exhaustion_fails_closed() {
        let mut admission = Admission::new(Totals {
            record_total: 2,
            dns_record_floor: 0,
            byte_only_owners: 0,
            ..totals()
        })
        .expect("granted");
        // Two records plus one transaction slot.
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(
                admission
                    .reserve(Request::bytes(1, Class::General))
                    .expect("granted"),
            );
        }
        let charged = admission.bytes_charged();
        assert_eq!(
            admission.reserve(Request::bytes(1, Class::General)),
            Err(Denied::Ledger)
        );
        assert_eq!(admission.bytes_charged(), charged);
        // A split needs a slot too, and there is none.
        assert_eq!(
            admission.split(&held[0], Request::bytes(1, Class::General)),
            Err(Denied::Ledger)
        );
        assert_eq!(admission.bytes_charged(), charged);
        for lease in held {
            admission.release(lease);
        }
    }

    /// Identities fail closed at exhaustion rather than wrapping onto a live one.
    #[test]
    fn identity_exhaustion_fails_closed() {
        let mut admission = admission();
        admission.next_id = u64::MAX;
        assert_eq!(admission.reserve(general(1, 1)), Err(Denied::Identities));
        assert_eq!(admission.records_charged(), 0);
    }

    /// A grant cannot be moved to itself. The two writes would land on one row and leave it claiming twice
    /// what the totals were charged, which a later release would hand back as capacity nobody ever took.
    #[test]
    fn a_grant_cannot_be_transferred_to_itself() {
        let mut admission = admission();
        let lease = admission
            .reserve(Request {
                records: 1,
                record_class: Class::General,
                bytes: 100,
                byte_class: Class::General,
                dns_tokens: 1,
                ..Request::default()
            })
            .expect("granted");
        let before = (
            admission.records_charged(),
            admission.bytes_charged(),
            admission.dns_tokens_charged(),
            admission.granted(&lease).expect("held"),
        );
        assert_eq!(
            admission.transfer(
                &lease,
                &lease,
                Request {
                    dns_tokens: 1,
                    ..Request::default()
                }
            ),
            Err(Denied::Aliased)
        );
        assert_eq!(admission.invariant_violations(), 1);
        assert_eq!(
            (
                admission.records_charged(),
                admission.bytes_charged(),
                admission.dns_tokens_charged(),
                admission.granted(&lease).expect("held"),
            ),
            before
        );
        // And releasing it now gives back exactly what it took, rather than the inflated row.
        let charged = admission.bytes_charged();
        admission.release(lease);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.bytes_charged(), charged - 100);
    }

    /// A subtraction naming a different class than the grant is refused, because applying it would take a
    /// general charge off the reserved ledger and desynchronize the categories from the aggregate.
    #[test]
    fn a_differently_classified_subtraction_is_refused() {
        let mut admission = admission();
        let general_lease = admission.reserve(general(1, 1_000)).expect("granted");
        let reserved = admission
            .reserve(Request::records(1, Class::Reserved))
            .expect("granted");
        let sums = (
            admission.general_records,
            admission.reserved_records,
            admission.general_bytes,
            admission.reserved_bytes,
        );

        // A shrink in the wrong class changes nothing and is counted.
        admission.shrink(&general_lease, Request::bytes(500, Class::Reserved));
        assert_eq!(admission.invariant_violations(), 1);
        assert_eq!(
            (
                admission.general_records,
                admission.reserved_records,
                admission.general_bytes,
                admission.reserved_bytes
            ),
            sums
        );
        // A transfer out of the wrong class likewise.
        assert_eq!(
            admission.transfer(
                &general_lease,
                &reserved,
                Request::records(1, Class::Reserved)
            ),
            Err(Denied::Arithmetic)
        );
        // And a split of the wrong class.
        assert_eq!(
            admission.split(&general_lease, Request::bytes(100, Class::Reserved)),
            Err(Denied::Arithmetic)
        );
        assert_eq!(
            (
                admission.general_records,
                admission.reserved_records,
                admission.general_bytes,
                admission.reserved_bytes
            ),
            sums
        );
        // The right class works, and the sums still add up to the aggregate.
        admission.shrink(&general_lease, Request::bytes(500, Class::General));
        assert_eq!(
            admission.general_bytes + admission.reserved_bytes,
            admission.bytes_charged()
        );
        admission.release(general_lease);
        admission.release(reserved);
    }

    /// Growth extends the row a lease already owns: no second slot, no second identity, and a full ledger
    /// cannot refuse a resize of something it is already recording.
    #[test]
    fn growth_uses_the_row_it_already_owns() {
        let mut admission = Admission::new(Totals {
            record_total: 2,
            dns_record_floor: 0,
            byte_only_owners: 0,
            ..totals()
        })
        .expect("granted");
        // Three rows is every slot this ledger has.
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(
                admission
                    .reserve(Request::bytes(10, Class::General))
                    .expect("granted"),
            );
        }
        assert_eq!(
            admission.reserve(Request::bytes(1, Class::General)),
            Err(Denied::Ledger)
        );
        let rows = admission.outstanding_leases();
        let identity = admission.next_id;
        // The ledger is full and the resize still succeeds, because it is not adding a row.
        admission
            .grow(&held[0], Request::bytes(90, Class::General))
            .expect("a resize needs no slot");
        assert_eq!(admission.granted(&held[0]).expect("held").bytes, 100);
        assert_eq!(admission.outstanding_leases(), rows);
        assert_eq!(admission.next_id, identity, "no identity was spent");

        // A resize that does not fit leaves the row and the totals exactly as they were.
        let charged = admission.bytes_charged();
        assert_eq!(
            admission.grow(&held[0], Request::bytes(u64::MAX, Class::General)),
            Err(Denied::Arithmetic)
        );
        assert_eq!(
            admission.grow(&held[0], Request::bytes(10_000_000, Class::General)),
            Err(Denied::Bytes)
        );
        assert_eq!(admission.granted(&held[0]).expect("held").bytes, 100);
        assert_eq!(admission.bytes_charged(), charged);
        for lease in held {
            admission.release(lease);
        }
    }

    /// The ledger fills to its slot bound, refuses the next atomically, and a released row is a row the next
    /// caller gets - which is the whole of what this owner needs from reuse.
    ///
    /// [Admission::ledger_slots] is the logical maximum and the only admission condition. Reuse is ordinary:
    /// a release frees a slot, the next reserve takes it, and nothing consults what the container has done
    /// with its own backing in between - that is opaque count-bounded overhead here, not accounted state.
    #[test]
    fn a_released_row_is_available_again() {
        let mut admission = admission();
        let slots = Admission::ledger_slots(totals().record_total, totals().byte_only_owners)
            .expect("the fixture totals hold their own accounting") as usize;

        // The whole bound, one row at a time.
        let mut held: Vec<Lease> = Vec::new();
        while admission.outstanding_leases() < slots {
            held.push(admission.reserve(general(0, 1)).expect("inside the bound"));
        }
        assert_eq!(held.len(), slots, "the bound really admits its whole count");
        assert_eq!(
            admission.reserve(general(0, 1)),
            Err(Denied::Ledger),
            "and one past it is refused"
        );
        assert_eq!(
            admission.outstanding_leases(),
            slots,
            "atomically: the refusal recorded nothing"
        );

        // A row given back is a row the next caller gets, and repeating that stays inside the same bound.
        let charged = (admission.records_charged(), admission.bytes_charged());
        for _ in 0..4 * slots {
            admission.release(held.remove(0));
            assert_eq!(
                admission.outstanding_leases(),
                slots - 1,
                "the row came back"
            );
            held.push(
                admission
                    .reserve(general(0, 1))
                    .expect("a released row is available again"),
            );
            assert_eq!(admission.outstanding_leases(), slots, "and was taken");
        }
        assert_eq!(
            (admission.records_charged(), admission.bytes_charged()),
            charged,
            "and reuse moved no accounting at all"
        );

        for lease in held {
            admission.release(lease);
        }
        assert_eq!(admission.outstanding_leases(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// Totals whose own accounting does not fit are refused at construction, before a dataplane exists to
    /// blame the resulting denials on.
    #[test]
    fn totals_that_cannot_hold_their_own_ledger_are_refused() {
        // Refused, and the reason names the sizing rather than a generic failure. Written through a helper
        // because [Admission] is not comparable - only the refusal is.
        fn refused(totals: Totals) -> Misconfigured {
            match Admission::new(totals) {
                Ok(_) => panic!("these totals cannot hold their own accounting"),
                Err(why) => why,
            }
        }
        // The derived row count overflows.
        assert_eq!(
            refused(Totals {
                record_total: u32::MAX,
                byte_only_owners: 8,
                ..totals()
            }),
            Misconfigured::LedgerSlots {
                record_total: u32::MAX,
                byte_only_owners: 8,
            }
        );
        // The ledger costs more than the floor that has to hold it. Not clamped: a clamp would report a
        // ledger that fits and then deny every later request as though traffic had filled the share.
        let slots = Admission::ledger_slots(100, 8).expect("fits");
        let ledger_bytes = Admission::ledger_bytes(slots).expect("fits");
        assert_eq!(
            refused(Totals {
                reserved_byte_floor: ledger_bytes - 1,
                ..totals()
            }),
            Misconfigured::Ledger {
                ledger_bytes,
                reserved_byte_floor: ledger_bytes - 1,
            }
        );
        // Floors and caps that are not inside their totals are configuration mistakes, not values to clamp.
        assert_eq!(
            refused(Totals {
                dns_record_floor: 101,
                ..totals()
            }),
            Misconfigured::RecordFloor {
                floor: 101,
                total: 100,
            }
        );
        assert_eq!(
            refused(Totals {
                reserved_byte_floor: 2_000_000,
                ..totals()
            }),
            Misconfigured::ByteFloor {
                floor: 2_000_000,
                total: 1_000_000,
            }
        );
        assert_eq!(
            refused(Totals {
                fragment_cap: 2_000_000,
                ..totals()
            }),
            Misconfigured::FragmentCap {
                cap: 2_000_000,
                total: 1_000_000,
            }
        );
        // And the exact fit is accepted, which is what makes the refusals above about the sizing rather than
        // about the check being conservative.
        let admission = Admission::new(Totals {
            reserved_byte_floor: ledger_bytes,
            ..totals()
        })
        .expect("a floor exactly the size of the ledger holds it");
        assert_eq!(admission.bytes_charged(), ledger_bytes);
    }

    /// The UDP first-send transaction, in the aggregate's own terms: two records and every byte at once, and
    /// every precommit failure restores exactly what an off-table mapping owned.
    ///
    /// The state ordering - socket dropped, collections dropped, then the lease released - lives with the
    /// relay; what this proves is the half the accounting owns: that one composite grant covers both records,
    /// that a partial grant is not representable, and that releasing an off-table mapping restores both
    /// totals.
    #[test]
    fn a_first_send_is_one_grant_and_every_precommit_failure_restores_it() {
        let mut admission = admission();
        let before = (admission.records_charged(), admission.bytes_charged());
        let composite = Request {
            records: 2,
            record_class: Class::General,
            bytes: 4_096,
            byte_class: Class::General,
            ..Request::default()
        };
        // Socket open, DF, a short send, a full send failure: each unwinds the same way, because there is one
        // grant to unwind rather than a record here and some bytes there.
        for _ in 0..4 {
            let lease = admission.reserve(composite).expect("granted");
            assert_eq!(admission.records_charged(), before.0 + 2);
            assert_eq!(admission.bytes_charged(), before.1 + 4_096);
            admission.release(lease);
            assert_eq!(
                (admission.records_charged(), admission.bytes_charged()),
                before,
                "an off-table mapping that was discarded restores both totals"
            );
        }
        assert_eq!(admission.invariant_violations(), 0);

        // A new remote on a mapping that already exists is a record against the grant it already holds, and a
        // failed send gives back that record and *not* the filter's row-state charge, which covers the
        // mapping's whole logical bound rather than the remotes currently in it.
        let lease = admission.reserve(composite).expect("granted");
        let charged = admission.bytes_charged();
        admission
            .grow(&lease, Request::records(1, Class::General))
            .expect("one more remote");
        assert_eq!(admission.records_charged(), before.0 + 3);
        admission.shrink(&lease, Request::records(1, Class::General));
        assert_eq!(admission.records_charged(), before.0 + 2);
        assert_eq!(
            admission.bytes_charged(),
            charged,
            "a failed send refunds none of the bound the mapping is still charged for"
        );
        admission.release(lease);
    }

    /// The fixed queues are charged once from a real depth times a real maximum payload, and an overflow in
    /// that arithmetic is a refusal rather than a small number.
    #[test]
    fn a_fixed_queue_reservation_is_charged_once_and_fails_closed() {
        let mut admission = admission();
        let before = admission.bytes_charged();
        // depth * max payload, once.
        let queue = admission
            .reserve(Request::bytes(16 * 1_024, Class::General))
            .expect("granted");
        assert_eq!(admission.bytes_charged(), before + 16 * 1_024);
        // Nothing is charged per item on top of it: an item occupies capacity that is already owed.
        assert_eq!(admission.bytes_charged(), before + 16 * 1_024);
        // An arithmetic overflow in the derivation is a denial, not a wrap.
        assert_eq!(
            admission.grow(&queue, Request::bytes(u64::MAX, Class::General)),
            Err(Denied::Arithmetic)
        );
        assert_eq!(admission.bytes_charged(), before + 16 * 1_024);
        admission.release(queue);
        assert_eq!(admission.bytes_charged(), before);
    }

    /// The floor arithmetic, stated as the property rather than as one example: the general ceiling plus the
    /// floor is the total, so the floor is inside the total at every size.
    #[test]
    fn the_dns_floor_stays_inside_the_aggregate_total() {
        for (record_total, dns_record_floor) in
            [(100u32, 32u32), (33, 32), (32, 32), (1_024, 32), (64, 0)]
        {
            let admission = Admission::new(Totals {
                record_total,
                dns_record_floor,
                ..totals()
            })
            .expect("granted");
            assert_eq!(admission.record_total(), record_total);
            assert_eq!(admission.dns_record_floor(), dns_record_floor);
            assert_eq!(
                admission.general_record_ceiling() + admission.dns_record_floor(),
                admission.record_total(),
                "the floor is inside the total, not beside it"
            );
            assert!(admission.general_record_ceiling() <= admission.record_total());
        }
    }

    /// A bound is solved against *general* headroom, so protected capacity can never inflate ordinary work.
    ///
    /// The failure this closes is a bound taken from "total minus charged": that counts the reserved floor -
    /// the descriptors held reachable for name resolution, the bytes held for one essential exchange and one
    /// output peak - as though a TCP flow could use them. A device whose general share is exhausted would
    /// still be told it could prepare for flows, out of capacity that exists so DNS is never crowded out.
    #[test]
    fn a_prepared_bound_sees_only_general_headroom() {
        let mut admission = Admission::new(Totals {
            record_total: 100,
            dns_record_floor: 40,
            byte_total: 1_000_000,
            reserved_byte_floor: 400_000,
            ..totals()
        })
        .expect("granted");
        let none = |_: usize| Some(0u64);
        let ledger = admission.bytes_charged();

        // Nothing charged yet: the headroom is the general ceilings, not the totals.
        let headroom = admission.general_headroom();
        assert_eq!(
            headroom.records, 60,
            "the DNS floor is not general's to use"
        );
        assert_eq!(headroom.bytes, 600_000, "nor is the essential floor");
        assert!(ledger > 0, "and the ledger's own charge is reserved-class");

        // General work fills its own ceiling. The reserved floor is untouched, and the bound is nonetheless
        // zero: there is nothing left that a flow may have.
        let filled = admission
            .reserve(general(60, 600_000))
            .expect("general fills its ceiling");
        assert_eq!(
            admission.general_headroom(),
            Headroom {
                records: 0,
                bytes: 0
            }
        );
        assert_eq!(largest_fitting(admission.general_headroom(), 1, none), 0);
        // Taken from the totals instead, the same admission would claim room it must not use.
        let naive = Headroom {
            records: admission.record_total(),
            bytes: admission.byte_total() - admission.bytes_charged(),
        };
        assert!(
            largest_fitting(naive, 1, none) > 0,
            "which is exactly the mistake this avoids"
        );
        admission.release(filled);

        // Adding reserved-class headroom alone never moves the bound.
        let before = largest_fitting(admission.general_headroom(), 1_000, none);
        let reserved = admission
            .reserve(Request::bytes(100_000, Class::Reserved))
            .expect("granted");
        assert!(
            largest_fitting(admission.general_headroom(), 1_000, none) <= before,
            "reserved work may take from general, never give to it"
        );
        admission.release(reserved);
        assert_eq!(
            largest_fitting(admission.general_headroom(), 1_000, none),
            before
        );

        // And each currency binds on its own: records first, then bytes.
        let records = admission.reserve(general(59, 0)).expect("granted");
        assert_eq!(largest_fitting(admission.general_headroom(), 1, none), 1);
        admission.release(records);
        let bytes = admission.reserve(general(0, 599_000)).expect("granted");
        assert_eq!(
            largest_fitting(admission.general_headroom(), 1_000, none),
            1
        );
        admission.release(bytes);
    }

    /// The prepared bound is solved rather than chosen, and it answers to whichever total binds first.
    #[test]
    fn a_prepared_bound_is_derived_from_whichever_total_binds() {
        // No tables at all, so the arithmetic is exactly items against bytes.
        let none = |_: usize| Some(0u64);

        // Record-limited: bytes would allow far more than the record ceiling does.
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10,
                    bytes: 1_000_000
                },
                100,
                none
            ),
            10
        );
        // Byte-limited: records would allow far more than the bytes do.
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10_000,
                    bytes: 1_000
                },
                100,
                none
            ),
            10
        );
        // Exactly at the boundary, and one byte short of it.
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10_000,
                    bytes: 1_000
                },
                1_000,
                none
            ),
            1
        );
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10_000,
                    bytes: 999
                },
                1_000,
                none
            ),
            0
        );
        // Not even one fits.
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10_000,
                    bytes: 0
                },
                1,
                none
            ),
            0
        );
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 0,
                    bytes: 1_000_000
                },
                1,
                none
            ),
            0
        );

        // The tables count too: preparing them consumes the very bytes the items would need, so a bound that
        // ignored its own tables would prepare for a count the total could not then admit.
        let tables = |n: usize| Some(n as u64 * 10);
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10_000,
                    bytes: 1_100
                },
                100,
                tables
            ),
            10
        );
        // Which is strictly fewer than ignoring them would have allowed.
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10_000,
                    bytes: 1_100
                },
                100,
                none
            ),
            11
        );

        // Arithmetic that would wrap is not a bound; it is a refusal.
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: u32::MAX,
                    bytes: u64::MAX
                },
                u64::MAX,
                none
            ),
            1
        );
        assert_eq!(
            largest_fitting(
                Headroom {
                    records: 10,
                    bytes: 1_000
                },
                1,
                |_| None
            ),
            0
        );
        // And the solved bound really is the largest that fits: one more does not.
        for (records, bytes, per_item) in [(64u32, 5_000u64, 70u64), (1_000, 1 << 20, 4_096)] {
            let bound = largest_fitting(Headroom { records, bytes }, per_item, tables);
            let fits = |n: usize| n as u64 * per_item + tables(n).unwrap() <= bytes;
            assert!(bound == 0 || fits(bound), "the bound fits");
            assert!(
                bound as u32 == records || !fits(bound + 1),
                "one more does not"
            );
        }
    }

    /// A completed resolver transaction gives back its descriptor record - and whatever logical token *its
    /// own grant* was holding - at once, and keeps a delivery grant for the bytes that are still being
    /// packetized.
    ///
    /// "Whatever it was holding" rather than "its token", because which grant holds one is per protocol: a
    /// UDP query owns its own, a DNS-over-TCP connection keeps the transport's between questions, and an
    /// unobservable outcome's token is moved into a quarantine instead of released at all. What is under test
    /// here is the split itself, on a grant that does hold one.
    ///
    /// The race this shape removes is the two orders of one pair of events: the answer arriving and the
    /// worker's terminal. Releasing the whole grant on the terminal freed capacity for a result buffer that
    /// still existed; releasing nothing until delivery held a descriptor record for a transaction that was
    /// over. Splitting is neither.
    #[test]
    fn a_resolver_terminal_releases_the_descriptor_and_keeps_the_delivery() {
        let mut admission = admission();
        let query = 1_500u64;
        let result = 2_500u64;
        let debt = admission
            .reserve(Request {
                records: 1,
                record_class: Class::Reserved,
                bytes: query + result,
                byte_class: Class::Reserved,
                dns_tokens: 1,
                ..Request::default()
            })
            .expect("granted");
        let bytes = admission.bytes_charged();
        assert_eq!(admission.records_charged(), 1);
        assert_eq!(admission.dns_tokens_charged(), 1);

        // At the joined terminal: the bytes are split off, then the descriptor record and the token go.
        let delivery = admission
            .split(&debt, Request::bytes(query + result, Class::Reserved))
            .expect("a pre-reserved slot");
        admission.release(debt);
        assert_eq!(
            admission.records_charged(),
            0,
            "the descriptor went at once"
        );
        assert_eq!(admission.dns_tokens_charged(), 0, "and so did the token");
        assert_eq!(
            admission.bytes_charged(),
            bytes,
            "and the result's bytes did not: they still exist"
        );

        // Only once the response has been built and the source buffers dropped.
        admission.release(delivery);
        assert_eq!(admission.bytes_charged(), bytes - query - result);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// The platform's returned buffer is adopted into the grant that was already reserved for it, at its
    /// actual capacity, rather than charged a second time.
    #[test]
    fn a_returned_result_is_adopted_rather_than_charged_twice() {
        let mut admission = admission();
        // A conservative maximum, reserved before the platform was asked.
        let reserved = 4_096u64;
        let debt = admission
            .reserve(Request {
                records: 1,
                record_class: Class::Reserved,
                bytes: reserved,
                byte_class: Class::Reserved,
                ..Request::default()
            })
            .expect("granted");
        let charged = admission.bytes_charged();

        // What actually came back is smaller, so the grant is reconciled downward - never upward, and never
        // by a second reserve, which would charge the same bytes twice.
        let actual = 900u64;
        admission.shrink(&debt, Request::bytes(reserved - actual, Class::Reserved));
        assert_eq!(admission.bytes_charged(), charged - (reserved - actual));
        assert_eq!(admission.granted(&debt).expect("held").bytes, actual);
        admission.release(debt);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// Every token-holding connection may own one submitted query at once, and the cap still stops the next
    /// *connection* rather than stopping at half as many.
    ///
    /// The artifact this rules out is a second token charged per query: with the cap at thirty-two, that
    /// turns thirty-two connections into sixteen connections with a query each, which is a limit the
    /// accounting invented rather than one anyone chose.
    #[test]
    fn a_query_costs_a_descriptor_and_no_second_token() {
        let mut admission = Admission::new(Totals {
            record_total: 200,
            dns_record_floor: 64,
            dns_token_cap: 32,
            ..totals()
        })
        .expect("granted");

        // Thirty-two connections, one logical token each.
        let mut connections = Vec::new();
        for _ in 0..32 {
            connections.push(
                admission
                    .reserve(Request {
                        records: 1,
                        record_class: Class::General,
                        dns_tokens: 1,
                        ..Request::default()
                    })
                    .expect("a connection and its token"),
            );
        }
        // A thirty-third connection is denied on the token, which is the limit that was chosen.
        assert_eq!(
            admission.reserve(Request {
                records: 1,
                record_class: Class::General,
                dns_tokens: 1,
                ..Request::default()
            }),
            Err(Denied::DnsTokens)
        );

        // And every one of the thirty-two may still have a query outstanding: a DNS-class descriptor record
        // and no token at all.
        let mut queries = Vec::new();
        for _ in 0..32 {
            queries.push(
                admission
                    .reserve(Request::records(1, Class::Reserved))
                    .expect("a query needs no second token"),
            );
        }
        assert_eq!(admission.dns_tokens_charged(), 32);
        assert_eq!(admission.records_charged(), 64);

        // A transport closing with its question still in flight hands that question its own token, rather
        // than releasing one and reserving another - which would leave a moment where the platform's slot
        // looked free while it was still taken.
        admission
            .transfer(
                &connections[0],
                &queries[0],
                Request {
                    dns_tokens: 1,
                    ..Request::default()
                },
            )
            .expect("the token moves");
        assert_eq!(admission.dns_tokens_charged(), 32, "no token was created");
        admission.release(connections.remove(0));
        assert_eq!(
            admission.dns_tokens_charged(),
            32,
            "and the closed transport did not take it"
        );
        // It goes when the question is really over.
        admission.release(queries.remove(0));
        assert_eq!(admission.dns_tokens_charged(), 31);

        for lease in connections.into_iter().chain(queries) {
            admission.release(lease);
        }
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    /// A lease dropped without release keeps its row, so the capacity is leaked fail-closed and the leak is
    /// visible rather than silently reused.
    #[test]
    fn a_dropped_lease_leaks_visibly_rather_than_refunding() {
        let mut admission = admission();
        let charged = admission.bytes_charged();
        {
            // Falls out of scope without ever being released, which is the whole of the case: [Lease] has no
            // `Drop` at all, so nothing gives the capacity back.
            let _lease = admission.reserve(general(1, 100)).expect("granted");
        }
        assert_eq!(admission.records_charged(), 1);
        assert_eq!(admission.bytes_charged(), charged + 100);
        assert_eq!(admission.outstanding_leases(), 1);
        assert!(admission.describe().contains("1 leases outstanding"));
    }
}
