//! What one terminated TCP flow's pieces are sized at, and what the whole of it costs before one may exist.
//!
//! Both halves live here rather than beside the engine because they are one statement made twice otherwise:
//! the same [Sizing] is what the charge is computed from and what the flow is *built* from, so a queue cannot
//! be one depth in the reservation and another in the channel. It is also the half of the engine that is
//! arithmetic rather than I/O, which is what makes it the half a host test can actually check - the engine
//! itself is in a binary target that runs no tests.
//!
//! # What a flow's bytes really are
//!
//! Two stack buffers dominate, and beside them sit the chunk-sized buffers that can exist *at the same
//! moment*. That last word is the whole of the arithmetic: with a read-ahead queue the two directions no
//! longer peak alternately, because what is queued is outside the splice's own `select!` branches and
//! therefore exists whichever branch it is in. See [payload_bytes] for the term-by-term list.
//!
//! Channels are charged from [crate::shared::reply_bound], at the message types and depths they are really
//! built at, plus the one heap allocation the owner's reserved slot keeps - see [crate::shared::room].

use crate::shared::mailbox::Chunk;
use crate::shared::reply_bound::{built_depth, channel_footprint};
use crate::shared::room::ACQUIRE_FUTURE_BYTES;

/// Buffer per direction for one terminated TCP flow.
///
/// 65535 is the largest window a receiver can advertise without RFC 1323 window scaling, so it is the largest
/// buffer that is useful against *every* peer rather than only against those that negotiate scaling. Rounded
/// to 64 KiB. Bigger would help some peers and cost every flow; smaller would cap throughput on any path whose
/// bandwidth-delay product exceeds it.
pub const FLOW_BUFFER: usize = 64 * 1024;

/// One read from the upstream socket. Sized to what one segment toward the client can carry, so a full read
/// turns into one segment rather than being re-split by the stack.
pub const READ_CHUNK: usize = 1500;

/// How many read quanta one flow's upstream half may queue ahead of the client's stack.
///
/// One client-side send buffer's worth, and derived from it rather than picked: the engine can never hand a
/// client more than its send buffer holds before that client acknowledges something, so a queue deeper than
/// this would be bytes held across a client round trip rather than across a scheduling gap - memory this
/// daemon owns that the stack it is feeding could not have taken even if the engine ran instantly. What it
/// has to be deeper than is one: at depth one the upstream half cannot read while the engine writes, which is
/// a scheduling rendezvous per segment.
///
/// Every chunk it may hold is charged before the flow exists - see [footprint] - so a device that cannot
/// afford the read-ahead admits fewer flows rather than overrunning its budget.
pub const READ_AHEAD: usize = FLOW_BUFFER / READ_CHUNK;

/// How deep each of a flow's control channels is built.
///
/// One, because each of them carries one thing at a time by construction: a chunk toward the upstream half,
/// the consumption acknowledgment a DNS-over-TCP transport waits for, the answer to a reservation it asked
/// for, and the query travelling back to the owner that granted it. Nobody produces a second before the first
/// is taken, so a deeper channel would be capacity charged for and never used.
///
/// The payload queue toward the client is the exception and is built at [READ_AHEAD] instead: it is the one
/// channel whose producer may usefully run ahead of its consumer.
pub const CONTROL_DEPTH: usize = 1;

/// How large the pieces of one flow are. One value, read by the charge and by the construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sizing {
    /// One client-side stack buffer, each way.
    pub buffer: usize,
    /// The read quantum every payload chunk is sized to.
    pub quantum: usize,
    /// How many quanta the upstream half may queue ahead of the client's stack.
    pub read_ahead: usize,
    /// Depth of each control channel.
    pub control: usize,
}

/// What production builds and charges every flow at.
pub const SIZING: Sizing = Sizing {
    buffer: FLOW_BUFFER,
    quantum: READ_CHUNK,
    read_ahead: READ_AHEAD,
    control: CONTROL_DEPTH,
};

/// How many quantum-sized buffers one flow can hold at once, beside its two stack buffers.
///
/// - the read-ahead queue, in full, whether or not a given flow ever fills it;
/// - the upstream half's persistent read buffer, allocated once per flow;
/// - the one chunk the engine has taken out of the queue and is writing into the client's send buffer at an
///   exact offset, which is the fair queue's row;
/// - one chunk queued in the client-to-upstream direction, which is the depth the engine's reserved slot
///   fills;
/// - and one more for whichever chunk the flow's own task is holding - a payload it built and is queueing, or
///   a chunk it took from the client-to-upstream queue and is writing upstream. One rather than two, because
///   that task is one `select!` and is therefore in exactly one branch at a time.
///
/// A DNS-over-TCP transport owns fewer, not more: it has no upstream socket and therefore no read buffer, and
/// it hands its pieces over one at a time and waits for each to be consumed, so it never fills the read-ahead.
/// Charging both kinds the same figure is deliberate - one preparation builds both - and conservative.
pub fn payload_bytes(sizing: &Sizing) -> Option<u64> {
    u64::try_from(sizing.read_ahead)
        .ok()?
        .checked_add(4)?
        .checked_mul(u64::try_from(sizing.quantum).ok()?)
}

/// Every bounded channel one flow owns, at the message types and depths they are really built at, plus what
/// the owner's reserved slot keeps beyond its channel.
///
/// `Payload` is what one byte buffer is carried as and `Control` what the DNS-over-TCP control channel
/// carries; both are the caller's real types, so a figure here cannot drift from the messages that are
/// actually queued.
///
/// One producer each, every one of them: these five are point to point between one flow's own worker and its
/// owner, and neither end clones its sender. So none of them can have a second sender in a grow race - see
/// [crate::shared::reply_bound]. The engine's end of the first is a room, which holds two sender handles
/// rather than one; both belong to that same owner and only one of them can be inside a send, so the producer
/// count is still one.
pub fn channels_footprint<Payload, Control>(sizing: &Sizing) -> Option<u64> {
    let control = built_depth(sizing.control);
    // The client-to-upstream payload channel...
    channel_footprint::<Payload>(control, 1)?
        // ...and what the engine's end of it keeps beyond the channel: the boxed reservation that registers
        // the owner to be woken when the flow's task frees the slot.
        .checked_add(ACQUIRE_FUTURE_BYTES)?
        // The read-ahead queue toward the client's stack, at the depth it is really built at...
        .checked_add(channel_footprint::<Chunk<Payload>>(
            built_depth(sizing.read_ahead),
            1,
        )?)?
        // ...and the consumption acknowledgment a DNS-over-TCP transport waits on, which is depth one
        // because only that kind waits and it waits for one piece at a time.
        .checked_add(channel_footprint::<()>(control, 1)?)?
        // The owner's answers to that transport: a reservation's outcome, or a published query's.
        .checked_add(channel_footprint::<Control>(control, 1)?)?
        // ...and the exact filled query on its way back to the owner that granted it.
        .checked_add(channel_footprint::<Payload>(control, 1)?)
}

/// What one flow really costs the aggregate: its stack buffers, every payload chunk that can exist at once,
/// and every channel it owns.
///
/// The two 64 KiB buffers dominate, which is the whole reason a flow is not "one record": counting it as one
/// would let memory run out long before descriptors did.
///
/// Checked throughout: a figure that would wrap is a flow that cannot be accounted for and therefore must not
/// be built, which is what `None` says.
pub fn footprint<Payload, Control>(sizing: &Sizing) -> Option<u64> {
    2u64.checked_mul(u64::try_from(sizing.buffer).ok()?)?
        .checked_add(payload_bytes(sizing)?)?
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
    fn the_payload_terms_are_the_read_ahead_and_the_four_beside_it() {
        // The queue in full, plus the read buffer, the row, the one chunk queued toward the upstream half,
        // and the one the flow's own task is holding.
        assert_eq!(
            payload_bytes(&SIZING),
            Some((READ_AHEAD as u64 + 4) * READ_CHUNK as u64)
        );
        // And that is what the engine is really prepared to hold: one send buffer's worth of read-ahead,
        // which has to be deeper than the one-chunk rendezvous it replaces.
        assert_eq!(READ_AHEAD, FLOW_BUFFER / READ_CHUNK);
        const { assert!(READ_AHEAD > 1) };
    }

    #[test]
    fn the_whole_figure_is_the_buffers_the_payload_and_the_channels() {
        let charged = footprints(&SIZING).expect("production sizing fits");
        assert_eq!(
            charged,
            2 * FLOW_BUFFER as u64
                + payload_bytes(&SIZING).expect("payload fits")
                + channels_footprint::<Payload, Control>(&SIZING).expect("channels fit")
        );
        // Nothing is charged twice and nothing is left out: the parts are the whole.
        assert!(charged > 2 * FLOW_BUFFER as u64 + payload_bytes(&SIZING).expect("payload fits"));
    }

    #[test]
    fn the_read_ahead_queue_is_charged_at_the_depth_it_is_built_at() {
        // A deeper queue is a larger charge, in both the payload it may hold and the channel that holds it.
        let deeper = Sizing {
            read_ahead: SIZING.read_ahead + 16,
            ..SIZING
        };
        assert!(payload_bytes(&deeper) > payload_bytes(&SIZING));
        assert!(
            channels_footprint::<Payload, Control>(&deeper)
                > channels_footprint::<Payload, Control>(&SIZING)
        );
        assert!(footprints(&deeper) > footprints(&SIZING));
        // And a derived depth of zero is still a real channel: one slot is built, so one is charged rather
        // than assumed free.
        let none = Sizing {
            read_ahead: 0,
            ..SIZING
        };
        let one = Sizing {
            read_ahead: 1,
            ..SIZING
        };
        assert_eq!(
            channels_footprint::<Payload, Control>(&none),
            channels_footprint::<Payload, Control>(&one)
        );
    }

    #[test]
    fn the_reservation_the_owner_keeps_is_charged_with_the_channels() {
        // The room's boxed acquisition is not free and is not padding: it is a named term, so removing it
        // would be visible here rather than in a device's memory.
        let sizing = Sizing {
            read_ahead: 1,
            ..SIZING
        };
        let channels = channels_footprint::<Payload, Control>(&sizing).expect("fits");
        let without = channel_footprint::<Payload>(1, 1).expect("fits")
            + channel_footprint::<Chunk<Payload>>(1, 1).expect("fits")
            + channel_footprint::<()>(1, 1).expect("fits")
            + channel_footprint::<Control>(1, 1).expect("fits")
            + channel_footprint::<Payload>(1, 1).expect("fits");
        assert_eq!(channels, without + ACQUIRE_FUTURE_BYTES);
    }

    #[test]
    fn a_figure_that_would_wrap_is_a_flow_that_cannot_be_charged() {
        assert_eq!(
            payload_bytes(&Sizing {
                read_ahead: usize::MAX,
                ..SIZING
            }),
            None
        );
        assert_eq!(
            payload_bytes(&Sizing {
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
        assert_eq!(
            channels_footprint::<Payload, Control>(&Sizing {
                read_ahead: usize::MAX,
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
                read_ahead: SIZING.read_ahead + 1,
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
    }
}
