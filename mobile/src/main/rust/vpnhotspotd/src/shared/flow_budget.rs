/// Matches pinned smoltcp's streaming-server buffers. Full buffers apply TCP backpressure.
/// <https://github.com/smoltcp-rs/smoltcp/blob/v0.13.1/examples/server.rs>
pub const FLOW_BUFFER: usize = u16::MAX as usize;

/// Bytes in one upstream-socket or DNS-over-TCP scratch read. This matches pinned Tokio 1.53.1's
/// `DEFAULT_BUF_SIZE`, used by `io::copy` and `copy_bidirectional`:
/// <https://github.com/tokio-rs/tokio/blob/tokio-1.53.1/tokio/src/io/util/mod.rs>.
/// Filling the scratch limits that read only; the readiness-driven loop immediately continues.
pub const READ_CHUNK: usize = 8 * 1024;

/// Matches the adjacent smoltcp direction; a full bridge propagates backpressure.
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
