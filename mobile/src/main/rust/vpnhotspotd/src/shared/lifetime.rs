//! How long an idle terminated TCP flow lives in this daemon's own outer state, and what activity does to
//! that.
//!
//! This is the outer, userspace half and nothing else. Android's inner IPv4 NAT keeps conntrack state of its
//! own for the same client, and none of it is mirrored, configured or timed from here: what the owner owns is
//! the flow record, its `smoltcp` socket and the worker behind them.
//!
//! Two floors, and RFC 5382 section 5's exact classification of which phase gets which. `FinWait1`,
//! `FinWait2` and `CloseWait` are *established* rather than transitory, because in each of them one
//! direction can still carry application data - a FIN in a header is not the connection being over, and a
//! client waiting out a long request it has finished sending is not idle. `TimeWait` is excluded from
//! REQ-5's transitory timeout by REQ-5 itself, and is left to smoltcp's own close timer - which in the
//! pinned `smoltcp 0.13.1` is `CLOSE_DELAY`, ten seconds, and not the two-minute 2MSL a Linux host waits.
//! Naming the real figure matters both ways: nothing here holds a flow for a conventional TIME-WAIT, and
//! nothing here shortens one either.
//!
//! **No post-RST retention is claimed.** RFC 7857 later recommends holding a mapping for four minutes after
//! a matching RST, which would need state outliving the live flow entirely. `Closed` is terminal here, and a
//! reset - the client's or this daemon's - ends the flow rather than starting a tombstone.
//!
//! # Why the policy is here rather than beside the engine
//!
//! Every function below is a pure map from a phase to a duration, and each was wrong at some point in a way
//! nothing observed. The one that matters most is [rearmed]: the ordinary mapping is exactly the wrong answer
//! for a flow whose client half-closed cleanly, and applying it there silently discards bytes this daemon had
//! already acknowledged. That is not a thing a table walk in an untested binary should decide.

use std::time::{Duration, Instant};

use smoltcp::socket::tcp::State;

/// RFC 5382 REQ-5's floor for an established connection: two hours four minutes.
const ESTABLISHED: Duration = Duration::from_secs(7_440);

/// RFC 5382 REQ-5's floor for a transitory one - partially open, or partially closed: four minutes.
const TRANSITORY: Duration = Duration::from_secs(240);

/// How long a flow in this phase may stay idle, or `None` where this owner has no say at all.
///
/// Exhaustive and without a wildcard, deliberately: a state smoltcp adds is a phase this table has no
/// opinion about yet, and a compile error is the only honest way to be told so.
pub fn floor(state: State) -> Option<Duration> {
    match state {
        // Partially open. A listening socket is a flow whose SYN has not reached the stack yet, which is the
        // same "no connection has been made" the transitory floor is for.
        State::Listen | State::SynSent | State::SynReceived => Some(TRANSITORY),
        // Established, and the three half-closed phases REQ-5 keeps with it because each can still carry
        // application data in one direction.
        State::Established | State::FinWait1 | State::FinWait2 | State::CloseWait => {
            Some(ESTABLISHED)
        }
        // Partially closed with nothing left to carry either way.
        State::Closing | State::LastAck => Some(TRANSITORY),
        // Named by REQ-5 as outside the transitory timeout, and outside this owner: the close wait is a
        // protocol timer and smoltcp holds it - ten seconds in the pinned version, `CLOSE_DELAY`, rather than
        // a Linux host's 2MSL. The owner's own combined deadline still schedules the wake it asks for.
        State::TimeWait => None,
        // Terminal. Reaching it is what begins an ordinary flow's retirement, and a zero floor is what says
        // this owner would not have waited either.
        State::Closed => Some(Duration::ZERO),
    }
}

/// When a flow in this phase next falls idle, measured from the activity that was just observed.
pub fn deadline(now: Instant, state: State) -> Option<Instant> {
    floor(state).map(|floor| now + floor)
}

/// Whether this phase is one a connection can only be in *after* its handshake completed.
///
/// The question the latch inside [crate::shared::bridge::Bridge] answers, and it has to be asked of the phase
/// rather than of one state: smoltcp accepts a third-handshake ACK that also carries FIN and goes
/// `SYN-RECEIVED` -> `CLOSE-WAIT` in a single step, without `ESTABLISHED` ever being observable
/// (smoltcp-0.13.1, `src/socket/tcp.rs:1880-1886`). A flow that watched only for `ESTABLISHED` therefore
/// never learned it was open, never propagated the client's half-close to its upstream, and sat on the
/// established floor with a peer waiting for bytes that would never come.
///
/// Exhaustive and without a wildcard, for the same reason [floor] is: a state smoltcp adds has to be
/// classified here rather than fall silently to one side.
pub fn opened(state: State) -> bool {
    match state {
        State::Listen | State::SynSent | State::SynReceived | State::Closed => false,
        State::Established
        | State::FinWait1
        | State::FinWait2
        | State::CloseWait
        | State::Closing
        | State::LastAck
        | State::TimeWait => true,
    }
}

/// Whether this phase proves the *client's* FIN has reached the stack.
///
/// The four phases a peer FIN produces and nothing else. `Closed` is deliberately outside: a reset reaches it
/// too, and a reset is owed no flush - see [Ending::Pending] for why that distinction is load-bearing.
///
/// Exhaustive and without a wildcard, for the same reason [floor] is.
pub(crate) fn peer_finished(state: State) -> bool {
    match state {
        State::CloseWait | State::Closing | State::LastAck | State::TimeWait => true,
        State::Listen
        | State::SynSent
        | State::SynReceived
        | State::Established
        | State::FinWait1
        | State::FinWait2
        | State::Closed => false,
    }
}

/// Whether this phase can still put application data in front of the client.
///
/// smoltcp answers exactly this with `may_send`, and answers it from the phase alone (smoltcp-0.13.1,
/// `src/socket/tcp.rs:1162-1171`): `ESTABLISHED` and `CLOSE-WAIT`, because in `CLOSE-WAIT` the remote has
/// closed *our* receive half and this side may still transmit indefinitely. Every other phase has either not
/// opened yet or already sent its own FIN, so no new byte can be put in front of the client from it.
///
/// Exhaustive and without a wildcard, for the same reason [floor] is.
fn carries_toward_client(state: State) -> bool {
    match state {
        State::Established | State::CloseWait => true,
        State::Listen
        | State::SynSent
        | State::SynReceived
        | State::FinWait1
        | State::FinWait2
        | State::Closing
        | State::LastAck
        | State::TimeWait
        | State::Closed => false,
    }
}

/// Where one flow stands in the clean client-ending lifecycle.
///
/// Derived from the bridge's own state and the client-side phase together - see
/// [crate::shared::bridge::Bridge::ending] - rather than from a flag kept beside them, because the two do
/// disagree, and it is exactly that disagreement the flush bound used to go missing in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Not a clean client ending, or not one yet: the client has not finished sending, this flow is being
    /// reset, or its worker has already gone. The phase's own floor applies.
    Ordinary,
    /// The stack has seen the client's FIN and the bridge has not propagated it yet.
    ///
    /// A transient *inside* one `accept`, and nothing longer-lived: after the poll that processed the packet
    /// and before the extraction a few steps later. That is what the rearm sees, and it is the order the
    /// packet boundary keeps rather than an accident of timing - the idle floor is armed *before* the ending
    /// is extracted, because a sealed flow is flushing and a flushing flow in a terminal phase preserves the
    /// deadline it already has - see [crate::shared::ingress]. When the daemon closed its own side first,
    /// that FIN can land the socket straight in `TIME-WAIT` - smoltcp goes `FIN-WAIT-1` + FIN + ack-of-FIN ->
    /// `TIME-WAIT` in one step
    /// (smoltcp-0.13.1, `src/socket/tcp.rs:1947-1957`), and `FIN-WAIT-2` + FIN likewise (`:1963-1966`) -
    /// whose floor is `None`. Rearming a pending flow against that gave it no outer bound at all, and ten
    /// seconds later `Closed` left it with no stack timer either: a worker blocked writing acknowledged bytes
    /// into an upstream zero window then held its flow, its descriptor and its admission for ever.
    Pending,
    /// The halt is propagated and the worker is flushing what the client sent.
    Flushing,
}

/// What one flow's idle deadline becomes when activity is observed on it.
///
/// `held` is the deadline the flow is carrying now, `state` the phase its socket ended up in, and `ending`
/// where it stands in the clean client-ending lifecycle.
///
/// # The three cases, and why a flush is never unbounded and never frozen
///
/// **Ordinary** takes the phase's own floor, which is all this ever used to do.
///
/// **Pending** takes the *established* floor rather than the phase's, and that is the correction: bytes the
/// client sent are still on their way through this daemon - in the receive buffer, about to cross into the
/// bridge and be written upstream - however far the client-facing half of the connection has torn down.
/// `TIME-WAIT` says nothing about them, because it is a statement about smoltcp's close timer and not about a
/// worker. So the floor that applies is the one for a connection that is still carrying application data, and
/// it is finite by construction.
///
/// **Flushing** splits, and the split is the second correction. `CLOSE-WAIT` and `ESTABLISHED` can still put
/// application data in front of the client - see [carries_toward_client] - so a halted flow in one of them is
/// an ordinary download whose client merely stopped sending, and every packet and every delivery is real
/// activity that must rearm it. Freezing it there expired a response that had never once been idle, purely
/// because it outlasted the floor its client's FIN happened to arm. Only the terminal phases, where no byte
/// can reach the client any more, preserve the finite deadline the flush already has - because there the
/// phase's own floor is `None` or zero, and either would take the flush's bound away or make it immediately
/// due and let the next expiry cancel a worker mid-flush.
pub fn rearmed(
    held: Option<Instant>,
    state: State,
    ending: Ending,
    now: Instant,
) -> Option<Instant> {
    match ending {
        Ending::Ordinary => deadline(now, state),
        // Finite by construction, and from an existing floor rather than a figure invented here.
        Ending::Pending => Some(now + ESTABLISHED),
        Ending::Flushing if carries_toward_client(state) => Some(now + ESTABLISHED),
        Ending::Flushing => held,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every phase smoltcp has, so a state it adds is a compile error here as well as in [floor].
    const EVERY: [State; 11] = [
        State::Closed,
        State::Listen,
        State::SynSent,
        State::SynReceived,
        State::Established,
        State::FinWait1,
        State::FinWait2,
        State::CloseWait,
        State::Closing,
        State::LastAck,
        State::TimeWait,
    ];

    #[test]
    fn every_phase_gets_the_floor_rfc_5382_gives_it() {
        for state in EVERY {
            let expected = match state {
                // Partially open.
                State::Listen | State::SynSent | State::SynReceived => Some(TRANSITORY),
                // Established, and the half-closed phases that can still carry application data.
                State::Established | State::FinWait1 | State::FinWait2 | State::CloseWait => {
                    Some(ESTABLISHED)
                }
                // Partially closed with nothing left to carry.
                State::Closing | State::LastAck => Some(TRANSITORY),
                // smoltcp's own close timer owns this one.
                State::TimeWait => None,
                State::Closed => Some(Duration::ZERO),
            };
            assert_eq!(floor(state), expected, "{state:?}");
        }
        assert_eq!(ESTABLISHED.as_secs(), 7_440);
        assert_eq!(TRANSITORY.as_secs(), 240);
    }

    #[test]
    fn a_deadline_is_the_floor_measured_from_the_activity_that_was_observed() {
        let now = Instant::now();
        assert_eq!(
            deadline(now, State::Established),
            Some(now + ESTABLISHED),
            "an established flow gets the long floor"
        );
        assert_eq!(deadline(now, State::LastAck), Some(now + TRANSITORY));
        assert_eq!(
            deadline(now, State::TimeWait),
            None,
            "smoltcp's timer, not ours"
        );
        assert_eq!(
            deadline(now, State::Closed),
            Some(now),
            "and a closed socket is due immediately"
        );
    }

    #[test]
    fn only_a_phase_past_the_handshake_counts_as_opened() {
        for state in EVERY {
            let expected = !matches!(
                state,
                State::Listen | State::SynSent | State::SynReceived | State::Closed
            );
            assert_eq!(opened(state), expected, "{state:?}");
        }
    }

    #[test]
    fn an_ordinary_flow_is_rearmed_to_the_phase_its_socket_ended_up_in() {
        let now = Instant::now();
        let stale = now - Duration::from_secs(3_600);
        for state in EVERY {
            assert_eq!(
                rearmed(Some(stale), state, Ending::Ordinary, now),
                deadline(now, state),
                "{state:?}: an ordinary flow takes its phase's floor"
            );
        }
    }

    #[test]
    fn only_the_four_phases_a_peer_fin_produces_prove_the_client_finished() {
        for state in EVERY {
            let expected = matches!(
                state,
                State::CloseWait | State::Closing | State::LastAck | State::TimeWait
            );
            assert_eq!(peer_finished(state), expected, "{state:?}");
        }
        // A reset reaches `Closed` too, so `Closed` may never stand for a clean ending.
        assert!(!peer_finished(State::Closed));
    }

    #[test]
    fn only_a_phase_that_may_still_send_carries_data_toward_the_client() {
        for state in EVERY {
            let expected = matches!(state, State::Established | State::CloseWait);
            assert_eq!(carries_toward_client(state), expected, "{state:?}");
        }
    }

    #[test]
    fn a_pending_flow_is_bounded_by_the_established_floor_in_every_phase_a_fin_produces() {
        let now = Instant::now();
        let stale = now - Duration::from_secs(3_600);
        // The window between the packet path seeing the client's FIN and a crossing propagating it. Every one
        // of these must come out finite: bytes the client sent are still on their way through this daemon,
        // whatever the client-facing half of the connection has done.
        for state in [
            State::CloseWait,
            State::Closing,
            State::LastAck,
            State::TimeWait,
        ] {
            assert_eq!(
                rearmed(Some(stale), state, Ending::Pending, now),
                Some(now + ESTABLISHED),
                "{state:?}: a pending clean close is bounded, and by an existing floor"
            );
        }
        // The one that used to be unbounded, stated on its own because it is the bug: the daemon closes
        // first, the client's payload and FIN arrive together acknowledging that FIN, and smoltcp lands the
        // socket straight in `TIME-WAIT` whose own floor is none at all.
        assert_eq!(
            rearmed(Some(stale), State::TimeWait, Ending::Ordinary, now),
            None,
            "which is exactly what the ordinary mapping would have given it"
        );
    }

    #[test]
    fn a_flushing_flow_rearms_while_it_can_still_reach_its_client_and_preserves_after() {
        let now = Instant::now();
        // The finite bound the flush is already carrying.
        let armed = now + Duration::from_secs(60);
        for state in EVERY {
            let expected = if carries_toward_client(state) {
                // Still an ordinary download whose client merely stopped sending: every packet and every
                // delivery is real activity, and freezing here expires a response that was never idle.
                Some(now + ESTABLISHED)
            } else {
                // No byte can reach the client from here, so the phase's own floor is none or zero - either
                // would take the flush's bound away or make it immediately due.
                Some(armed)
            };
            assert_eq!(
                rearmed(Some(armed), state, Ending::Flushing, now),
                expected,
                "{state:?}"
            );
        }
        // Named on their own, because these two are the ones the terminal rule exists for.
        assert_eq!(
            rearmed(Some(armed), State::TimeWait, Ending::Flushing, now),
            Some(armed)
        );
        assert_eq!(
            rearmed(Some(armed), State::Closed, Ending::Flushing, now),
            Some(armed)
        );
        // And the active one, which the blanket freeze got wrong.
        assert_eq!(
            rearmed(Some(armed), State::CloseWait, Ending::Flushing, now),
            Some(now + ESTABLISHED),
            "a halted CloseWait flow is still delivering, so activity rearms it"
        );
    }

    #[test]
    fn every_phase_a_flush_can_reach_leaves_it_with_a_finite_deadline() {
        // The property the whole lifecycle exists for, asserted over the reachable sequence rather than one
        // phase: a clean ending is bounded from the moment the stack sees the FIN, and stays bounded through
        // every teardown phase after it. Nothing here may answer `None`.
        let now = Instant::now();
        let mut held = deadline(now, State::Established);
        for (state, ending) in [
            // The daemon closed first, then the client's payload and FIN arrive together.
            (State::TimeWait, Ending::Pending),
            // The crossing drains them and records the halt; the teardown runs on.
            (State::TimeWait, Ending::Flushing),
            (State::Closed, Ending::Flushing),
        ] {
            held = rearmed(held, state, ending, now);
            assert!(
                held.is_some(),
                "{state:?}/{ending:?} left the flush unbounded"
            );
        }
        // And the other ordering, where the client finishes first and the response is still streaming.
        let mut held = deadline(now, State::Established);
        for (state, ending) in [
            (State::CloseWait, Ending::Pending),
            (State::CloseWait, Ending::Flushing),
            (State::LastAck, Ending::Flushing),
            (State::Closed, Ending::Flushing),
        ] {
            held = rearmed(held, state, ending, now);
            assert!(
                held.is_some(),
                "{state:?}/{ending:?} left the flush unbounded"
            );
        }
    }
}
