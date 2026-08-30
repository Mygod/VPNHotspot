use std::collections::{hash_map::RandomState, HashMap};
use std::hash::BuildHasher;
use std::net::Ipv4Addr;

/// The fields IPv4 reassembly uses in addition to the Identification.
pub type Tuple = (Ipv4Addr, Ipv4Addr, u8);

/// Per-session IPv4 Identification sequences.
///
/// Each reassembly tuple owns a wrapping `u16` sequence. Its first value is a keyed hash from a fresh
/// [RandomState], so restarting the daemon does not restart every tuple at a predictable common value.
/// This preserves the useful properties of Linux's `__ip_select_ident`, which hashes source, destination,
/// and protocol under a per-network-namespace secret before advancing an Identification generator, while
/// keeping exact tuple-local state instead of Linux's shared finite bucket table:
/// <https://github.com/torvalds/linux/blob/master/net/ipv4/route.c>.
/// Tuple state grows with the downstream traffic observed by this session; there is deliberately no
/// per-client denial cap or reuse quarantine because downstream clients are trusted and restarting the
/// session discards the table.
pub struct Ipv4Identifications {
    initial: RandomState,
    sequences: HashMap<Tuple, u16>,
}

impl Default for Ipv4Identifications {
    fn default() -> Self {
        Self::new()
    }
}

impl Ipv4Identifications {
    pub fn new() -> Self {
        Self {
            initial: RandomState::new(),
            sequences: HashMap::new(),
        }
    }

    /// Returns this tuple's next Identification, wrapping after the complete 16-bit protocol space.
    pub fn next(&mut self, tuple: Tuple) -> u16 {
        if let Some(identification) = self.sequences.get_mut(&tuple) {
            *identification = identification.wrapping_add(1);
            return *identification;
        }
        let identification = self.initial.hash_one(tuple) as u16;
        self.sequences.insert(tuple, identification);
        identification
    }

    pub fn len(&self) -> usize {
        self.sequences.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sequences.is_empty()
    }

    pub fn describe(&self) -> String {
        format!("ipv4-identification tuples {}", self.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);
    const B: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 2);
    const REMOTE: Ipv4Addr = Ipv4Addr::new(198, 51, 100, 1);

    #[test]
    fn tuples_have_independent_wrapping_sequences() {
        let mut identifications = Ipv4Identifications::new();
        let a = (A, REMOTE, 17);
        let b = (B, REMOTE, 17);
        let first_a = identifications.next(a);
        let first_b = identifications.next(b);

        assert_eq!(identifications.next(a), first_a.wrapping_add(1));
        assert_eq!(identifications.next(a), first_a.wrapping_add(2));
        assert_eq!(identifications.next(b), first_b.wrapping_add(1));
        assert_eq!(identifications.len(), 2);
    }

    #[test]
    fn a_sequence_wraps_without_denial() {
        let mut identifications = Ipv4Identifications::new();
        let tuple = (A, REMOTE, 17);
        identifications.sequences.insert(tuple, u16::MAX);

        assert_eq!(identifications.next(tuple), 0);
        assert_eq!(identifications.next(tuple), 1);
    }

    #[test]
    fn describe_counts_dynamic_tuple_state() {
        let mut identifications = Ipv4Identifications::new();
        assert!(identifications.is_empty());
        assert_eq!(identifications.describe(), "ipv4-identification tuples 0");
        identifications.next((A, REMOTE, 17));
        assert_eq!(identifications.describe(), "ipv4-identification tuples 1");
    }
}
