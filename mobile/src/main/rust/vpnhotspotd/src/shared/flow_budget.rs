/// Bytes in each direction of one terminated client-side TCP socket. The 16-bit Window field in
/// RFC 9293 section 3.1 makes `u16::MAX` the largest window TCP can advertise without negotiating
/// window scaling, and pinned smoltcp 0.13.1 uses the same capacity for both directions of its
/// streaming server sockets:
/// <https://www.rfc-editor.org/rfc/rfc9293.html#section-3.1>,
/// <https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/examples/server.rs>.
/// A full smoltcp buffer applies TCP backpressure; it does not evict queued bytes.
pub const FLOW_BUFFER: usize = u16::MAX as usize;

/// Bytes in one upstream-socket or DNS-over-TCP scratch read. This matches pinned Tokio 1.53.1's
/// `DEFAULT_BUF_SIZE`, used by `io::copy` and `copy_bidirectional`:
/// <https://github.com/tokio-rs/tokio/blob/tokio-1.53.1/tokio/src/io/util/mod.rs>.
/// Filling the scratch limits that read only; the readiness-driven loop immediately continues.
pub const READ_CHUNK: usize = 8 * 1024;

/// Bytes one direction of the Tokio bridge may hold. Matching the adjacent smoltcp direction avoids
/// introducing a narrower handoff than its 65,535-byte TCP buffer. A full bridge suspends its writer
/// and propagates backpressure without dropping bytes.
pub const BRIDGE_BUFFER: usize = FLOW_BUFFER;

/// One slot in each DNS-over-TCP control direction. A transport processes one framed query at a time, so one
/// slot holds every message the protocol can legitimately have pending. A refused query-to-owner handoff ends
/// the transport. A second owner-to-transport control is discarded and counted unreachable; closure means the
/// transport has already ended. Neither channel grows.
pub const CONTROL_DEPTH: usize = 1;

/// How large the pieces of one flow are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizing {
    /// One client-side stack buffer, each way.
    pub buffer: usize,
    /// The scratch the worker reads into, once per direction.
    pub quantum: usize,
    /// How many bytes one direction of the bridge may hold.
    pub bridge: usize,
    /// Depth of each control channel.
    pub control: usize,
}

/// How many bytes the reserved terminal tail may hold, which is not a figure anyone chooses.
pub fn tail_bytes(sizing: &Sizing) -> usize {
    sizing.buffer
}

/// What production builds every flow at.
pub const SIZING: Sizing = Sizing {
    buffer: FLOW_BUFFER,
    quantum: READ_CHUNK,
    bridge: BRIDGE_BUFFER,
    control: CONTROL_DEPTH,
};
