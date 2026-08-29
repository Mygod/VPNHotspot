use crate::shared::reply_bound::{built_depth, channel_footprint};

/// Buffer per direction for one terminated TCP flow.
pub const FLOW_BUFFER: usize = 64 * 1024;

/// One read from the upstream socket into the bridge, and one read out of the bridge by a DNS-over-TCP
/// transport.
pub const READ_CHUNK: usize = 1500;

/// How many bytes one direction of a flow's bridge may hold before its writer must wait.
pub const BRIDGE_BUFFER: usize = FLOW_BUFFER;

/// How deep each of a flow's control channels is built.
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
const PIPE_ALLOCATION_FACTOR: u64 = 4;

/// What one pipe keeps beyond its buffer: the `SimplexStream` itself - two waker slots, a closed flag, the
/// maximum and the `BytesMut` header - inside the `Arc`'d mutex `simplex` puts both halves behind.
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
pub fn tail_footprint(sizing: &Sizing) -> Option<u64> {
    pipe_footprint(tail_bytes(sizing))
}

/// The fixed scratch a flow's worker reads into: one per direction of the bidirectional copy an ordinary flow
/// runs.
pub fn scratch_bytes(sizing: &Sizing) -> Option<u64> {
    u64::try_from(sizing.quantum).ok()?.checked_mul(2)
}

/// Every bounded channel one flow owns, at the message types and depths they are really built at.
pub fn channels_footprint<Payload, Control>(sizing: &Sizing) -> Option<u64> {
    let control = built_depth(sizing.control);
    // The owner's answers to a DNS-over-TCP transport: a reservation's outcome, or a published query's...
    channel_footprint::<Control>(control, 1)?
        // ...and the exact filled query on its way back to the owner that granted it.
        .checked_add(channel_footprint::<Payload>(control, 1)?)
}

/// What one flow really costs the aggregate: its stack buffers, both directions of its bridge, the scratch
/// its worker reads into, and every channel it owns.
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
        assert_eq!(
            bridge_footprint(&SIZING),
            Some(2 * (PIPE_ALLOCATION_FACTOR * BRIDGE_BUFFER as u64 + PIPE_BYTES))
        );
        assert_eq!(BRIDGE_BUFFER, FLOW_BUFFER);
        const { assert!(BRIDGE_BUFFER > READ_CHUNK) };
    }

    #[test]
    fn the_reserved_tail_is_charged_before_a_flow_may_exist() {
        assert_eq!(
            tail_footprint(&SIZING),
            Some(PIPE_ALLOCATION_FACTOR * FLOW_BUFFER as u64 + PIPE_BYTES)
        );
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
        let widened = footprints(&wider).expect("fits") - footprints(&SIZING).expect("fits");
        assert_eq!(
            widened,
            2 * 4_096 + PIPE_ALLOCATION_FACTOR * 4_096,
            "two stack buffers and one reserved tail, and nothing counted twice"
        );
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
