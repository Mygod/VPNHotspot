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
//! promised a turn against one. The UDP, Echo and virtual-DNS completion arms are resource-bounded instead:
//! each one's readiness is produced by the metered sources, so it drains once those stop being served. Either
//! way this is a bound on turns among the metered sources and not a wall-clock bound - an unmetered arm can
//! legitimately run several times between two turns of a metered one.
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

    /// Whether the ready reset arm must remain enabled.
    pub fn started(&self) -> bool {
        matches!(self.phase, Phase::Carried) || self.served != 0
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

    /// Reduced form of the owner's biased select. `scanned` counts owner turns and `flows.walked` counts
    /// polls of the expensive arm.
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
                () = std::future::ready(()), if pass.started() => pass.end(),
            }
        }
    }

    #[test]
    fn each_source_takes_one_turn_per_pass() {
        let mut pass = Pass::default();
        assert!(!pass.started());
        for (taken, source) in SOURCES.iter().enumerate() {
            assert!(pass.owed(*source), "{source:?}");
            pass.take(*source);
            assert!(pass.started(), "{source:?}");
            for (index, other) in SOURCES.iter().enumerate() {
                assert_eq!(
                    pass.owed(*other),
                    index > taken,
                    "{source:?} then {other:?}"
                );
            }
        }
        pass.end();
        assert!(!pass.started());
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
        assert!(pass.started());
        assert!(pass.owed(Source::UdpReply));
        assert!(pass.owed(Source::EchoReply));
        assert!(!pass.owed(Source::TcpAttention));
        assert!(!pass.owed(Source::TunIngress));
        pass.end();
        assert!(!pass.started());
        assert!(pass.owed(Source::TcpAttention));
        assert!(pass.owed(Source::TunIngress));
        pass.take(Source::UdpReply);
        pass.end();
        assert!(pass.owed(Source::UdpReply));
        pass.take(Source::UdpReply);
        assert!(pass.started());
        assert!(!pass.owed(Source::UdpReply));
        assert!(pass.owed(Source::TcpAttention));
        assert!(pass.owed(Source::TunIngress));
    }

    #[test]
    fn a_reset_after_attention_ran_carries_nothing() {
        let mut pass = Pass::default();
        pass.take(Source::TcpAttention);
        pass.end();
        assert!(!pass.started());
        for source in SOURCES {
            assert!(pass.owed(source), "{source:?}");
        }
        pass.take(Source::TcpAttention);
        pass.take(Source::UdpReply);
        pass.end();
        assert!(!pass.started());
        for source in SOURCES {
            assert!(pass.owed(source), "{source:?}");
        }
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
