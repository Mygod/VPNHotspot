use std::time::{Duration, Instant};

use smoltcp::socket::tcp::State;

/// RFC 5382 REQ-5's floor for an established connection: two hours four minutes. Once idle for
/// this long, the flow and its upstream descriptor are retired; new traffic creates a new flow.
/// <https://www.rfc-editor.org/rfc/rfc5382.html#section-5>
const ESTABLISHED: Duration = Duration::from_secs(7_440);

/// RFC 5382 REQ-5's floor for a transitory one - partially open, or partially closed: four minutes.
/// Expiry retires the flow and its descriptor rather than retaining an incomplete handshake indefinitely.
/// <https://www.rfc-editor.org/rfc/rfc5382.html#section-5>
const TRANSITORY: Duration = Duration::from_secs(240);

/// How long a flow in this phase may stay idle, or `None` where this owner has no say at all.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Not a clean client ending, or not one yet: the client has not finished sending, this flow is being
    /// reset, or its worker has already gone. The phase's own floor applies.
    Ordinary,
    /// The stack has seen the client's FIN and the bridge has not propagated it yet.
    Pending,
    /// The halt is propagated and the worker is flushing what the client sent.
    Flushing,
}

/// What one flow's idle deadline becomes when activity is observed on it.
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
                State::Listen | State::SynSent | State::SynReceived => Some(TRANSITORY),
                State::Established | State::FinWait1 | State::FinWait2 | State::CloseWait => {
                    Some(ESTABLISHED)
                }
                State::Closing | State::LastAck => Some(TRANSITORY),
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
        assert_eq!(
            rearmed(Some(stale), State::TimeWait, Ending::Ordinary, now),
            None,
            "which is exactly what the ordinary mapping would have given it"
        );
    }

    #[test]
    fn a_flushing_flow_rearms_while_it_can_still_reach_its_client_and_preserves_after() {
        let now = Instant::now();
        let armed = now + Duration::from_secs(60);
        for state in EVERY {
            let expected = if carries_toward_client(state) {
                Some(now + ESTABLISHED)
            } else {
                Some(armed)
            };
            assert_eq!(
                rearmed(Some(armed), state, Ending::Flushing, now),
                expected,
                "{state:?}"
            );
        }
        assert_eq!(
            rearmed(Some(armed), State::TimeWait, Ending::Flushing, now),
            Some(armed)
        );
        assert_eq!(
            rearmed(Some(armed), State::Closed, Ending::Flushing, now),
            Some(armed)
        );
        assert_eq!(
            rearmed(Some(armed), State::CloseWait, Ending::Flushing, now),
            Some(now + ESTABLISHED),
            "a halted CloseWait flow is still delivering, so activity rearms it"
        );
    }

    #[test]
    fn every_phase_a_flush_can_reach_leaves_it_with_a_finite_deadline() {
        let now = Instant::now();
        let mut held = deadline(now, State::Established);
        for (state, ending) in [
            (State::TimeWait, Ending::Pending),
            (State::TimeWait, Ending::Flushing),
            (State::Closed, Ending::Flushing),
        ] {
            held = rearmed(held, state, ending, now);
            assert!(
                held.is_some(),
                "{state:?}/{ending:?} left the flush unbounded"
            );
        }
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
