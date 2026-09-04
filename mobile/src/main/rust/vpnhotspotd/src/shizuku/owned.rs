/// One owned byte buffer that never reallocates beyond its announced message capacity.
pub(crate) struct Owned {
    bytes: Vec<u8>,
}

impl Owned {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// An empty buffer sized to the message length already accepted by its protocol owner.
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
}

/// Where a framed DNS message's bytes go. The contract is [Owned::extend_within_capacity]'s: a buffer sized
/// from its frame may be filled and may never grow past that frame.
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
