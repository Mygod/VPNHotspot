//! The seam between the TUN this daemon owns and the TCP stack that terminates on it.
//!
//! Terminated TCP needs a real TCP implementation, and root mode's answer - `TPROXY`, letting the kernel do
//! it - needs netfilter, which the app UID cannot touch. So the stack is in-process. Nothing here implements
//! TCP; this is the two-line adapter that lets one exist:
//!
//! - **Ingress** is push, not pull. The ingress task already read and classified the packet, so it is handed
//!   over directly rather than polled for. One packet is in flight at a time, consumed inside a single poll.
//! - **Egress** does not go straight to the descriptor. Segments are collected here and drained by the owner
//!   into the same writer every other producer uses, so the generation gate and the final size validation
//!   apply to TCP exactly as they do to everything else.
//!
//! The advertised MTU is the session's own, fixed for its whole life at the value the TUN was verified to
//! carry. That is what does the right thing for TCP with no DF games at all: the stack negotiates an MSS from
//! it at SYN time, so every segment already fits the interface and nothing has to fragment or reject one.

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

pub(crate) struct Shim {
    /// The packet the ingress task just handed over, if the stack has not taken it yet.
    inbound: Option<Vec<u8>>,
    /// One segment the stack has produced, waiting for the owner to drain it to the writer.
    ///
    /// One, structurally, rather than a `Vec` of however many one poll produced. How many sockets have
    /// something to send is a client's choice, and a poll that emitted one segment per socket into a growing
    /// vector was an allocation sized by that choice. A device that refuses to transmit is the interface
    /// smoltcp already has for a busy link - [Device::transmit] answering `None` leaves the socket's data
    /// queued and untouched - so the owner drains between polls and the stack simply retries.
    outbound: Option<Vec<u8>>,
    mtu: usize,
}

impl Shim {
    pub(crate) fn new(mtu: usize) -> Self {
        Self {
            inbound: None,
            outbound: None,
            mtu,
        }
    }

    /// Offers one packet to the stack. Returns it back when the stack still holds an untaken one, which the
    /// caller counts rather than queues: a second packet before the first is consumed would mean the poll
    /// that should have consumed it did not run.
    pub(crate) fn push(&mut self, packet: &[u8]) -> bool {
        if self.inbound.is_some() {
            return false;
        }
        self.inbound = Some(packet.to_vec());
        true
    }

    /// Takes the one segment the slot holds, freeing it for the next poll. `None` when the stack produced
    /// nothing, which is also how the owner knows a poll made no output progress.
    pub(crate) fn drain(&mut self) -> Option<Vec<u8>> {
        self.outbound.take()
    }
}

impl Device for Shim {
    type RxToken<'a> = Received;
    type TxToken<'a> = Transmit<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Refused while the slot is occupied, for the same reason [Shim::transmit] is: the stack asks for both
        // tokens together so that a reply can be built from the request it just read, and handing it a receive
        // token whose reply has nowhere to go would consume the packet and drop the answer.
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
        // `None` is "the device is busy", which smoltcp handles by leaving the socket's data queued and
        // trying again on the next poll - nothing is lost and no state advances. That is what makes one slot
        // safe rather than lossy.
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

pub(crate) struct Received {
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

pub(crate) struct Transmit<'a> {
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
