use libc::{c_int, SCHED_BATCH, SCHED_IDLE};

/// The ordinary fair policy, zero on every Linux ABI.
const ORDINARY: c_int = 0;

/// The policy this thread should be moved to, or `None` when the one it has is not this daemon's to change.
pub fn normalized(policy: c_int) -> Option<c_int> {
    match policy {
        SCHED_BATCH | SCHED_IDLE => Some(ORDINARY),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_inherited_non_interactive_policy_is_normalized() {
        assert_eq!(normalized(SCHED_BATCH), Some(ORDINARY));
        assert_eq!(normalized(SCHED_IDLE), Some(ORDINARY));
    }

    #[test]
    fn the_ordinary_policy_is_left_alone_rather_than_set_again() {
        assert_eq!(ORDINARY, libc::SCHED_OTHER);
        assert_eq!(normalized(ORDINARY), None);
    }

    #[test]
    fn a_real_time_policy_is_never_lowered() {
        assert_eq!(normalized(libc::SCHED_FIFO), None);
        assert_eq!(normalized(libc::SCHED_RR), None);
        assert_eq!(normalized(6), None);
    }

    #[test]
    fn the_policy_this_test_process_runs_under_needs_no_normalization() {
        let policy = unsafe { libc::sched_getscheduler(0) };
        if policy == SCHED_BATCH || policy == SCHED_IDLE {
            assert_eq!(normalized(policy), Some(ORDINARY));
        } else {
            assert_eq!(normalized(policy), None);
        }
    }
}
