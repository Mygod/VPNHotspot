//! Session-wide record and byte accounting. Leases are refunded only after their owner releases the state.
use std::collections::HashMap;
use std::fmt;

/// Which side of a floor a request is on.
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

    /// Includes record owners, fixed byte-only owners, and one owner-confined split in flight.
    pub fn ledger_slots(record_total: u32, byte_only_owners: u32) -> Option<u32> {
        record_total.checked_add(byte_only_owners)?.checked_add(1)
    }

    /// What a ledger of that many rows costs, so the caller sizing the reserved floor charges the same number
    /// this will.
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
pub fn logical_footprint<Row>(entries: usize) -> Option<u64> {
    u64::try_from(entries)
        .ok()?
        .checked_mul(std::mem::size_of::<Row>() as u64)
}

/// A conservative upper bound on a contiguous collection prepared for `slots` of `slot_bytes`.
pub fn linear_footprint(slots: usize, slot_bytes: u64) -> Option<u64> {
    u64::try_from(slots)
        .ok()?
        .checked_mul(slot_bytes)?
        .checked_mul(2)
}

/// A set of totals the accounting cannot honestly record, answered at construction rather than as a denial
/// once traffic is flowing.
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

    #[test]
    fn a_logical_row_cost_is_the_rows_times_one_row() {
        assert_eq!(logical_footprint::<(u64, u64)>(0), Some(0));
        assert_eq!(logical_footprint::<(u64, u64)>(1_000), Some(16_000));
        let pair = std::mem::size_of::<(IpAddr, Instant)>() as u64;
        assert!(
            pair > std::mem::size_of::<IpAddr>() as u64 + std::mem::size_of::<Instant>() as u64 - 8
        );
        assert_eq!(logical_footprint::<(IpAddr, Instant)>(64), Some(64 * pair));
        assert!(logical_footprint::<(u64, u64)>(11) > logical_footprint::<(u64, u64)>(10));
        assert_eq!(logical_footprint::<[u8; 1024]>(usize::MAX), None);
    }

    fn totals() -> Totals {
        Totals {
            admission_id: 1,
            record_total: 100,
            dns_record_floor: 32,
            byte_total: 1_000_000,
            reserved_byte_floor: 200_000,
            fragment_cap: 4_000,
            dns_token_cap: 32,
            byte_only_owners: 8,
        }
    }

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

    #[test]
    fn the_dns_floor_is_inside_the_total_and_only_dns_may_enter_it() {
        let mut admission = admission();
        assert_eq!(admission.record_total(), 100);
        assert_eq!(admission.general_record_ceiling(), 68);
        assert_eq!(admission.dns_record_floor(), 32);

        let general_fill = admission
            .reserve(Request::records(68, Class::General))
            .expect("general work fills its ceiling");
        assert_eq!(
            admission.reserve(Request::records(1, Class::General)),
            Err(Denied::Records)
        );
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
        assert_eq!(
            admission.reserve(Request {
                bytes: 1,
                byte_class: Class::General,
                fragment_bytes: 1,
                ..Request::default()
            }),
            Err(Denied::FragmentBytes)
        );
        assert!(admission.bytes_charged() >= 4_000);
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

    #[test]
    fn retained_collection_capacity_keeps_its_charge() {
        let mut admission = admission();
        let table = admission
            .reserve(Request::bytes(1_024, Class::General))
            .expect("granted");
        let entry = admission
            .reserve(Request::records(1, Class::General))
            .expect("granted");
        let bytes = admission.bytes_charged();
        admission.release(entry);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.bytes_charged(), bytes);
        admission.shrink(&table, Request::bytes(512, Class::General));
        assert_eq!(admission.bytes_charged(), bytes - 512);
        admission.release(table);
    }

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
        let honest = other.reserve(general(1, 10)).expect("granted");
        other.release(honest);
        assert_eq!(other.invariant_violations(), 0);
    }

    #[test]
    fn a_composite_two_record_reserve_is_atomic() {
        let mut admission = Admission::new(Totals {
            record_total: 33,
            dns_record_floor: 32,
            ..totals()
        })
        .expect("granted");
        assert_eq!(admission.general_record_ceiling(), 1);
        assert_eq!(admission.reserve(general(2, 10)), Err(Denied::Records));
        assert_eq!(admission.records_charged(), 0);
        let one = admission.reserve(general(1, 10)).expect("granted");
        assert_eq!(admission.reserve(general(2, 10)), Err(Denied::Records));
        admission.release(one);
    }

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
        assert_eq!(admission.bytes_charged(), ledger_self_charge);
        assert_eq!(admission.fragment_bytes_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.outstanding_leases(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

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
        let ordinary = admission.reserve(general(1, 10)).expect("granted");
        admission.release(ordinary);
        for lease in held {
            admission.release(lease);
        }
        assert_eq!(admission.dns_tokens_charged(), 0);
    }

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
        admission.release(transport);
        assert_eq!(admission.dns_tokens_charged(), 1);
        admission.release(debt);
        assert_eq!(admission.dns_tokens_charged(), 0);
    }

    #[test]
    fn ledger_slot_exhaustion_fails_closed() {
        let mut admission = Admission::new(Totals {
            record_total: 2,
            dns_record_floor: 0,
            byte_only_owners: 0,
            ..totals()
        })
        .expect("granted");
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
        assert_eq!(
            admission.split(&held[0], Request::bytes(1, Class::General)),
            Err(Denied::Ledger)
        );
        assert_eq!(admission.bytes_charged(), charged);
        for lease in held {
            admission.release(lease);
        }
    }

    #[test]
    fn identity_exhaustion_fails_closed() {
        let mut admission = admission();
        admission.next_id = u64::MAX;
        assert_eq!(admission.reserve(general(1, 1)), Err(Denied::Identities));
        assert_eq!(admission.records_charged(), 0);
    }

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
        let charged = admission.bytes_charged();
        admission.release(lease);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.bytes_charged(), charged - 100);
    }

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
        assert_eq!(
            admission.transfer(
                &general_lease,
                &reserved,
                Request::records(1, Class::Reserved)
            ),
            Err(Denied::Arithmetic)
        );
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
        admission.shrink(&general_lease, Request::bytes(500, Class::General));
        assert_eq!(
            admission.general_bytes + admission.reserved_bytes,
            admission.bytes_charged()
        );
        admission.release(general_lease);
        admission.release(reserved);
    }

    #[test]
    fn growth_uses_the_row_it_already_owns() {
        let mut admission = Admission::new(Totals {
            record_total: 2,
            dns_record_floor: 0,
            byte_only_owners: 0,
            ..totals()
        })
        .expect("granted");
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
        admission
            .grow(&held[0], Request::bytes(90, Class::General))
            .expect("a resize needs no slot");
        assert_eq!(admission.granted(&held[0]).expect("held").bytes, 100);
        assert_eq!(admission.outstanding_leases(), rows);
        assert_eq!(admission.next_id, identity);

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

    #[test]
    fn a_released_row_is_available_again() {
        let mut admission = admission();
        let slots = Admission::ledger_slots(totals().record_total, totals().byte_only_owners)
            .expect("the fixture totals hold their own accounting") as usize;

        let mut held: Vec<Lease> = Vec::new();
        while admission.outstanding_leases() < slots {
            held.push(admission.reserve(general(0, 1)).expect("inside the bound"));
        }
        assert_eq!(held.len(), slots);
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
            assert_eq!(admission.outstanding_leases(), slots);
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

    #[test]
    fn totals_that_cannot_hold_their_own_ledger_are_refused() {
        fn refused(totals: Totals) -> Misconfigured {
            match Admission::new(totals) {
                Ok(_) => panic!("these totals cannot hold their own accounting"),
                Err(why) => why,
            }
        }
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
        let admission = Admission::new(Totals {
            reserved_byte_floor: ledger_bytes,
            ..totals()
        })
        .expect("a floor exactly the size of the ledger holds it");
        assert_eq!(admission.bytes_charged(), ledger_bytes);
    }

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

    #[test]
    fn a_fixed_queue_reservation_is_charged_once_and_fails_closed() {
        let mut admission = admission();
        let before = admission.bytes_charged();
        let queue = admission
            .reserve(Request::bytes(16 * 1_024, Class::General))
            .expect("granted");
        assert_eq!(admission.bytes_charged(), before + 16 * 1_024);
        assert_eq!(admission.bytes_charged(), before + 16 * 1_024);
        assert_eq!(
            admission.grow(&queue, Request::bytes(u64::MAX, Class::General)),
            Err(Denied::Arithmetic)
        );
        assert_eq!(admission.bytes_charged(), before + 16 * 1_024);
        admission.release(queue);
        assert_eq!(admission.bytes_charged(), before);
    }

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

        let headroom = admission.general_headroom();
        assert_eq!(
            headroom.records, 60,
            "the DNS floor is not general's to use"
        );
        assert_eq!(headroom.bytes, 600_000);
        assert!(ledger > 0);

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
        let naive = Headroom {
            records: admission.record_total(),
            bytes: admission.byte_total() - admission.bytes_charged(),
        };
        assert!(
            largest_fitting(naive, 1, none) > 0,
            "which is exactly the mistake this avoids"
        );
        admission.release(filled);

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

    #[test]
    fn a_prepared_bound_is_derived_from_whichever_total_binds() {
        let none = |_: usize| Some(0u64);

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
        for (records, bytes, per_item) in [(64u32, 5_000u64, 70u64), (1_000, 1 << 20, 4_096)] {
            let bound = largest_fitting(Headroom { records, bytes }, per_item, tables);
            let fits = |n: usize| n as u64 * per_item + tables(n).unwrap() <= bytes;
            assert!(bound == 0 || fits(bound));
            assert!(
                bound as u32 == records || !fits(bound + 1),
                "one more does not"
            );
        }
    }

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

        let delivery = admission
            .split(&debt, Request::bytes(query + result, Class::Reserved))
            .expect("a pre-reserved slot");
        admission.release(debt);
        assert_eq!(
            admission.records_charged(),
            0,
            "the descriptor went at once"
        );
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(
            admission.bytes_charged(),
            bytes,
            "and the result's bytes did not: they still exist"
        );

        admission.release(delivery);
        assert_eq!(admission.bytes_charged(), bytes - query - result);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_returned_result_is_adopted_rather_than_charged_twice() {
        let mut admission = admission();
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

        let actual = 900u64;
        admission.shrink(&debt, Request::bytes(reserved - actual, Class::Reserved));
        assert_eq!(admission.bytes_charged(), charged - (reserved - actual));
        assert_eq!(admission.granted(&debt).expect("held").bytes, actual);
        admission.release(debt);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_query_costs_a_descriptor_and_no_second_token() {
        let mut admission = Admission::new(Totals {
            record_total: 200,
            dns_record_floor: 64,
            dns_token_cap: 32,
            ..totals()
        })
        .expect("granted");

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
        assert_eq!(
            admission.reserve(Request {
                records: 1,
                record_class: Class::General,
                dns_tokens: 1,
                ..Request::default()
            }),
            Err(Denied::DnsTokens)
        );

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
        assert_eq!(admission.dns_tokens_charged(), 32);
        admission.release(connections.remove(0));
        assert_eq!(
            admission.dns_tokens_charged(),
            32,
            "and the closed transport did not take it"
        );
        admission.release(queries.remove(0));
        assert_eq!(admission.dns_tokens_charged(), 31);

        for lease in connections.into_iter().chain(queries) {
            admission.release(lease);
        }
        assert_eq!(admission.dns_tokens_charged(), 0);
        assert_eq!(admission.records_charged(), 0);
        assert_eq!(admission.invariant_violations(), 0);
    }

    #[test]
    fn a_dropped_lease_leaks_visibly_rather_than_refunding() {
        let mut admission = admission();
        let charged = admission.bytes_charged();
        {
            let _lease = admission.reserve(general(1, 100)).expect("granted");
        }
        assert_eq!(admission.records_charged(), 1);
        assert_eq!(admission.bytes_charged(), charged + 100);
        assert_eq!(admission.outstanding_leases(), 1);
        assert!(admission.describe().contains("1 leases outstanding"));
    }
}
