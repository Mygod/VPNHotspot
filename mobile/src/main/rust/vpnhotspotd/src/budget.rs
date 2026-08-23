//! What the device measures, and the totals derived from it. Nothing here decides whether state may exist.
//!
//! That decision is [vpnhotspotd::shared::admission::Admission]'s, and the split is the point: this file is
//! the only thing that reads `RLIMIT_NOFILE`, `/proc/self/fd` and `/proc/meminfo`, and admission is the only
//! thing that charges against what they say. Measurement that also enforced would be a second accounting
//! beside the aggregate, which is exactly the shape this split exists to prevent.
//!
//! Every number below is either measured or derived from a measurement by a documented rule. The two that are
//! neither - the resolver's nested cap and the reassembly cap - are the platform's own limits, cited where
//! they are declared, and both are checked against the measured share rather than added to it.

use std::io;
use std::mem::MaybeUninit;

use vpnhotspotd::shared::admission::{Admission, Totals};

use crate::report;

/// The resolver's own per-UID limit on concurrent queries, held across `resolv_res_nsend`.
///
/// https://android.googlesource.com/platform/packages/modules/DnsResolver/+/refs/tags/android-17.0.0_r1/OperationLimiter.h#32
const MAX_QUERIES_PER_UID: u32 = 256;

/// The nested ceiling on the daemon's own concurrent resolver transactions, an eighth of the platform's
/// per-UID limit.
///
/// A fraction rather than the whole limit because the daemon is not the only holder of those slots.
/// `DnsProxyListener` keeps one across `resolv_res_nsend`, a replaced session's queries keep draining after
/// the app has started a new one, and the app process resolves for itself under the same UID. Half would let
/// two pools consume every slot with nothing left for either; an eighth leaves seven pools' worth of
/// headroom. No fraction survives unbounded session churn, which is why exhaustion returns SERVFAIL rather
/// than being treated as an invariant violation - sizing makes it rare, handling it makes it harmless.
///
/// Ample in absolute terms, and checkably so - the daemon reports its slowest resolver round trip, which is
/// how this was checked rather than assumed. Measured at 274 ms under a deliberately saturating 80-query
/// burst, so thirty-two in flight sustains over a hundred queries per second even at that worst case, and
/// considerably more at the unsaturated one.
///
/// This is both the logical token cap and the record floor, and they are different things measured by the
/// same number: a token bounds how many transactions the platform is holding for this process, and the floor
/// is the descriptors those transactions need, kept reachable inside the aggregate total rather than
/// subtracted from it.
pub(crate) const CONCURRENT_QUERIES: u32 = MAX_QUERIES_PER_UID / 8;

/// Buffer per direction for one terminated TCP flow.
///
/// 65535 is the largest window a receiver can advertise without RFC 1323 window scaling, so it is the largest
/// buffer that is useful against *every* peer rather than only against those that negotiate scaling. Rounded
/// to 64 KiB. Bigger would help some peers and cost every flow; smaller would cap throughput on any path whose
/// bandwidth-delay product exceeds it.
pub(crate) const FLOW_BUFFER: usize = 64 * 1024;

/// The largest IP datagram there is, which is what a UDP or Echo reply may be.
pub(crate) const MAX_DATAGRAM: usize = u16::MAX as usize;

/// How deep a reply queue is, and therefore how many received datagrams may exist at once.
///
/// A real bound rather than a nominal one, because the reply task takes its slot *before* it allocates the
/// payload: at most this many maximum-sized datagrams are queued at once - plus the one a receiver has taken
/// out of a slot and is still holding, which is why the reservation is `(depth + 1) * MAX_DATAGRAM` rather
/// than one datagram less. That is what makes the number small. The TUN writer's own queue
/// behind it is what absorbs a burst; this one only has to cover the ingress loop's scheduling latency, and a
/// deeper one would buy nothing but two megabytes per additional thirty-two slots.
pub(crate) const REPLY_QUEUE_DEPTH: usize = 32;

/// Bytes the daemon may hold in incomplete reassembly contexts.
///
/// Linux's own `ipfrag_high_thresh` default, which is the kernel solving this exact problem on this exact
/// device: the total it will hold in incomplete IPv4 reassembly before it starts evicting. The sysctl is not
/// readable at the app UID, so the documented default is taken rather than a number invented here - and it is
/// a ceiling for a *relay* serving a handful of tethered clients, where the kernel's is for the whole host, so
/// erring toward the kernel's figure errs generous.
///
/// Clamped below to the dataplane's measured memory share, so a device with less memory than this commits
/// less rather than more. Nested inside that share rather than added to it: it is a cap on how much of the
/// byte total fragments may hold, not a pool of its own.
///
/// https://android.googlesource.com/kernel/common/+/refs/heads/android16-6.12/net/ipv4/ip_fragment.c
const FRAGMENT_BYTES: u64 = 4 * 1024 * 1024;

/// The share of measurably available memory the dataplane may hold.
///
/// An eighth, for the same shape of reason as the query ceiling: this is a background helper for tethering, and
/// it should not be the reason the system reclaims. The denominator is measured rather than assumed - see
/// [measure] - so the byte total follows the device instead of a guess about it.
///
/// A policy share, not a process ceiling. Exceeding it would not fail; it is the number the daemon holds
/// itself to, and what it counts is Rust-visible owned heap.
const MEMORY_SHARE: u64 = 8;

/// Owners that hold bytes and never a record, which is what the admission ledger has to be sized for beyond
/// the record-backed ones.
///
/// Counted rather than estimated: the TUN writer, the UDP reply queue, the Echo reply queue, the TCP
/// readiness channel, the output packetization scratch, the reassembly table's retained capacity, its one
/// transient completed packet, the IPv4 identification table, the ingress read buffer, the UDP mapping table,
/// the Echo socket table, the Echo session table, the TCP flow table, the TCP transaction table, the virtual
/// DNS transaction table, the smoltcp socket set, the engine's output slot, and the per-flow fair queue. Eight
/// spare, so a later fixed owner does not silently push the ledger past what was charged for it.
///
/// The writer is one row and not four: its packet queue, the packet it has in hand, its retirement channel
/// and its Identification settlement channel are built together, released together, and charged as one lease.
/// What this count is for is the *simultaneous* ledger inventory, so what matters is how many rows exist at
/// once rather than how many allocations each covers.
const BYTE_ONLY_OWNERS: u32 = 18 + 8;

/// What one maximum resolver exchange needs at once: the query as it arrived, the answer as the platform
/// returns it, and the framing copy in between.
///
/// DNS over TCP frames a message with a 16-bit length, so `u16::MAX` is the largest either direction can be.
/// Three of them is the simultaneous peak the resolver path can reach - query held through classification,
/// result adopted from the platform, and the framed or packetized copy being handed to output.
const ESSENTIAL_DNS_BYTES: u64 = 3 * MAX_DATAGRAM as u64;

/// What one output packetization peak needs: the datagram being split and the fragment being built from it.
const ESSENTIAL_OUTPUT_BYTES: u64 = 2 * MAX_DATAGRAM as u64;

/// What the device says, alongside the totals derived from it.
///
/// Both, because the derivation is what a reader has to be able to check: a report naming only the totals
/// cannot be argued with, while one naming the limit, the open descriptors and the available memory it came
/// from can.
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
///
/// The descriptor total is the whole of what this process may open, with the DNS floor *inside* it: nothing is
/// subtracted here, because a floor that is subtracted is a smaller total wearing the name of a bigger one.
/// The byte total is the measured share, and the reserved floor inside it is what the accounting itself costs
/// plus the headroom essential work needs - derived here rather than guessed, so a share too small to hold its
/// own ledger is refused by [Admission::new] before a dataplane exists rather than showing up later as
/// denials blamed on traffic.
pub(crate) async fn measure() -> io::Result<Measured> {
    let mut limit = MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: getrlimit fills the rlimit it is given and reads nothing else.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above succeeded, so the value is initialized.
    //
    // Widened rather than used as-is: `rlim_t` is 32 bits on the 32-bit Android ABIs this is built for and 64
    // on the others, and the totals below are `u64` everywhere. `From` is reflexive, so this is the identity
    // on the wider ABIs and the widening on the narrower ones, with no cast that could truncate on either.
    // Useless on the 64-bit ABIs, where `rlim_t` is already `u64`, and load-bearing on the 32-bit ones,
    // where it is `u32`. Allowed rather than dropped: the alternative is a cast that would truncate if the
    // widths were ever the other way round, and a lint that is right for one of this crate's four targets is
    // not a reason to write something wrong on the other three.
    #[allow(clippy::useless_conversion)]
    let soft_limit = u64::from(unsafe { limit.assume_init() }.rlim_cur);
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
    let available = available_memory().await?;
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
            admission_id: next_admission_id(),
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

/// A fresh identity per admission, so a lease that outlived its session cannot be mistaken for one of this
/// session's.
fn next_admission_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// `MemAvailable` in bytes. An absent or unparseable field is an error rather than a default, because a
/// silently assumed ceiling is exactly what the Resource Policy forbids.
async fn available_memory() -> io::Result<u64> {
    let meminfo = tokio::fs::read_to_string("/proc/meminfo").await?;
    parse_available_memory(&meminfo)
}

/// Parsed apart from the read so that what the device provides and what is done with it are separable, which
/// is as far as this can go: the binary crate is `test = false`, so a unit test here would never run and is
/// not written.
///
/// `u64` throughout, and it stays `u64`: the byte total is a `u64` dimension in the aggregate, so a 32-bit
/// Android ABI does not narrow it. What the address space can actually hold on such an ABI is a separate
/// question the allocator answers, and the share taken here is far below it.
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
