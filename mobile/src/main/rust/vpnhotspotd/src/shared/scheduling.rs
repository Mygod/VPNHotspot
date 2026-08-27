//! Which scheduling policy a launched daemon may put itself back on, and which it must leave alone.
//!
//! A process inherits its scheduling policy from whichever thread forked it, and every thread it creates
//! inherits the policy of the thread that created *it*. The app-UID daemon is launched from a coroutine
//! dispatcher thread inside the app, so what it inherits is whatever that thread was running under - observed
//! on device as `SCHED_BATCH`, which is the fair class with wakeup preemption switched off. That is a
//! reasonable policy for a background batch job and a poor one for a dataplane whose whole job is to be woken
//! by a packet and answer it, and nothing about it was chosen: it is an accident of where the launch
//! happened.
//!
//! What the daemon may do about it is narrow, and this is the decision rather than the syscall - the caller
//! owns that, because it has to happen before a runtime creates the threads that would inherit the old
//! policy. Only the two non-interactive fair policies are normalized, and only onto the ordinary one:
//!
//! - `SCHED_BATCH` and `SCHED_IDLE` are what an accidental inheritance looks like, and the ordinary fair
//!   policy is what both are moved to. The nice value does not change - `sched_setscheduler` keeps the
//!   caller's - so nothing here reorders this thread against others at the same policy. Leaving `SCHED_IDLE`
//!   is nevertheless a move *up a scheduling class*, out of the class the kernel runs only when nothing else
//!   wants the CPU, and the kernel treats it as one: it is refused for an unprivileged caller whose nice
//!   value is outside what `RLIMIT_NICE` allows. That is a refusal to report rather than a reason not to
//!   ask, and `SCHED_BATCH` - the one actually observed here - has no such condition.
//! - a real-time policy is never touched. Nothing here can be launched under one by accident, and turning one
//!   into a fair policy would be this daemon lowering a priority somebody deliberately gave it.
//! - the ordinary fair policy is already the answer, and asking for it again would be a syscall whose only
//!   effect is a failure to explain.
//!
//! This changes the *policy* and nothing else. Scheduler cgroup, cpuset and nice value are inherited too and
//! are left exactly as they arrived: they are Android's own placement of the app, and a daemon rewriting them
//! would be a background app escaping a classification the platform gave it on purpose.

use libc::{c_int, SCHED_BATCH, SCHED_IDLE};

/// The ordinary fair policy, zero on every Linux ABI.
///
/// Written out rather than taken from `libc` because the two C libraries this crate is built against name it
/// differently and each exposes only its own name: bionic's headers call it `SCHED_NORMAL`
/// (`linux/sched.h`), and glibc's call it `SCHED_OTHER` (POSIX). Naming it once here is what keeps a target
/// switch out of a decision that does not depend on the target.
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
        // The same value both C libraries name, which is what the constant above claims.
        assert_eq!(ORDINARY, libc::SCHED_OTHER);
        assert_eq!(normalized(ORDINARY), None);
    }

    #[test]
    fn a_real_time_policy_is_never_lowered() {
        assert_eq!(normalized(libc::SCHED_FIFO), None);
        assert_eq!(normalized(libc::SCHED_RR), None);
        // `SCHED_DEADLINE`, which libc does not name for every target this builds for and which the kernel
        // numbers 6. Named by value here for the same reason it is refused: a policy this daemon does not
        // recognise is one it has no business rewriting.
        assert_eq!(normalized(6), None);
    }

    #[test]
    fn the_policy_this_test_process_runs_under_needs_no_normalization() {
        // The host's own thread, read rather than assumed: a test runner is an ordinary fair-class process,
        // so this is the no-op arm above and stays deterministic wherever it runs. What it does not do is
        // set anything - the syscall belongs to the daemon's own startup, and asserting on its effect here
        // would be a test of the host's seccomp policy.
        // SAFETY: reads the calling thread's own policy and takes no argument that can be invalid.
        let policy = unsafe { libc::sched_getscheduler(0) };
        if policy == SCHED_BATCH || policy == SCHED_IDLE {
            assert_eq!(normalized(policy), Some(ORDINARY));
        } else {
            assert_eq!(normalized(policy), None);
        }
    }
}
