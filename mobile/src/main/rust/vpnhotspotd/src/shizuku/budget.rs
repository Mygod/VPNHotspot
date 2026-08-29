use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::sys::resource::{getrlimit, Resource};
use vpnhotspotd::shared::admission::{Admission, Totals};

use crate::report;

/// The resolver's own per-UID limit on concurrent queries, held across `resolv_res_nsend`.
const MAX_QUERIES_PER_UID: u32 = 256;

/// The nested ceiling on the daemon's own concurrent resolver transactions, an eighth of the platform's
/// per-UID limit.
pub(crate) const CONCURRENT_QUERIES: u32 = MAX_QUERIES_PER_UID / 8;

/// The largest IP datagram there is, which is what a UDP or Echo reply may be.
pub(crate) const MAX_DATAGRAM: usize = u16::MAX as usize;

/// How deep a reply queue is, and therefore how many received datagrams may exist at once.
pub(crate) const REPLY_QUEUE_DEPTH: usize = 32;

/// Bytes the daemon may hold in incomplete reassembly contexts.
const FRAGMENT_BYTES: u64 = 4 * 1024 * 1024;

/// The share of measurably available memory the dataplane may hold.
const MEMORY_SHARE: u64 = 8;

/// Owners that hold bytes and never a record, which is what the admission ledger has to be sized for beyond
/// the record-backed ones.
const BYTE_ONLY_OWNERS: u32 = 16 + 9;

static NEXT_ADMISSION_ID: AtomicU64 = AtomicU64::new(1);

/// What one maximum resolver exchange needs at once: the query as it arrived, the answer as the platform
/// returns it, and the framing copy in between.
const ESSENTIAL_DNS_BYTES: u64 = 3 * MAX_DATAGRAM as u64;

/// What one output packetization peak needs: the datagram being split and the fragment being built from it.
const ESSENTIAL_OUTPUT_BYTES: u64 = 2 * MAX_DATAGRAM as u64;

/// What the device says, alongside the totals derived from it.
pub(crate) struct Measured {
    pub(crate) totals: Totals,
    soft_limit: u64,
    open: u64,
    available: u64,
}

impl Measured {
    pub(crate) fn describe(&self) -> String {
        format!(
            "RLIMIT_NOFILE {} less {} already open leaves {} records, {} of them a floor only DNS may \
             enter; {} bytes available gives a {} byte share, {} of it a floor only essential work may \
             enter, with at most {} in incomplete reassembly and at most {} logical resolver transactions",
            self.soft_limit,
            self.open,
            self.totals.record_total,
            self.totals.dns_record_floor,
            self.available,
            self.totals.byte_total,
            self.totals.reserved_byte_floor,
            self.totals.fragment_cap,
            self.totals.dns_token_cap,
        )
    }
}

/// Measures the device and derives the totals one [Admission] is built from.
pub(crate) async fn measure() -> io::Result<Measured> {
    // `rlim_t` is u32 on 32-bit Android and u64 on 64-bit Android; `From` cannot truncate either ABI.
    #[allow(clippy::useless_conversion)]
    let soft_limit = u64::from(
        getrlimit(Resource::RLIMIT_NOFILE)
            .map_err(io::Error::from)?
            .0,
    );
    // Counted rather than predicted: stdio, the control socket, the TUN, and whatever the runtime
    // opened for itself are all already here, and none of them is the daemon's to enumerate. The
    // directory handle doing the counting is itself open, so this over-counts by one.
    let mut open = 0u64;
    let mut entries = tokio::fs::read_dir("/proc/self/fd").await?;
    while entries.next_entry().await?.is_some() {
        open += 1;
    }
    let record_total = soft_limit.checked_sub(open).ok_or_else(|| {
        io::Error::other(format!(
            "RLIMIT_NOFILE {soft_limit} leaves no room for {open} open descriptors"
        ))
    })?;
    // Narrowed rather than saturated: a descriptor total this process cannot count is one it cannot account
    // for, and admitting against a silently truncated one would over-admit for the whole session.
    let record_total = u32::try_from(record_total)
        .map_err(|e| io::Error::other(format!("implausible RLIMIT_NOFILE {soft_limit}: {e}")))?;
    if record_total <= CONCURRENT_QUERIES {
        return Err(io::Error::other(format!(
            "RLIMIT_NOFILE {soft_limit} less {open} open descriptors leaves {record_total} records, which \
             is not more than the {CONCURRENT_QUERIES} the resolver floor holds"
        )));
    }
    // MemAvailable rather than MemTotal or a cgroup limit: it is the kernel's own estimate of what can be
    // allocated without reclaiming, it is readable at the app UID where the sysctls this daemon would
    // otherwise want are not, and it is the only memory number this process can honestly measure.
    let meminfo = tokio::fs::read_to_string("/proc/meminfo").await?;
    let available = parse_available_memory(&meminfo)?;
    let byte_total = available / MEMORY_SHARE;
    // The accounting's own cost, derived from the same bound the ledger is allocated to. Inside the reserved
    // floor rather than beside it, because it is not optional and it is not traffic.
    let ledger_slots = Admission::ledger_slots(record_total, BYTE_ONLY_OWNERS).ok_or_else(|| {
        io::Error::other(format!(
            "a ledger for {record_total} records and {BYTE_ONLY_OWNERS} byte-only owners does not fit"
        ))
    })?;
    let ledger_bytes = Admission::ledger_bytes(ledger_slots)
        .ok_or_else(|| io::Error::other(format!("a ledger of {ledger_slots} rows does not fit")))?;
    let reserved_byte_floor = ledger_bytes
        .checked_add(ESSENTIAL_DNS_BYTES)
        .and_then(|floor| floor.checked_add(ESSENTIAL_OUTPUT_BYTES))
        .ok_or_else(|| io::Error::other("the essential byte floor does not fit"))?;
    if reserved_byte_floor > byte_total {
        return Err(io::Error::other(format!(
            "a {byte_total} byte share of {available} available cannot hold the {reserved_byte_floor} bytes \
             the accounting and one essential exchange need"
        )));
    }
    let measured = Measured {
        totals: Totals {
            // One per session, and monotone across sessions in the same process, so a lease from a replaced
            // session is recognisably foreign rather than plausibly current.
            admission_id: NEXT_ADMISSION_ID.fetch_add(1, Ordering::Relaxed),
            record_total,
            dns_record_floor: CONCURRENT_QUERIES,
            byte_total,
            reserved_byte_floor,
            // Nested inside the same measured share rather than added on top of it, so reassembly and
            // everything else cannot between them promise more than the share the dataplane was granted.
            fragment_cap: FRAGMENT_BYTES.min(byte_total),
            dns_token_cap: CONCURRENT_QUERIES,
            byte_only_owners: BYTE_ONLY_OWNERS,
        },
        soft_limit,
        open,
        available,
    };
    report::stdout!("dataplane budget: {}", measured.describe());
    Ok(measured)
}

/// Parses `MemAvailable` without inventing a fallback resource ceiling.
fn parse_available_memory(meminfo: &str) -> io::Result<u64> {
    const FIELD: &str = "MemAvailable:";
    let line = meminfo
        .lines()
        .find(|line| line.starts_with(FIELD))
        .ok_or_else(|| io::Error::other("/proc/meminfo has no MemAvailable"))?;
    // "MemAvailable:   2159872 kB", and the unit is always kB
    let kilobytes: u64 = line[FIELD.len()..]
        .split_whitespace()
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| io::Error::other(format!("cannot parse {line:?}")))?;
    Ok(kilobytes.saturating_mul(1024))
}
