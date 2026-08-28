//! What one terminated TCP flow's pieces are sized at, and what the whole of it costs before one may exist.
//!
//! Both halves live here rather than beside the engine because they are one statement made twice otherwise:
//! the same [Sizing] is what the charge is computed from and what the flow is *built* from, so the bridge
//! cannot be one capacity in the reservation and another in the stream. It is also the half of the engine
//! that is arithmetic rather than I/O, which is what makes it the half a host test can actually check - the
//! engine itself is in a binary target that runs no tests - which is also why the *order* that engine
//! handles a packet in lives in [crate::shared::ingress] rather than beside it.
//!
//! # What a flow's bytes really are
//!
//! Five buffers dominate, and they are the same five however the flow is used: the client-side stack's send
//! and receive buffers, the two directions of the bounded byte bridge its worker crosses, and the one-way
//! tail reserved for the client's ending. Each of the last three is one `tokio::io::simplex` pipe, named
//! separately here because it is separately bounded. Beside them sit the fixed scratch the worker reads into
//! and the two depth-one control channels a DNS-over-TCP transport uses. There is no
//! payload queue and no chunk in flight, because no chunk exists: bytes are copied into the bridge's own
//! buffer and out of it into `smoltcp`'s, and what bounds each direction is that buffer rather than a count
//! of allocations.

use crate::shared::reply_bound::{built_depth, channel_footprint};

/// Buffer per direction for one terminated TCP flow.
///
/// 65535 is the largest window a receiver can advertise without RFC 1323 window scaling, so it is the largest
/// buffer that is useful against *every* peer rather than only against those that negotiate scaling. Rounded
/// to 64 KiB. Bigger would help some peers and cost every flow; smaller would cap throughput on any path whose
/// bandwidth-delay product exceeds it.
pub const FLOW_BUFFER: usize = 64 * 1024;

/// One read from the upstream socket into the bridge, and one read out of the bridge by a DNS-over-TCP
/// transport.
///
/// Sized to what one segment toward the client can carry. What that buys is no longer a segment boundary -
/// the bridge is a byte stream and `smoltcp` segments it to the client's MSS whatever arrives - but the
/// syscall granularity of the upstream half, which is the figure this daemon's throughput was last measured
/// at. Each direction of [tokio::io::copy_bidirectional_with_sizes] gets one, and a DNS-over-TCP transport
/// gets one for the stream it frames questions out of.
pub const READ_CHUNK: usize = 1500;

/// How many bytes one direction of a flow's bridge may hold before its writer must wait.
///
/// One client-side stack buffer, and derived from it in both directions rather than picked:
///
/// - **upstream to client**, the bridge is what the upstream half reads ahead into while the engine is
///   writing what it read before. The engine's destination is the client's send buffer, so a bridge deeper
///   than that buffer would hold bytes across a client round trip rather than across a scheduling gap. Bridge
///   plus send buffer is the whole of what this direction may hold, which is what it held before: one send
///   buffer of queued read-ahead and one of stack buffer.
/// - **client to upstream**, it is what the engine may take out of the client's receive buffer before the
///   worker has written it upstream. The client cannot have sent more than that buffer holds, so the same
///   figure bounds it - and a full bridge is what stops the engine draining the receive buffer, which closes
///   the client's window rather than dropping a byte.
///
/// What it has to be deeper than is one segment: a bridge that holds one is a scheduling rendezvous per
/// segment, which is exactly what a read-ahead exists to remove.
pub const BRIDGE_BUFFER: usize = FLOW_BUFFER;

/// How deep each of a flow's control channels is built.
///
/// One, because each of them carries one thing at a time by construction: the answer to a reservation a
/// DNS-over-TCP transport asked for, and the query travelling back to the owner that granted it. Nobody
/// produces a second before the first is taken, so a deeper channel would be capacity charged for and never
/// used.
pub const CONTROL_DEPTH: usize = 1;

/// How large the pieces of one flow are. One value, read by the charge and by the construction.
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
///
/// The client-side receive buffer, exactly - the tail exists to take everything still in that buffer when
/// the client's FIN arrives, in one go, and that buffer cannot hold more than it is. Derived here rather
/// than stored beside [Sizing::buffer] because the construction derives it too, from the socket itself
/// (`shared::bridge::TailCapacity::of`), and two figures that must be equal are two figures that can differ.
/// An earlier version kept both and guarded them with a debug assertion, which is no guard at all in
/// release: a change to `buffer` alone would have undercharged every flow in production.
///
/// A byte less is not slower; it is *fatal*. The extraction is one uninterruptible step, because the owner
/// polls the stack between crossings and a `TIME-WAIT` socket clears its receive buffer inside one - so a
/// tail that cannot take the whole ending at once has no second chance, and the crossing ends the flow
/// abortively rather than closing it cleanly over a truncated stream. See [crate::shared::bridge].
pub fn tail_bytes(sizing: &Sizing) -> usize {
    sizing.buffer
}

/// What production builds and charges every flow at.
pub const SIZING: Sizing = Sizing {
    buffer: FLOW_BUFFER,
    quantum: READ_CHUNK,
    bridge: BRIDGE_BUFFER,
    control: CONTROL_DEPTH,
};

/// What one `tokio::io::simplex` pipe can really have allocated, as a multiple of the bytes it will admit.
///
/// Four, and it is the library's arithmetic rather than a margin of ours. A direction is a `BytesMut` whose
/// *length* tokio holds at or below `max_buf_size` (`tokio 1.53.1`, `io/util/mem.rs`,
/// `SimplexStream::poll_write_internal`), but whose *capacity* is `Vec`'s amortized doubling. Writing `M`
/// for that maximum, `A` for the allocation, `off` for how far the reader has advanced and `len` for what is
/// live, `bytes 1.12.1` (`src/bytes_mut.rs:626-686`) reallocates only when it can neither satisfy the write
/// from the tail nor reclaim the head - the reclaim wanting `off >= len` - and then takes
/// `max(2A, off + len + additional)`. A reallocation therefore happens only while `A` is under
/// `off + len + additional`, and with `off` under `len` and `len + additional` at most `M` that bound is
/// below `2M`; the allocation it produces is below `4M`.
///
/// Charged rather than assumed away, because the alternative is a flow holding memory the aggregate believes
/// is free. The realistic figure is nearer three, since the doubling starts from one [READ_CHUNK] write - it
/// is not tightened here, because a bound that has to be re-derived from a runtime's growth policy every time
/// a dependency moves is worth less than the bytes it saves.
const PIPE_ALLOCATION_FACTOR: u64 = 4;

/// What one pipe keeps beyond its buffer: the `SimplexStream` itself - two waker slots, a closed flag, the
/// maximum and the `BytesMut` header - inside the `Arc`'d mutex `simplex` puts both halves behind.
///
/// Deliberately more than that layout needs rather than a reproduction of it: it is crate-private, this is
/// charged three times per flow, and a generous figure costs a few kilobytes across a whole dataplane while
/// buying a bound that does not have to be revisited when a runtime version reorganises itself.
const PIPE_BYTES: u64 = 256;

/// One pipe of `admits` bytes: its buffer at the capacity it can really reach, and the pipe state around it.
fn pipe_footprint(admits: usize) -> Option<u64> {
    u64::try_from(admits)
        .ok()?
        .checked_mul(PIPE_ALLOCATION_FACTOR)?
        .checked_add(PIPE_BYTES)
}

/// Both directions of one flow's bridge, at the capacity each of them can really reach.
pub fn bridge_footprint(sizing: &Sizing) -> Option<u64> {
    pipe_footprint(sizing.bridge)?.checked_mul(2)
}

/// The reserved terminal tail: one pipe, charged like the other two.
///
/// At the same factor as the steady-state directions, deliberately. An earlier figure halved it on the
/// grounds that the tail is written once from empty and never read while it fills - but the extraction closes
/// the downward pipe *first*, so a worker that is keeping up starts draining the tail while the writes into
/// it are still happening. That is an ordinary producer and consumer, so it gets the ordinary bound.
pub fn tail_footprint(sizing: &Sizing) -> Option<u64> {
    pipe_footprint(tail_bytes(sizing))
}

/// The fixed scratch a flow's worker reads into: one per direction of the bidirectional copy an ordinary flow
/// runs.
///
/// A DNS-over-TCP transport owns fewer, not more - it frames one stream and holds one such buffer - and is
/// charged the same figure deliberately, because one preparation builds both kinds.
pub fn scratch_bytes(sizing: &Sizing) -> Option<u64> {
    u64::try_from(sizing.quantum).ok()?.checked_mul(2)
}

/// Every bounded channel one flow owns, at the message types and depths they are really built at.
///
/// `Payload` is what one byte buffer is carried as and `Control` what the DNS-over-TCP control channel
/// carries; both are the caller's real types, so a figure here cannot drift from the messages that are
/// actually queued.
///
/// One producer each: these two are point to point between one flow's own worker and its owner, and neither
/// end clones its sender, so neither can have a second sender in a grow race - see
/// [crate::shared::reply_bound].
pub fn channels_footprint<Payload, Control>(sizing: &Sizing) -> Option<u64> {
    let control = built_depth(sizing.control);
    // The owner's answers to a DNS-over-TCP transport: a reservation's outcome, or a published query's...
    channel_footprint::<Control>(control, 1)?
        // ...and the exact filled query on its way back to the owner that granted it.
        .checked_add(channel_footprint::<Payload>(control, 1)?)
}

/// What one flow really costs the aggregate: its stack buffers, both directions of its bridge, the scratch
/// its worker reads into, and every channel it owns.
///
/// The five large buffers dominate - two stack buffers and three pipes - which is the whole reason a flow is
/// not "one record": counting it as one would let memory run out long before descriptors did.
///
/// Checked throughout: a figure that would wrap is a flow that cannot be accounted for and therefore must not
/// be built, which is what `None` says.
pub fn footprint<Payload, Control>(sizing: &Sizing) -> Option<u64> {
    2u64.checked_mul(u64::try_from(sizing.buffer).ok()?)?
        .checked_add(bridge_footprint(sizing)?)?
        .checked_add(tail_footprint(sizing)?)?
        .checked_add(scratch_bytes(sizing)?)?
        .checked_add(channels_footprint::<Payload, Control>(sizing)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-ins for the engine's own message types, at the shapes they really have: one owned byte buffer,
    /// and one control message that carries either an outcome or a buffer. Never constructed, because what
    /// the arithmetic reads of them is their size and alignment.
    type Payload = Vec<u8>;
    #[allow(dead_code)]
    enum Control {
        Granted(Option<Payload>),
        Answered(u64, Payload),
    }

    fn footprints(sizing: &Sizing) -> Option<u64> {
        footprint::<Payload, Control>(sizing)
    }

    #[test]
    fn the_bridge_is_charged_at_what_it_can_allocate_rather_than_what_it_admits() {
        // Both directions, at the multiple of the admitted maximum the runtime's growth policy allows.
        assert_eq!(
            bridge_footprint(&SIZING),
            Some(2 * (PIPE_ALLOCATION_FACTOR * BRIDGE_BUFFER as u64 + PIPE_BYTES))
        );
        // And what it admits is a whole client-side stack buffer, which is what the read-ahead has to be:
        // deeper than one segment, and no deeper than the buffer the engine is writing into.
        assert_eq!(BRIDGE_BUFFER, FLOW_BUFFER);
        const { assert!(BRIDGE_BUFFER > READ_CHUNK) };
    }

    #[test]
    fn the_reserved_tail_is_charged_before_a_flow_may_exist() {
        // One pipe rather than the bridge's two, and at the same multiple as they are: the extraction closes
        // the main stream first, so a worker that is keeping up drains this pipe while it is still being
        // written.
        assert_eq!(
            tail_footprint(&SIZING),
            Some(PIPE_ALLOCATION_FACTOR * FLOW_BUFFER as u64 + PIPE_BYTES)
        );
        // And it is the receive buffer, not a second figure that happens to equal it. There is one axis, so
        // the charge cannot be left behind by a change to the buffer - which is exactly what a release build
        // used to allow, because the two were only tied together by a debug assertion.
        let wider = Sizing {
            buffer: SIZING.buffer + 4_096,
            ..SIZING
        };
        assert_eq!(tail_bytes(&wider), wider.buffer);
        assert_eq!(
            tail_footprint(&wider),
            Some(PIPE_ALLOCATION_FACTOR * wider.buffer as u64 + PIPE_BYTES),
            "the reserved tail follows the receive buffer"
        );
        // And the whole charge moves with it, by both the stack buffers and the tail.
        let widened = footprints(&wider).expect("fits") - footprints(&SIZING).expect("fits");
        assert_eq!(
            widened,
            2 * 4_096 + PIPE_ALLOCATION_FACTOR * 4_096,
            "two stack buffers and one reserved tail, and nothing counted twice"
        );
        // A figure that would wrap is a flow that cannot be accounted for, so it is denied rather than built.
        assert_eq!(
            tail_footprint(&Sizing {
                buffer: usize::MAX,
                ..SIZING
            }),
            None
        );
        assert_eq!(
            footprints(&Sizing {
                buffer: usize::MAX,
                ..SIZING
            }),
            None,
            "and the denial reaches the whole charge"
        );
    }

    #[test]
    fn the_whole_figure_is_the_buffers_the_bridge_the_tail_the_scratch_and_the_channels() {
        let charged = footprints(&SIZING).expect("production sizing fits");
        assert_eq!(
            charged,
            2 * FLOW_BUFFER as u64
                + bridge_footprint(&SIZING).expect("bridge fits")
                + tail_footprint(&SIZING).expect("tail fits")
                + scratch_bytes(&SIZING).expect("scratch fits")
                + channels_footprint::<Payload, Control>(&SIZING).expect("channels fit")
        );
        // Nothing is charged twice and nothing is left out: the parts are the whole.
        assert!(charged > 2 * FLOW_BUFFER as u64 + bridge_footprint(&SIZING).expect("bridge fits"));
    }

    #[test]
    fn a_figure_that_would_wrap_is_a_flow_that_cannot_be_charged() {
        assert_eq!(
            bridge_footprint(&Sizing {
                bridge: usize::MAX,
                ..SIZING
            }),
            None
        );
        assert_eq!(
            scratch_bytes(&Sizing {
                quantum: usize::MAX,
                ..SIZING
            }),
            None
        );
        assert_eq!(
            footprints(&Sizing {
                buffer: usize::MAX,
                ..SIZING
            }),
            None
        );
    }

    #[test]
    fn every_axis_of_the_sizing_costs_something() {
        // A charge that ignored one of its inputs would be an undercharge nobody could see from the total.
        for larger in [
            Sizing {
                buffer: SIZING.buffer + 1,
                ..SIZING
            },
            Sizing {
                quantum: SIZING.quantum + 1,
                ..SIZING
            },
            Sizing {
                bridge: SIZING.bridge + 1,
                ..SIZING
            },
            Sizing {
                control: SIZING.control + 32,
                ..SIZING
            },
        ] {
            assert!(
                footprints(&larger) > footprints(&SIZING),
                "{larger:?} must cost more than {SIZING:?}"
            );
        }
        // And a derived depth of zero is still a real channel: one slot is built, so one is charged rather
        // than assumed free.
        assert_eq!(
            channels_footprint::<Payload, Control>(&Sizing {
                control: 0,
                ..SIZING
            }),
            channels_footprint::<Payload, Control>(&Sizing {
                control: 1,
                ..SIZING
            })
        );
    }
}
