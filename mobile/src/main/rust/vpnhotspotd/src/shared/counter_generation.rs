use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nix::time::{clock_gettime, ClockId};

/// Next daemon-owned traffic counter generation, or 0 before the first allocation. Generation 0 is
/// reserved for IPv4 forwarding counters, whose Kotlin chains follow the committed `(MAC, IPv4)`
/// pair instead of a daemon-owned lifetime.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Starts a daemon-owned DNS or NAT66 counter generation.
///
/// Generations stay unique across the daemon processes of one boot, because Kotlin may still hold
/// chains from a daemon that died when a successor reports counters. The first allocation takes
/// its base from `CLOCK_BOOTTIME` in nanoseconds, which is monotonic system-wide for the whole
/// boot, and every allocation then adds one. Allocation `i` made at time `t_i` yields
/// `base + i <= t_0 + i <= t_i`, because a daemon allocates far fewer than one generation per
/// nanosecond. A successor reads its base after Kotlin has stopped reading its predecessor, so all
/// of its generations exceed every predecessor generation that Kotlin saw. The clock is far above
/// zero once userspace runs, so this never yields the reserved generation 0.
pub fn next_counter_generation() -> io::Result<u64> {
    if NEXT_GENERATION.load(Ordering::Relaxed) == 0 {
        // Racing first allocations each offer a clock reading. Keeping the largest one preserves
        // the bound, and the counter never moves backwards, so every allocation stays unique.
        NEXT_GENERATION.fetch_max(boot_time_nanos()?, Ordering::Relaxed);
    }
    Ok(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed))
}

fn boot_time_nanos() -> io::Result<u64> {
    u64::try_from(Duration::from(clock_gettime(ClockId::CLOCK_BOOTTIME)?).as_nanos())
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_are_nonzero_increasing_and_not_ahead_of_boot_time() {
        let first = next_counter_generation().unwrap();
        let second = next_counter_generation().unwrap();
        assert_ne!(first, 0);
        assert!(second > first);
        assert!(second <= boot_time_nanos().unwrap());
    }
}
