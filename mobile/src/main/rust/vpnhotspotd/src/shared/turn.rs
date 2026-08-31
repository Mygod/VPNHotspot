//! One metered pass over the app-UID dataplane owner's ordinary sources.
//!
//! That owner selects on every source at once, biased. Cancellation is the first arm and is answered before
//! anything else. Bias below that point is what turns a source that is ready every time into starvation of
//! everything under it: an unbounded reply queue may already contain another event, and a worker can publish
//! another after the owner takes one. A pass gives each metered source at most one turn, so a source that is
//! ready every time is inactive for the rest of that pass however often it is polled.
//!
//! Not every arm is metered, and the unmetered ones are unmetered for two different reasons. The
//! configuration channel is prioritized deliberately, because it carries the admission value everything
//! below it reads: one call is in flight at a time, since the config reader waits for the owner's
//! acknowledgement before reading another, but it is the app's authenticated control stream that produces
//! them, so metered dataplane work does not make a run of config calls finite and no ordinary source is
//! promised a turn against one. The UDP, Echo and virtual-DNS completion arms are producer-bounded instead:
//! each one's readiness is produced by the metered sources, so it drains once those stop being served. The
//! TUN writer's two arms are the same kind, and the order between them matters: a guarded-datagram
//! settlement exists only for a datagram this owner itself handed over, and the interface-drain wait is ready
//! only while the owner has already filled the one-deep handoff. The settlement arm sits above the drain
//! wait, which is what keeps the settlement handoff from deadlocking - the writer may block handing an ending
//! over, and the owner takes it even while parked waiting for that same writer to free interface capacity.
//! Either way this is a bound on turns among the metered sources and not a wall-clock bound - an unmetered
//! arm can legitimately run several times between two turns of a metered one.
//!
//! The pass has to end on *readiness* rather than on having served everyone, because most of these sources
//! are idle most of the time and one that never becomes ready must not be able to hold a pass open. The owner
//! ends it from the last arm of its own select: while [Pass::started] is true at least one arm is inactive,
//! so that arm is armed and ready, and the select therefore cannot block without first offering every source
//! that still owes a turn the chance to run. That arm is also the only one that retries the select in place
//! instead of returning to the owner's turn, because ending a pass is the only thing that touches no owner
//! state - so the deadlines the owner read for this turn still apply and are not rescanned. Nothing is lost
//! by an inactive arm: every source the owner selects on is cancel-safe and is polled again from scratch.
//!
//! Ending one takes two stages, but only when the first of them buys something. The select that ends a pass
//! has just polled every source that still owed a turn and found all of them pending, and exactly one of
//! those polls is expensive: the TCP engine's attention walks every live flow before it can answer. When
//! attention was among them, the first stage offers only the cohort that actually took a turn in the pass
//! that ended - those are the sources a worker can have made ready again, and attention is not one of them -
//! so a source that is ready every time runs after one walk of that flow table rather than two. The second
//! stage lifts the restriction whether or not the cohort ran, which is what keeps the two from alternating
//! for ever: a source that became ready just after it was polled is deferred through the cohort and no
//! further, and a select that reaches the second stage blocks on real readiness.
//!
//! When attention has already taken its turn there is nothing to spare it, because the select that ended the
//! pass had it inactive and walked nothing. Carrying a cohort there would only defer the one poll of
//! attention the owner still owes it and add a second when the restriction lifted, so that reset opens the
//! whole select at once instead. The carried stage therefore always leaves attention inactive, which is the
//! whole of what it is for.
//!
//! One group of arms is outside all of this. A pass is frozen while the interface handoff cannot take another
//! datagram - see [Pass::started] - and that freeze is about the sources that *produce* output. The owner's
//! four recurring deadline arms are enabled regardless of what a frozen pass owes and record no turn while it
//! is frozen, because each of them rearms its own table on a later deadline every time it runs: metering them
//! with the pass would serve the first deadline of a stall and none of the ones behind it, leaving expired
//! rows for a reply to be authorized against once capacity returned. Recording no turn is what keeps them out
//! of the fairness state entirely - [Pass] is left exactly as the producer that filled the slot left it,
//! carried cohort included - and nothing spins, because each firing consumes the entries that were really due
//! and its index moves on.
//!
//! Being outside the metering is exactly why such an arm must not emit. See [Expiry].

/// One metered select arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// TCP traffic and completion work. A pending poll scans all live flows.
    TcpAttention,
    UdpReply,
    EchoReply,
    TcpDnsAsk,
    MappingDeadline,
    EchoDeadline,
    FragmentDeadline,
    TcpDeadline,
    TunIngress,
}

/// Every source must have a distinct bit in [Pass].
const _: () = assert!((Source::TunIngress as u32) < u16::BITS);

impl Source {
    fn bit(self) -> u16 {
        1 << self as u16
    }
}

/// What a due-deadline turn may do about output.
///
/// The owner reads the interface handoff once per turn, and every gate on that turn - the reply arms, TUN
/// readability, the stack's own poll delay and the arm that ends a pass - uses that one answer. The reading
/// can go stale in the owner's favour while the select is pending, because the serial writer frees a slot
/// whenever it takes what was queued. That release is what [Pass] hands to the producer next in the biased
/// order, and the arm that wakes for it sits below every deadline arm: a deadline that is due at the same
/// moment therefore wins the race and runs with a slot free that no metered producer was offered.
///
/// A turn that recorded no turn must not be able to take that slot. Repeated deadlines otherwise become an
/// unmetered producer that can keep winning releases ahead of replies already waiting in their bounded
/// mailboxes - the very starvation [Pass::started] exists to prevent. So the *snapshot* decides what a
/// deadline arm may do, not what the handoff happens to hold by the time it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expiry {
    /// The handoff had room when the turn read it, so this arm also took its turn in the pass. Whatever
    /// retiring a row produces goes to the client as usual.
    Delivering,
    /// The handoff was full when the turn read it, so this arm records no turn. Rows that are due are still
    /// retired - that is what keeps successive deadlines actionable through a stall - but nothing is emitted.
    /// Anything a retirement would have put in front of a client is either dropped as the best-effort
    /// notification it already was, or left where a later metered turn produces it.
    Maintaining,
}

impl Expiry {
    /// Whether a retirement performed in this mode may also emit.
    pub fn delivering(self) -> bool {
        matches!(self, Self::Delivering)
    }
}

impl From<bool> for Expiry {
    /// From the turn's one reading of the interface handoff, and from nothing sampled later.
    fn from(accepting: bool) -> Self {
        if accepting {
            Self::Delivering
        } else {
            Self::Maintaining
        }
    }
}

/// Which of the two sets [Pass::served] names, and so which stage of a reset is in force. A reset cannot be
/// a plain complement of that mask: the mask alone would not say which of the two sets it currently holds,
/// and the owner's ending arm would swap them for ever without either becoming the whole select again.
#[derive(Default)]
enum Phase {
    /// An ordinary pass. [Pass::served] names the sources that have already taken their turn.
    #[default]
    Open,
    /// The first stage of a reset. [Pass::served] names the cohort that took a turn in the pass that just
    /// ended, and is the only cohort still offered, because every other source was polled and observed
    /// pending by the very select that ended it. [Source::TcpAttention] is never in that cohort, since a
    /// reset only carries one when attention was among the sources just polled, so this stage always leaves
    /// the one expensive arm inactive.
    Carried,
}

/// Fairness state for the owner's metered arms.
#[derive(Default)]
pub struct Pass {
    served: u16,
    phase: Phase,
}

impl Pass {
    /// Whether this source is enabled in the current phase.
    pub fn owed(&self, source: Source) -> bool {
        match self.phase {
            Phase::Open => self.served & source.bit() == 0,
            Phase::Carried => self.served & source.bit() != 0,
        }
    }

    /// Records a turn. A carried source starts the new open pass.
    pub fn take(&mut self, source: Source) {
        self.served = match self.phase {
            Phase::Open => self.served | source.bit(),
            Phase::Carried => {
                self.phase = Phase::Open;
                source.bit()
            }
        };
    }

    /// Whether the ready reset arm must remain enabled. `accepting` is whether the interface handoff can
    /// take another datagram.
    ///
    /// While it cannot, the owner holds its output-producing sources inactive, and a pass must not end: the
    /// sources it gated off still owe a turn, and forgetting that would let the next release open a fresh
    /// biased pass that a continuously ready TCP stack wins every time, indefinitely starving replies already
    /// waiting in their bounded mailboxes. Freezing the pass instead is what makes a release re-enter the
    /// ordinary order at the point it interrupted. Nothing spins on it: while the pass is frozen the only
    /// thing that can make the gated sources runnable again is the interface handoff draining, which the
    /// owner has its own arm for.
    ///
    /// What is frozen is producing, not maintaining. The owner runs its recurring deadline arms throughout a
    /// stall and records no turn for them, so this state stays exactly what the interrupted producer left -
    /// see the module documentation.
    pub fn started(&self, accepting: bool) -> bool {
        accepting && (matches!(self.phase, Phase::Carried) || self.served != 0)
    }

    /// Resets the pass, because nothing that still owed a turn was ready to take one. When
    /// [Source::TcpAttention] still owed a turn it is one of the sources this select just polled and found
    /// pending, and the reset carries the sources that did take one into a retry of their own: they are the
    /// ones a worker can have refilled, and offering them without attention lets one of them run without a
    /// second walk of the flow table. The stage after that lifts the restriction whether or not the cohort
    /// ran, so every source owes a turn again and the select can block.
    ///
    /// When attention has already taken its turn there is nothing to carry for: this select had it inactive
    /// and walked nothing, so a carried stage would only defer the one poll the owner still owes it and add
    /// a second when the restriction lifted. That reset opens the whole select at once instead.
    pub fn end(&mut self) {
        if matches!(self.phase, Phase::Open) && self.owed(Source::TcpAttention) {
            self.phase = Phase::Carried;
            return;
        }
        self.phase = Phase::Open;
        self.served = 0;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;

    const SOURCES: [Source; 9] = [
        Source::TcpAttention,
        Source::UdpReply,
        Source::EchoReply,
        Source::TcpDnsAsk,
        Source::MappingDeadline,
        Source::EchoDeadline,
        Source::FragmentDeadline,
        Source::TcpDeadline,
        Source::TunIngress,
    ];

    #[derive(Debug, PartialEq, Eq)]
    enum Turned {
        Cancelled,
        Attention,
        Flood,
        Ingress,
    }

    fn flooded() -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
        let (sender, receiver) = mpsc::channel(1);
        sender.try_send(()).expect("the first reply is queued");
        (sender, receiver)
    }

    #[derive(Default)]
    struct Flows {
        walked: usize,
        ready: usize,
    }

    async fn attention(flows: &mut Flows) {
        flows.walked += 1;
        match flows.ready.checked_sub(1) {
            Some(left) => flows.ready = left,
            None => std::future::pending().await,
        }
    }

    /// Reduced owner select; separately counts turns and TCP scans.
    async fn owner_turn(
        pass: &mut Pass,
        scanned: &mut usize,
        flows: &mut Flows,
        cancel: &CancellationToken,
        flood: &mut mpsc::Receiver<()>,
        refill: &mpsc::Sender<()>,
        ingress: &mut mpsc::Receiver<()>,
    ) -> Turned {
        *scanned += 1;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break Turned::Cancelled,
                () = attention(flows), if pass.owed(Source::TcpAttention) => {
                    pass.take(Source::TcpAttention);
                    break Turned::Attention;
                }
                Some(()) = flood.recv(), if pass.owed(Source::UdpReply) => {
                    pass.take(Source::UdpReply);
                    refill.try_send(()).expect("the worker refills what the owner took");
                    break Turned::Flood;
                }
                Some(()) = ingress.recv(), if pass.owed(Source::TunIngress) => {
                    pass.take(Source::TunIngress);
                    break Turned::Ingress;
                }
                // This model never fills the interface handoff.
                () = std::future::ready(()), if pass.started(true) => pass.end(),
            }
        }
    }

    #[test]
    fn each_source_takes_one_turn_per_pass() {
        let mut pass = Pass::default();
        assert!(!pass.started(true));
        for (taken, source) in SOURCES.iter().enumerate() {
            assert!(pass.owed(*source), "{source:?}");
            pass.take(*source);
            assert!(pass.started(true), "{source:?}");
            for (index, other) in SOURCES.iter().enumerate() {
                assert_eq!(
                    pass.owed(*other),
                    index > taken,
                    "{source:?} then {other:?}"
                );
            }
        }
        pass.end();
        assert!(!pass.started(true));
        for source in SOURCES {
            assert!(pass.owed(source), "{source:?}");
        }
    }

    #[test]
    fn a_reset_carries_the_cohort_that_ran_before_it_lifts_the_restriction() {
        let mut pass = Pass::default();
        pass.take(Source::UdpReply);
        pass.take(Source::EchoReply);
        pass.end();
        assert!(pass.started(true));
        assert!(pass.owed(Source::UdpReply));
        assert!(pass.owed(Source::EchoReply));
        assert!(!pass.owed(Source::TcpAttention));
        assert!(!pass.owed(Source::TunIngress));
        pass.end();
        assert!(!pass.started(true));
        assert!(pass.owed(Source::TcpAttention));
        assert!(pass.owed(Source::TunIngress));
        pass.take(Source::UdpReply);
        pass.end();
        assert!(pass.owed(Source::UdpReply));
        pass.take(Source::UdpReply);
        assert!(pass.started(true));
        assert!(!pass.owed(Source::UdpReply));
        assert!(pass.owed(Source::TcpAttention));
        assert!(pass.owed(Source::TunIngress));
    }

    #[test]
    fn a_reset_after_attention_ran_carries_nothing() {
        let mut pass = Pass::default();
        pass.take(Source::TcpAttention);
        pass.end();
        assert!(!pass.started(true));
        for source in SOURCES {
            assert!(pass.owed(source), "{source:?}");
        }
        pass.take(Source::TcpAttention);
        pass.take(Source::UdpReply);
        pass.end();
        assert!(!pass.started(true));
        for source in SOURCES {
            assert!(pass.owed(source), "{source:?}");
        }
    }

    /// Metered output sources in biased order.
    const EMITTING: [Source; 4] = [
        Source::TcpAttention,
        Source::UdpReply,
        Source::EchoReply,
        Source::TunIngress,
    ];

    fn gated(source: Source) -> bool {
        matches!(
            source,
            Source::UdpReply | Source::EchoReply | Source::TunIngress
        )
    }

    /// Models ready producers across one-slot capacity releases.
    fn served_across_releases(releases: usize, tcp_ready_while_stalled: bool) -> Vec<Source> {
        let mut pass = Pass::default();
        let mut accepting = true;
        let mut served = Vec::new();
        let mut left = releases;
        loop {
            let ready = |source: Source| {
                accepting || source != Source::TcpAttention || tcp_ready_while_stalled
            };
            let next = EMITTING.into_iter().find(|source| {
                pass.owed(*source) && ready(*source) && (accepting || !gated(*source))
            });
            match next {
                Some(source) => {
                    pass.take(source);
                    // An accepting turn fills the slot.
                    if accepting {
                        served.push(source);
                        accepting = false;
                    }
                }
                None if pass.started(accepting) => pass.end(),
                None if !accepting => {
                    // Model `output.accepted()`.
                    let Some(remaining) = left.checked_sub(1) else {
                        break;
                    };
                    left = remaining;
                    accepting = true;
                }
                // The real owner would block here.
                None => break,
            }
        }
        served
    }

    #[test]
    fn sustained_tcp_output_cannot_starve_queued_replies_across_capacity_releases() {
        for tcp_ready_while_stalled in [true, false] {
            let served = served_across_releases(31, tcp_ready_while_stalled);
            assert_eq!(
                served.len(),
                32,
                "every release placed exactly one datagram (stalled-ready {tcp_ready_while_stalled})"
            );
            // Every group must contain each producer once.
            for (index, group) in served
                .as_chunks::<{ EMITTING.len() }>()
                .0
                .iter()
                .enumerate()
            {
                let mut seen = group.to_vec();
                seen.sort_by_key(|source| *source as u16);
                let mut all = EMITTING.to_vec();
                all.sort_by_key(|source| *source as u16);
                assert_eq!(
                    seen, all,
                    "release group {index} served {group:?} (stalled-ready {tcp_ready_while_stalled})"
                );
            }
            for source in EMITTING {
                assert_eq!(
                    served.iter().filter(|served| **served == source).count(),
                    8,
                    "{source:?} (stalled-ready {tcp_ready_while_stalled})"
                );
            }
        }
    }

    #[test]
    fn ending_a_pass_while_the_interface_is_full_would_hand_every_release_to_one_source() {
        // Counterexample: resetting while producers are gated starves them.
        let mut pass = Pass::default();
        let mut served = Vec::new();
        let mut accepting = true;
        for _ in 0..10 {
            let next = EMITTING
                .into_iter()
                .find(|source| pass.owed(*source) && (accepting || !gated(*source)));
            match next {
                Some(source) => {
                    pass.take(source);
                    served.push(source);
                    accepting = false;
                }
                None => {
                    pass.end();
                    accepting = true;
                }
            }
        }
        assert!(
            served.iter().all(|source| *source == Source::TcpAttention),
            "without the rule the first emitting source takes every release: {served:?}"
        );
    }

    /// Recurring deadlines in biased order.
    const RECURRING: [Source; 4] = [
        Source::MappingDeadline,
        Source::EchoDeadline,
        Source::FragmentDeadline,
        Source::TcpDeadline,
    ];

    /// Arms that can emit toward the client.
    const TOWARD_CLIENT: [Source; 5] = [
        Source::TcpAttention,
        Source::UdpReply,
        Source::EchoReply,
        Source::TcpDeadline,
        Source::TunIngress,
    ];

    /// Models the owner's deadline gating rule.
    fn deadline_arm(pass: &mut Pass, source: Source, accepting: bool) -> bool {
        if accepting && !pass.owed(source) {
            return false;
        }
        if accepting {
            pass.take(source);
        }
        true
    }

    /// First owed output arm in biased order.
    fn offered(pass: &Pass) -> Option<Source> {
        SOURCES
            .into_iter()
            .find(|source| TOWARD_CLIENT.contains(source) && pass.owed(*source))
    }

    #[test]
    fn a_stall_serves_successive_deadlines_without_disturbing_the_pass_it_interrupted() {
        // TCP filled the slot; other producers remain owed.
        let mut pass = Pass::default();
        pass.take(Source::TcpAttention);
        assert!(!pass.started(false), "a frozen pass may not end");

        // Successive deadlines run through one continuous stall.
        for round in 0..4 {
            for source in RECURRING {
                assert!(
                    deadline_arm(&mut pass, source, false),
                    "{source:?} in round {round}"
                );
            }
        }

        // Maintenance leaves the pass unchanged.
        assert!(
            !pass.owed(Source::TcpAttention),
            "TCP output has had its turn"
        );
        for source in SOURCES {
            assert_eq!(
                pass.owed(source),
                source != Source::TcpAttention,
                "{source:?} after the stall"
            );
        }
        assert!(!pass.started(false), "and the pass still may not end");
        assert_eq!(
            EMITTING
                .into_iter()
                .find(|source| pass.owed(*source) && !gated(*source)),
            None
        );

        // Capacity returns to the previously owed producers before TCP.
        assert_eq!(offered(&pass), Some(Source::UdpReply));
        pass.take(Source::UdpReply);
        assert_eq!(offered(&pass), Some(Source::EchoReply));
        pass.take(Source::EchoReply);
        assert_eq!(
            offered(&pass),
            Some(Source::TcpDeadline),
            "and only then does the stack get one"
        );
    }

    #[test]
    fn maintenance_cannot_take_a_slot_the_writer_freed_after_the_turn_read_the_handoff() {
        let mut pass = Pass::default();
        pass.take(Source::TcpAttention);
        // Capacity returns after the turn snapshots a full handoff.
        let accepting = false;
        let freed = true;
        let mut placed = Vec::new();
        for round in 0..2 {
            for source in RECURRING {
                assert!(
                    deadline_arm(&mut pass, source, accepting),
                    "{source:?} in round {round}"
                );
                if freed && Expiry::from(accepting).delivering() {
                    placed.push(source);
                }
            }
        }
        assert!(
            placed.is_empty(),
            "a turn that recorded no turn emitted into the freed slot: {placed:?}"
        );
        // The released slot remains owed to the gated producers.
        for source in SOURCES {
            assert_eq!(
                pass.owed(source),
                source != Source::TcpAttention,
                "{source:?}"
            );
        }
        assert_eq!(offered(&pass), Some(Source::UdpReply));
    }

    #[test]
    fn a_delivering_deadline_turn_is_the_one_that_took_a_turn() {
        let mut pass = Pass::default();
        assert_eq!(Expiry::from(true), Expiry::Delivering);
        assert!(Expiry::from(true).delivering());
        assert_eq!(Expiry::from(false), Expiry::Maintaining);
        assert!(!Expiry::from(false).delivering());
        for source in RECURRING {
            assert!(deadline_arm(&mut pass, source, true), "{source:?}");
            assert!(!pass.owed(source), "{source:?} took its turn");
        }
        for source in RECURRING {
            assert!(deadline_arm(&mut pass, source, false), "{source:?}");
            assert!(!pass.owed(source), "and a maintaining turn records none");
        }
    }

    #[test]
    fn metering_a_deadline_arm_with_a_frozen_pass_would_strand_every_deadline_after_the_first() {
        // Counterexample: metered maintenance strands later deadlines.
        let mut pass = Pass::default();
        pass.take(Source::TcpAttention);
        for source in RECURRING {
            assert!(deadline_arm(&mut pass, source, true), "{source:?}");
        }
        for source in RECURRING {
            assert!(
                !deadline_arm(&mut pass, source, true),
                "a second {source:?} is refused for the rest of the stall"
            );
        }
        assert!(
            !pass.started(false),
            "and no arm can end the pass to re-enable it while the interface is full"
        );
    }

    #[tokio::test]
    async fn a_queue_that_is_ready_every_turn_cannot_hold_off_the_tun() {
        let cancel = CancellationToken::new();
        let (refill, mut flood) = flooded();
        let (client, mut ingress) = mpsc::channel(1);
        client.try_send(()).expect("one packet is waiting");
        let mut pass = Pass::default();
        let mut scanned = 0;
        let mut flows = Flows::default();
        for expected in [Turned::Flood, Turned::Ingress] {
            assert_eq!(
                owner_turn(
                    &mut pass,
                    &mut scanned,
                    &mut flows,
                    &cancel,
                    &mut flood,
                    &refill,
                    &mut ingress
                )
                .await,
                expected
            );
        }
        assert_eq!(flood.len(), 1, "the queue stayed nonempty throughout");
    }

    #[tokio::test(start_paused = true)]
    async fn a_pass_that_ends_retries_the_cohort_rather_than_the_owner_turn() {
        let cancel = CancellationToken::new();
        let (refill, mut flood) = flooded();
        let (_client, mut ingress) = mpsc::channel(1);
        let mut pass = Pass::default();
        let mut scanned = 0;
        let mut flows = Flows::default();
        for served in 1..=3 {
            let turned = tokio::time::timeout(
                Duration::from_secs(1),
                owner_turn(
                    &mut pass,
                    &mut scanned,
                    &mut flows,
                    &cancel,
                    &mut flood,
                    &refill,
                    &mut ingress,
                ),
            )
            .await
            .expect("a pass ends rather than waiting on a source that never becomes ready");
            assert_eq!(turned, Turned::Flood);
            assert_eq!(scanned, served, "one owner scan per reply");
            assert_eq!(flows.walked, served, "one TCP flow scan per reply");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_pass_that_ends_after_attention_ran_polls_it_once_more_and_blocks() {
        let cancel = CancellationToken::new();
        let (refill, mut flood) = mpsc::channel(1);
        let (_client, mut ingress) = mpsc::channel(1);
        let mut pass = Pass::default();
        let mut scanned = 0;
        let mut flows = Flows {
            walked: 0,
            ready: 1,
        };
        assert_eq!(
            owner_turn(
                &mut pass,
                &mut scanned,
                &mut flows,
                &cancel,
                &mut flood,
                &refill,
                &mut ingress
            )
            .await,
            Turned::Attention
        );
        assert_eq!(flows.walked, 1, "one walk to report what it had");
        assert!(
            tokio::time::timeout(
                Duration::from_secs(1),
                owner_turn(
                    &mut pass,
                    &mut scanned,
                    &mut flows,
                    &cancel,
                    &mut flood,
                    &refill,
                    &mut ingress,
                ),
            )
            .await
            .is_err(),
            "nothing else ever becomes ready"
        );
        assert_eq!(flows.walked, 2);
    }

    #[tokio::test]
    async fn cancellation_wins_over_a_source_that_is_ready() {
        let cancel = CancellationToken::new();
        let (refill, mut flood) = flooded();
        let (client, mut ingress) = mpsc::channel(1);
        client.try_send(()).expect("one packet is waiting");
        cancel.cancel();
        let mut pass = Pass::default();
        let mut scanned = 0;
        let mut flows = Flows::default();
        // Check cancellation in Open, partially served, and Carried states.
        for stage in 0..3 {
            assert_eq!(
                owner_turn(
                    &mut pass,
                    &mut scanned,
                    &mut flows,
                    &cancel,
                    &mut flood,
                    &refill,
                    &mut ingress
                )
                .await,
                Turned::Cancelled
            );
            match stage {
                0 => pass.take(Source::UdpReply),
                _ => pass.end(),
            }
        }
        assert_eq!(
            flows.walked, 0,
            "cancellation was answered before any table was walked"
        );
    }
}
