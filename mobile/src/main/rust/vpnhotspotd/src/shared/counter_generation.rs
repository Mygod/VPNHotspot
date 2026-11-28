use std::sync::atomic::{AtomicU64, Ordering};

/// Next daemon-owned traffic counter generation. Generation 0 is reserved for IPv4 forwarding
/// counters, whose Kotlin chains follow the committed `(MAC, IPv4)` pair instead of a daemon-owned
/// lifetime.
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Starts a daemon-owned DNS or NAT66 counter generation.
///
/// A generation is unique within this daemon process. A successor daemon reuses numbers, which is
/// safe because it hosts none of its predecessor's sessions: Kotlin ends every chain that a read
/// omits, and starts a session on a downstream only after reverting or cleaning its previous
/// routing, which removes that downstream's chains for every MAC the routing allowed.
pub fn next_counter_generation() -> u64 {
    NEXT_GENERATION.fetch_add(1, Ordering::Relaxed)
}
