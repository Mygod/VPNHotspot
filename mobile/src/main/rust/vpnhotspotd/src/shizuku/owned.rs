/// One owned byte buffer that never reallocates beyond the capacity its owner was granted.
pub(crate) struct Owned {
    bytes: Vec<u8>,
}

impl Owned {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// An empty buffer at a capacity its owner has already been granted.
    pub(crate) fn with_capacity(bytes: usize) -> Self {
        Self::new(Vec::with_capacity(bytes))
    }

    /// Appends as much of `bytes` as the granted capacity still has room for, and answers how much that was.
    pub(crate) fn extend_within_capacity(&mut self, bytes: &[u8]) -> usize {
        let room = self.bytes.capacity() - self.bytes.len();
        let taken = room.min(bytes.len());
        self.bytes.extend_from_slice(&bytes[..taken]);
        taken
    }

    /// What this buffer really cost, which is what it was counted at rather than what it currently holds.
    /// An owner reconciling a conservative reservation downward has to read the same figure the count was
    /// taken from, or the two would disagree about the same buffer.
    pub(crate) fn capacity(&self) -> usize {
        self.bytes.capacity()
    }
}

/// Where a framed DNS message's bytes go. The contract is [Owned::extend_within_capacity]'s: a buffer that
/// was charged before it existed may be filled and may never grow.
impl vpnhotspotd::shared::dns_wire::Body for Owned {
    fn extend_within_capacity(&mut self, bytes: &[u8]) -> usize {
        Owned::extend_within_capacity(self, bytes)
    }
}

impl std::ops::Deref for Owned {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for Owned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes", self.bytes.len())
    }
}
