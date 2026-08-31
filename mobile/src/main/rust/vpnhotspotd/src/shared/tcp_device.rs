//! One-packet device between smoltcp and the interface handoff.
//!
//! smoltcp advances a socket when it fills a transmit token, so polling requires confirmed output capacity.
//! Otherwise data or a one-shot reset could be lost after leaving the socket.
use smoltcp::iface::{Interface, PollResult, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

pub struct Shim {
    /// One client packet offered to smoltcp.
    ///
    /// **Resource:** one heap packet of at most one interface MTU.
    ///
    /// **Derivation:** ingress pushes and polls synchronously, so no second packet is needed before the first
    /// is consumed. See [crate::shared::ingress::accept].
    ///
    /// **Failure mode:** [Interface::poll_delay] does not represent device readiness, so a retained packet
    /// could remain unscheduled and block later ingress.
    ///
    /// **Exhaustion:** known output exhaustion prevents the TUN read. A post-read
    /// [crate::shared::ingress::Ingress::push] refusal drops and counts `unconsumed`, unwinding tentative
    /// state. [Shim::push] refuses an occupied slot; the owner separately refuses when reset pre-settlement
    /// consumed the last output capacity.
    inbound: Option<Vec<u8>>,
    /// One packet produced by smoltcp and not yet drained by [quiesce].
    ///
    /// **Resource:** one heap packet of at most one interface MTU.
    ///
    /// **Derivation:** the device exposes one transmit token until [quiesce] drains it before the next poll.
    ///
    /// **Failure mode:** a stranded packet has already advanced its socket; an abort reset is not reproduced.
    ///
    /// **Exhaustion:** the device reports busy, leaving data in smoltcp for a later poll; [quiesce] always
    /// drains the slot before returning.
    outbound: Option<Vec<u8>>,
    mtu: usize,
}

impl Shim {
    pub fn new(mtu: usize) -> Self {
        Self {
            inbound: None,
            outbound: None,
            mtu,
        }
    }

    /// Offers one packet, refusing while the slot is occupied.
    pub fn push(&mut self, packet: &[u8]) -> bool {
        if self.inbound.is_some() {
            return false;
        }
        self.inbound = Some(packet.to_vec());
        true
    }

    /// Takes the produced packet.
    fn drain(&mut self) -> Option<Vec<u8>> {
        self.outbound.take()
    }

    /// Whether a produced packet is pending; true only within [quiesce].
    pub fn holding(&self) -> bool {
        self.outbound.is_some()
    }
}

/// The interface handoff, as the client-facing stack sees it.
pub trait Handoff {
    /// Exact capacity check; [quiesce] is the only producer.
    fn accepting(&self) -> bool;

    /// Hands one already-formed packet over, answering whether the handoff took it. A refusal is the
    /// implementation's to count.
    fn packet(&mut self, packet: Vec<u8>) -> bool;
}

/// Runs the client-facing stack to quiescence **at `at`**, handing everything it produces to `output`.
///
/// It drains each committed packet before another capacity-checked poll. A full handoff leaves output in
/// smoltcp; aborted sockets must likewise be retained while [crate::shared::lifetime::owes_reset] is true.
pub fn quiesce(
    interface: &mut Interface,
    device: &mut Shim,
    sockets: &mut SocketSet<'_>,
    at: Instant,
    output: &mut impl Handoff,
) {
    loop {
        // Deliver the previous poll's committed packet first.
        let emitted = match device.drain() {
            Some(packet) => output.packet(packet),
            None => false,
        };
        // Poll only with room for its one possible packet. Closure refuses later and ends the session.
        if !output.accepting() {
            break;
        }
        if matches!(interface.poll(at, device, sockets), PollResult::None) && !emitted {
            break;
        }
    }
    debug_assert!(
        !device.holding(),
        "the stack was not advanced past a packet this loop left behind"
    );
}

impl Device for Shim {
    type RxToken<'a> = Received;
    type TxToken<'a> = Transmit<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Receiving may require an immediate response, so both tokens require a free output slot.
        if self.outbound.is_some() {
            return None;
        }
        // Taken before the transmit token is built, so the borrow checker sees one mutable borrow at a time.
        let packet = self.inbound.take()?;
        Some((
            Received { packet },
            Transmit {
                outbound: &mut self.outbound,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Busy leaves the socket data queued for a later poll.
        if self.outbound.is_some() {
            return None;
        }
        Some(Transmit {
            outbound: &mut self.outbound,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        // No link layer: the TUN carries bare IP packets, which is also why the interface needs no hardware
        // address and no neighbour discovery.
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = self.mtu;
        capabilities
    }
}

pub struct Received {
    packet: Vec<u8>,
}

impl RxToken for Received {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.packet)
    }
}

pub struct Transmit<'a> {
    outbound: &'a mut Option<Vec<u8>>,
}

impl TxToken for Transmit<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut packet = vec![0u8; len];
        let result = f(&mut packet);
        // The slot was empty when this token was handed out and nothing else can fill it, so this replaces
        // nothing.
        *self.outbound = Some(packet);
        result
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use smoltcp::iface::{Config, SocketHandle};
    use smoltcp::socket::tcp::{Socket, SocketBuffer, State};
    use smoltcp::time::Duration;
    use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};

    use super::*;
    use crate::shared::lifetime::owes_reset;

    /// Small enough that the test payload needs several segments.
    const MTU: usize = 1_500;
    /// Keeps socket buffers from becoming the tested bottleneck.
    const BUFFER: usize = 65_535;

    /// Handoff with controllable capacity.
    #[derive(Default)]
    struct Slots {
        free: usize,
        taken: Vec<Vec<u8>>,
        refused: usize,
    }

    impl Handoff for Slots {
        fn accepting(&self) -> bool {
            self.free > 0
        }

        fn packet(&mut self, packet: Vec<u8>) -> bool {
            match self.free.checked_sub(1) {
                Some(left) => {
                    self.free = left;
                    self.taken.push(packet);
                    true
                }
                None => {
                    self.refused += 1;
                    false
                }
            }
        }
    }

    /// Two smoltcp sockets connected only through the device and handoff.
    struct Wire {
        interface: Interface,
        device: Shim,
        sockets: SocketSet<'static>,
        /// The flow's client-facing socket, which is the one this daemon owns.
        engine: SocketHandle,
        /// Stands in for the client at the other end of the TUN.
        client: SocketHandle,
        handoff: Slots,
        millis: i64,
    }

    impl Wire {
        fn connected() -> Self {
            let mut device = Shim::new(MTU);
            let mut interface = Interface::new(
                Config::new(HardwareAddress::Ip),
                &mut device,
                Instant::from_millis(0),
            );
            // Match the engine's any-IP interception.
            interface.set_any_ip(true);
            interface.update_ip_addrs(|addresses| {
                addresses
                    .push(IpCidr::new(IpAddress::v4(192, 0, 2, 1), 24))
                    .expect("one address fits");
            });
            let mut sockets = SocketSet::new(Vec::new());
            let engine = sockets.add(socket());
            let client = sockets.add(socket());
            sockets
                .get_mut::<Socket>(engine)
                .listen(80)
                .expect("a fresh socket may listen");
            sockets
                .get_mut::<Socket>(client)
                .connect(
                    interface.context(),
                    (IpAddress::v4(192, 0, 2, 1), 80),
                    49_152,
                )
                .expect("a fresh socket may connect");
            let mut wire = Self {
                interface,
                device,
                sockets,
                engine,
                client,
                handoff: Slots::default(),
                millis: 0,
            };
            wire.exchange();
            assert_eq!(wire.state(wire.engine), State::Established);
            assert_eq!(wire.state(wire.client), State::Established);
            wire
        }

        fn now(&self) -> Instant {
            Instant::from_millis(self.millis)
        }

        fn socket(&mut self, handle: SocketHandle) -> &mut Socket<'static> {
            self.sockets.get_mut::<Socket>(handle)
        }

        fn state(&self, handle: SocketHandle) -> State {
            self.sockets.get::<Socket>(handle).state()
        }

        /// One [quiesce] with current handoff capacity.
        fn stack(&mut self) {
            let at = self.now();
            quiesce(
                &mut self.interface,
                &mut self.device,
                &mut self.sockets,
                at,
                &mut self.handoff,
            );
        }

        fn poll_delay(&mut self) -> Option<Duration> {
            self.interface.poll_delay(self.now(), &self.sockets)
        }

        /// Exchanges packets until both sockets are quiet.
        fn exchange(&mut self) {
            let mut inflight: VecDeque<Vec<u8>> = VecDeque::new();
            for _ in 0..1_024 {
                self.handoff.free = 1;
                self.stack();
                inflight.extend(self.handoff.taken.drain(..));
                let Some(packet) = inflight.pop_front() else {
                    return;
                };
                assert!(
                    self.device.push(&packet),
                    "the device slot is free between polls"
                );
            }
            panic!("the two sockets never went quiet");
        }
    }

    fn socket() -> Socket<'static> {
        Socket::new(
            SocketBuffer::new(vec![0; BUFFER]),
            SocketBuffer::new(vec![0; BUFFER]),
        )
    }

    #[test]
    fn a_stalled_handoff_never_leaves_the_stack_advanced_past_a_packet_it_did_not_deliver() {
        let mut wire = Wire::connected();
        let payload = vec![0x5a; 4_000];
        let engine = wire.engine;
        wire.socket(engine)
            .send_slice(&payload)
            .expect("the send buffer takes the whole payload");
        // Put a FIN behind a multi-segment payload.
        wire.socket(engine).close();

        // Release one packet at a time without advancing the retransmission clock.
        let mut released = 0;
        while wire.poll_delay() == Some(Duration::ZERO) {
            wire.handoff.free = 1;
            wire.stack();
            assert!(
                !wire.device.holding(),
                "release {released} left a packet the stack had already been advanced past"
            );
            released += 1;
            assert!(released < 64, "the stack never went quiet");
        }
        assert!(
            released >= 2,
            "the payload has to need more than one packet for this to be about the second"
        );
        assert_eq!(
            wire.handoff.refused, 0,
            "nothing was handed over to be lost"
        );
        assert_ne!(
            wire.poll_delay(),
            Some(Duration::ZERO),
            "what the stack is waiting for now is an acknowledgement, not a slot"
        );

        // Delivery completes without retransmission.
        wire.exchange();
        assert_eq!(wire.millis, 0, "no retransmission timer was ever involved");
        let client = wire.client;
        assert_eq!(
            wire.socket(client).recv_queue(),
            payload.len(),
            "every byte crossed"
        );
        assert_eq!(
            wire.state(client),
            State::CloseWait,
            "and the FIN behind them did too"
        );
    }

    #[test]
    fn an_abort_keeps_its_reset_in_the_socket_until_the_handoff_can_take_it() {
        let mut wire = Wire::connected();
        let engine = wire.engine;
        // Abort while the interface is stalled.
        wire.handoff.free = 0;
        wire.socket(engine).abort();
        wire.stack();
        assert!(wire.handoff.taken.is_empty(), "there was nowhere to put it");
        assert!(
            !wire.device.holding(),
            "and it was not taken out of the socket to sit in the device either"
        );
        assert!(
            owes_reset(wire.socket(engine)),
            "so the socket still owns it, which is what stops its flow being reclaimed"
        );
        assert_eq!(
            wire.poll_delay(),
            Some(Duration::ZERO),
            "and asks to be polled for it as soon as there is capacity, on its own account"
        );

        // Capacity sends the retained reset and makes the flow reclaimable.
        wire.handoff.free = 1;
        wire.stack();
        assert_eq!(wire.handoff.taken.len(), 1);
        assert!(
            !owes_reset(wire.socket(engine)),
            "the reset has left the socket, so nothing of it depends on the socket any more"
        );

        wire.exchange();
        assert_eq!(
            wire.state(wire.client),
            State::Closed,
            "and the client was really told"
        );
    }

    #[test]
    fn a_full_handoff_leaves_a_pushed_packet_unconsumed_and_asks_for_nothing() {
        let mut wire = Wire::connected();
        // Capture client data whose consumption requires an acknowledgement.
        let client = wire.client;
        let sent = b"payload";
        wire.socket(client)
            .send_slice(sent)
            .expect("the send buffer takes it");
        wire.handoff.free = 1;
        wire.stack();
        let segment = wire.handoff.taken.pop().expect("the client's data segment");
        assert!(wire.handoff.taken.is_empty());

        // Model the state prevented by the TUN readability gate.
        wire.handoff.free = 0;
        assert!(wire.device.push(&segment), "the device slot is free");
        wire.stack();
        let engine = wire.engine;
        assert_eq!(
            wire.socket(engine).recv_queue(),
            0,
            "the stack never saw it, so nothing of it was acted on"
        );
        assert!(wire.handoff.taken.is_empty(), "and nothing was answered");
        assert_ne!(
            wire.poll_delay(),
            Some(Duration::ZERO),
            "device readiness is no part of the schedule, so nothing wakes for a packet left here"
        );
        assert!(
            !wire.device.push(&segment),
            "and the packet behind it would be refused for as long as it stays"
        );

        // Capacity lets the same call consume it.
        wire.handoff.free = 2;
        wire.stack();
        assert_eq!(wire.socket(engine).recv_queue(), sent.len());
        assert!(!wire.device.holding());
        assert!(
            wire.device.push(&segment),
            "and the device is free for the next packet, rather than wedged behind a stale one"
        );

        // smoltcp schedules its delayed acknowledgement normally.
        let delay = wire.poll_delay().expect("an acknowledgement is pending");
        assert!(delay > Duration::ZERO);
        wire.millis += delay.millis() as i64 + 1;
        wire.stack();
        assert_eq!(wire.handoff.taken.len(), 1, "the acknowledgement");
    }

    #[test]
    fn a_handoff_with_room_drains_the_stack_in_one_call() {
        let mut wire = Wire::connected();
        let engine = wire.engine;
        wire.socket(engine)
            .send_slice(&vec![0x5a; 4_000])
            .expect("the send buffer takes the whole payload");
        wire.handoff.free = 16;
        wire.stack();
        assert!(wire.handoff.taken.len() > 1, "one call, every packet");
        assert!(!wire.device.holding());
        assert_ne!(wire.poll_delay(), Some(Duration::ZERO));
    }
}
