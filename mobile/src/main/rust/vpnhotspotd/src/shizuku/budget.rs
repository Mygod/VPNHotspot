use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use nix::sys::resource::{getrlimit, Resource};
use vpnhotspotd::shared::admission::Totals;

use crate::report;

static NEXT_ADMISSION_ID: AtomicU64 = AtomicU64::new(1);

/// One resolver descriptor reserved for essential DNS: one is the minimum capacity that lets any virtual-DNS
/// query reach Android while general traffic fills its share. General admission refuses before consuming it;
/// DNS may still use all free descriptor headroom, so this is a liveness floor rather than a DNS ceiling.
const ESSENTIAL_DNS_DESCRIPTORS: u32 = 1;

/// What the device says, alongside the totals derived from it.
pub(crate) struct Measured {
    pub(crate) totals: Totals,
    soft_limit: u64,
    open: u64,
}

impl Measured {
    pub(crate) fn describe(&self) -> String {
        format!(
            "RLIMIT_NOFILE {} less {} already open leaves {} descriptors, {} of them a floor only DNS may \
             enter",
            self.soft_limit,
            self.open,
            self.totals.descriptor_total,
            self.totals.dns_descriptor_floor,
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
    // directory handle doing the counting is itself open, so this over-counts by one. Besides keeping the
    // admitted steady state below RLIMIT, that spare slot lets a synchronous TCP candidate be opened before
    // it is charged and immediately closed if general admission refuses it.
    let mut open = 0u64;
    let mut entries = tokio::fs::read_dir("/proc/self/fd").await?;
    while entries.next_entry().await?.is_some() {
        open += 1;
    }
    let descriptor_total = soft_limit.checked_sub(open).ok_or_else(|| {
        io::Error::other(format!(
            "RLIMIT_NOFILE {soft_limit} leaves no room for {open} open descriptors"
        ))
    })?;
    // Narrowed rather than saturated: a descriptor total this process cannot count is one it cannot account
    // for, and admitting against a silently truncated one would over-admit for the whole session.
    let descriptor_total = u32::try_from(descriptor_total)
        .map_err(|e| io::Error::other(format!("implausible RLIMIT_NOFILE {soft_limit}: {e}")))?;
    if descriptor_total < ESSENTIAL_DNS_DESCRIPTORS {
        return Err(io::Error::other(format!(
            "RLIMIT_NOFILE {soft_limit} less {open} open descriptors leaves {descriptor_total} descriptors, \
             which is smaller than the {ESSENTIAL_DNS_DESCRIPTORS} the resolver floor requires"
        )));
    }
    let measured = Measured {
        totals: Totals {
            // One per session, and monotone across sessions in the same process, so a lease from a replaced
            // session is recognisably foreign rather than plausibly current.
            admission_id: NEXT_ADMISSION_ID.fetch_add(1, Ordering::Relaxed),
            descriptor_total,
            dns_descriptor_floor: ESSENTIAL_DNS_DESCRIPTORS,
        },
        soft_limit,
        open,
    };
    report::stdout!("dataplane budget: {}", measured.describe());
    Ok(measured)
}
