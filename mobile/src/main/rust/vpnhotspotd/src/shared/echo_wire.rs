//! Wire format for relayed Echo: the strict parse of a client Echo Request read from the TUN, and the
//! construction of the Echo Reply that goes back out through the common writer.
//!
//! Only Echo. Every other ICMP type a client sends belongs to somebody else - Neighbor Discovery, Router
//! Advertisement and DHCP are Android's downstream link control, and an *error* from a client describes a
//! packet this daemon never forwarded - so this recognises one message and rejects the rest rather than
//! growing into a dispatch table.
//!
//! The two families are built separately and never by reinterpreting one header as the other. The type
//! numbers differ (8 and 128 for a request, 0 and 129 for a reply) and ICMPv6's checksum covers a
//! pseudo-header that ICMPv4's does not, so one numeric path would produce a packet that looks right here
//! and fails validation at the client.
//!
//! The identifier and the sequence get different treatment, and that asymmetry is forced by the kernel
//! rather than chosen. An unprivileged ping socket overwrites the identifier of everything sent through it
//! with its own bound port, so on the wire every session on one socket shares an identifier and it cannot
//! tell them apart. The sequence is passed through untouched, which makes it the only field left to
//! allocate - and the client's own values then have to be carried in the table and restored on the way
//! back.
//!
//! Both halves of that pair are restored, for two different readers. `nf_conntrack_proto_icmp` builds its
//! tuple from type, code and the identifier only, so the identifier is what Android's inner IPv4 NAT
//! reverses a reply by - and the sequence's absence from that tuple is exactly why substituting it is safe.
//! The client's own sequence still has to come back, because a ping matches replies on the pair.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use etherparse::{
    icmpv4, icmpv6, IcmpEchoHeader, Icmpv4Header, Icmpv4Type, Icmpv6Header, Icmpv6Type, IpNumber,
    Ipv4Header, Ipv6Header,
};

use crate::shared::packet_writer::{WriterError, IPV4_HEADER_LEN, IPV6_HEADER_LEN};
use crate::shared::udp_wire::Reject;

/// Type, code, checksum, identifier, sequence. The same eight bytes in both families, which is the only
/// thing about them that is the same.
pub const ECHO_HEADER_LEN: usize = 8;

/// One client Echo Request, in the terms the table keys and forwards on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request<'a> {
    /// The TUN-visible source, which is where the reply goes. Never an identity: for IPv4 this is Android's
    /// own inner NAT address, and nothing distinguishes a tethered client's from one a local app chose.
    pub client: IpAddr,
    pub remote: IpAddr,
    /// As it arrived, not yet decremented. The forwarding decision belongs to the caller, which is also the
    /// only thing that can answer an expired one.
    pub hop_limit: u8,
    /// IPv4 DF, which the caller reapplies to the shared ping socket before each send.
    ///
    /// True for IPv6, where it is not a bit at all: no router may fragment an IPv6 packet, so the permission
    /// the IPv4 bit withholds is withheld unconditionally there.
    pub dont_fragment: bool,
    /// The client's own pair, neither half of which survives the trip: the kernel overwrites the identifier
    /// and the daemon substitutes the sequence. So both have to be kept here to be restored.
    pub identity: Identity,
    pub payload: &'a [u8],
}

/// The identifier and sequence pair that names one Echo message.
///
/// A unit rather than two arguments because both directions need both together: coming back from a ping
/// socket the pair is what the session is looked up by, and going out to a client it is what the session
/// restores. Whose values they are differs by direction, and that is the caller's business - the sequence
/// arriving from upstream is the daemon's own, while the pair written toward a client is the client's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    pub identifier: u16,
    pub sequence: u16,
}

pub fn parse(packet: &[u8]) -> Result<Request<'_>, Reject> {
    match packet.first().map(|byte| byte >> 4) {
        Some(4) => {
            let header_length = ((packet[0] & 0xf) as usize) * 4;
            if header_length < IPV4_HEADER_LEN || packet.len() < header_length {
                return Err(Reject::Malformed("IPv4 header does not fit"));
            }
            if u16::from_be_bytes([packet[2], packet[3]]) as usize != packet.len() {
                return Err(Reject::Malformed("IPv4 total length disagrees"));
            }
            if packet[9] != IpNumber::ICMP.0 {
                return Err(Reject::NotUdp);
            }
            let flags = u16::from_be_bytes([packet[6], packet[7]]);
            // more-fragments, or a non-zero offset
            if flags & 0x2000 != 0 || flags & 0x1fff != 0 {
                return Err(Reject::Fragmented);
            }
            let echo = message(&packet[header_length..], icmpv4::TYPE_ECHO_REQUEST)?;
            // Never optional, unlike UDP's over IPv4: RFC 792 requires it. Verified because the message
            // leaves under a substituted sequence and so acquires a fresh valid checksum, which makes this
            // the only place corruption in it can still be seen.
            if Icmpv4Header::with_checksum(Icmpv4Type::EchoRequest(echo.header()), echo.payload)
                .checksum
                != echo.checksum
            {
                return Err(Reject::Malformed("ICMPv4 echo checksum mismatch"));
            }
            Ok(Request {
                client: IpAddr::V4(Ipv4Addr::from(
                    <[u8; 4]>::try_from(&packet[12..16]).unwrap(),
                )),
                remote: IpAddr::V4(Ipv4Addr::from(
                    <[u8; 4]>::try_from(&packet[16..20]).unwrap(),
                )),
                hop_limit: packet[8],
                dont_fragment: flags & 0x4000 != 0,
                identity: echo.identity(),
                payload: echo.payload,
            })
        }
        Some(6) => {
            if packet.len() < IPV6_HEADER_LEN {
                return Err(Reject::Malformed("IPv6 header does not fit"));
            }
            if u16::from_be_bytes([packet[4], packet[5]]) as usize != packet.len() - IPV6_HEADER_LEN
            {
                return Err(Reject::Malformed("IPv6 payload length disagrees"));
            }
            // the extension-header set comes from the library rather than a list repeated here, so a chain
            // this parse cannot walk is never mistaken for an unsupported transport
            match IpNumber(packet[6]) {
                IpNumber::IPV6_ICMP => {}
                IpNumber::IPV6_FRAGMENTATION_HEADER => return Err(Reject::Fragmented),
                header if header.is_ipv6_ext_header_value() => return Err(Reject::Extended),
                _ => return Err(Reject::NotUdp),
            }
            let client = Ipv6Addr::from(<[u8; 16]>::try_from(&packet[8..24]).unwrap());
            let remote = Ipv6Addr::from(<[u8; 16]>::try_from(&packet[24..40]).unwrap());
            let echo = message(&packet[IPV6_HEADER_LEN..], icmpv6::TYPE_ECHO_REQUEST)?;
            if Icmpv6Header::with_checksum(
                Icmpv6Type::EchoRequest(echo.header()),
                client.octets(),
                remote.octets(),
                echo.payload,
            )
            .map_err(|_| Reject::Malformed("ICMPv6 echo payload too long"))?
            .checksum
                != echo.checksum
            {
                return Err(Reject::Malformed("ICMPv6 echo checksum mismatch"));
            }
            Ok(Request {
                client: IpAddr::V6(client),
                remote: IpAddr::V6(remote),
                hop_limit: packet[7],
                dont_fragment: true,
                identity: echo.identity(),
                payload: echo.payload,
            })
        }
        _ => Err(Reject::Malformed("not an IPv4 or IPv6 packet")),
    }
}

/// Reads the identifier and sequence out of one Echo Reply as a ping socket delivers it, which is the ICMP
/// message with no IP header in front of it.
///
/// The identifier is the kernel's rather than the client's, since the kernel imposed it on the way out and the
/// remote echoed it back. It is checked for nothing: demultiplexing on it is how this message reached the
/// socket at all.
///
/// The checksum is not verified and cannot usefully be: over IPv4 it covers only the message, which the
/// kernel already checked before demultiplexing, and over IPv6 it covers a pseudo-header whose destination
/// address is the selected network's own and is not reported with the datagram. Nothing here is repeated to
/// the client either - the reply is rebuilt around the client's own identity - so an undetected flip in the
/// payload is a payload the client compares against what it sent, which is where a ping expects to catch it.
pub fn peek_reply(message: &[u8], ipv6: bool) -> Result<(Identity, &[u8]), Reject> {
    peek(
        message,
        if ipv6 {
            icmpv6::TYPE_ECHO_REPLY
        } else {
            icmpv4::TYPE_ECHO_REPLY
        },
    )
}

/// Reads the identity out of an Echo *Request* quoted back inside an ICMP error.
///
/// The sequence in it is the one the daemon substituted, which is what identifies the session - a ping socket's
/// errors name no destination, so this is the only handle on which request the error is about.
pub fn peek_request(message: &[u8], ipv6: bool) -> Result<(Identity, &[u8]), Reject> {
    peek(
        message,
        if ipv6 {
            icmpv6::TYPE_ECHO_REQUEST
        } else {
            icmpv4::TYPE_ECHO_REQUEST
        },
    )
}

fn peek(message: &[u8], expected: u8) -> Result<(Identity, &[u8]), Reject> {
    if message.len() < ECHO_HEADER_LEN {
        return Err(Reject::Malformed("ICMP echo header does not fit"));
    }
    if message[0] != expected || message[1] != 0 {
        return Err(Reject::NotUdp);
    }
    Ok((
        Identity {
            identifier: u16::from_be_bytes([message[4], message[5]]),
            sequence: u16::from_be_bytes([message[6], message[7]]),
        },
        &message[ECHO_HEADER_LEN..],
    ))
}

/// Builds the Echo Request to write to a ping socket, under the sequence the caller allocated.
///
/// The checksum is left zero because the kernel computes it for a ping socket, and the identifier is left
/// zero because the kernel overwrites it: writing either would be writing a value that never reaches the
/// wire, which would then have to be explained at every reader.
pub fn build_request(ipv6: bool, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(ECHO_HEADER_LEN + payload.len());
    message.extend([
        if ipv6 {
            icmpv6::TYPE_ECHO_REQUEST
        } else {
            icmpv4::TYPE_ECHO_REQUEST
        },
        0,
        0,
        0,
        0,
        0,
    ]);
    message.extend_from_slice(&sequence.to_be_bytes());
    message.extend_from_slice(payload);
    message
}

/// Builds the TUN-side Echo Reply for one upstream reply, from the remote that answered to the client that
/// asked, restoring the client's own identifier and sequence.
///
/// `identification` is the IPv4 fragmentation permission and works exactly as in
/// [crate::shared::udp_wire::build_reply]: `None` sets DF, which is what a reply within the downstream floor
/// gets, and `Some` clears it and carries the value Android's downstream fragmentation repeats into every
/// fragment. IPv6 ignores it.
pub fn build_reply(
    remote: IpAddr,
    client: IpAddr,
    hop_limit: u8,
    identification: Option<u16>,
    identity: Identity,
    payload: &[u8],
) -> Result<Vec<u8>, WriterError> {
    build(
        false,
        remote,
        client,
        hop_limit,
        identification,
        identity,
        payload,
    )
}

/// Rebuilds the Echo Request a client sent, as it looked on the TUN.
///
/// Only ever needed as the quote inside an error about that request. The daemon does not keep the packet, so
/// this is a reconstruction: the addresses and the client's own identifier and sequence are exact, which is what
/// lets the client match the error to the ping that caused it, and the payload is empty because inventing bytes
/// the client would then compare against what it sent would be worse than sending none.
pub fn build_request_packet(
    client: IpAddr,
    remote: IpAddr,
    hop_limit: u8,
    identity: Identity,
    payload: &[u8],
) -> Result<Vec<u8>, WriterError> {
    build(true, client, remote, hop_limit, None, identity, payload)
}

/// One Echo packet from `source` to `destination`. `request` picks the type, which differs per family and is
/// never derived by arithmetic on the other one.
fn build(
    request: bool,
    source: IpAddr,
    destination: IpAddr,
    hop_limit: u8,
    identification: Option<u16>,
    identity: Identity,
    payload: &[u8],
) -> Result<Vec<u8>, WriterError> {
    let echo = IcmpEchoHeader {
        id: identity.identifier,
        seq: identity.sequence,
    };
    match (source, destination) {
        (IpAddr::V4(remote), IpAddr::V4(client)) => {
            let icmp = Icmpv4Header::with_checksum(
                if request {
                    Icmpv4Type::EchoRequest(echo)
                } else {
                    Icmpv4Type::EchoReply(echo)
                },
                payload,
            );
            let mut ip = Ipv4Header {
                time_to_live: hop_limit,
                protocol: IpNumber::ICMP,
                source: remote.octets(),
                destination: client.octets(),
                identification: identification.unwrap_or(0),
                dont_fragment: identification.is_none(),
                ..Default::default()
            };
            ip.set_payload_len(ECHO_HEADER_LEN + payload.len())
                .map_err(|_| WriterError::Malformed("echo reply exceeds an IPv4 datagram"))?;
            // last, because to_bytes serializes the stored value rather than recomputing it
            ip.header_checksum = ip.calc_header_checksum();
            Ok(assemble(&ip.to_bytes(), &icmp.to_bytes(), payload))
        }
        (IpAddr::V6(remote), IpAddr::V6(client)) => {
            let mut ip = Ipv6Header {
                next_header: IpNumber::IPV6_ICMP,
                hop_limit,
                source: remote.octets(),
                destination: client.octets(),
                ..Default::default()
            };
            ip.set_payload_length(ECHO_HEADER_LEN + payload.len())
                .map_err(|_| WriterError::Malformed("echo reply exceeds an IPv6 datagram"))?;
            // ICMPv6's checksum covers a pseudo-header, unlike ICMPv4's, so it needs the addresses
            let icmp = Icmpv6Header::with_checksum(
                if request {
                    Icmpv6Type::EchoRequest(echo)
                } else {
                    Icmpv6Type::EchoReply(echo)
                },
                ip.source,
                ip.destination,
                payload,
            )
            .map_err(|_| WriterError::Malformed("echo reply payload too long"))?;
            Ok(assemble(&ip.to_bytes(), &icmp.to_bytes(), payload))
        }
        _ => Err(WriterError::Malformed(
            "echo reply crosses address families",
        )),
    }
}

/// The Echo header's own fields, before either family's checksum has been verified.
struct Echo<'a> {
    checksum: u16,
    identifier: u16,
    sequence: u16,
    payload: &'a [u8],
}

impl Echo<'_> {
    /// The identifier and sequence in the shape both families' checksum builders take. The wrapping type
    /// differs per family and is applied at the call site, which is what keeps one numeric path from
    /// producing an ICMPv4 checksum for an ICMPv6 message.
    fn header(&self) -> IcmpEchoHeader {
        IcmpEchoHeader {
            id: self.identifier,
            seq: self.sequence,
        }
    }

    fn identity(&self) -> Identity {
        Identity {
            identifier: self.identifier,
            sequence: self.sequence,
        }
    }
}

/// The fixed Echo header and the payload behind it. A different type is [Reject::NotUdp] rather than
/// malformed, because it is a well-formed message for somebody else.
fn message(slice: &[u8], expected: u8) -> Result<Echo<'_>, Reject> {
    if slice.len() < ECHO_HEADER_LEN {
        return Err(Reject::Malformed("ICMP echo header does not fit"));
    }
    if slice[0] != expected {
        return Err(Reject::NotUdp);
    }
    // Both RFC 792 and RFC 4443 define Echo with code 0 only. A non-zero one is not an Echo this daemon may
    // repeat: the remote echoes the code back, and the client's stack would then see a message it never sent.
    if slice[1] != 0 {
        return Err(Reject::Malformed("ICMP echo code is not zero"));
    }
    Ok(Echo {
        checksum: u16::from_be_bytes([slice[2], slice[3]]),
        identifier: u16::from_be_bytes([slice[4], slice[5]]),
        sequence: u16::from_be_bytes([slice[6], slice[7]]),
        payload: &slice[ECHO_HEADER_LEN..],
    })
}

fn assemble(ip: &[u8], icmp: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(ip.len() + icmp.len() + payload.len());
    packet.extend_from_slice(ip);
    packet.extend_from_slice(icmp);
    packet.extend_from_slice(payload);
    packet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::packet_writer::validate;

    /// Whatever the identity is, when the test is about something else.
    const ONE: Identity = Identity {
        identifier: 1,
        sequence: 1,
    };

    const CLIENT4: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    const REMOTE4: IpAddr = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
    const CLIENT6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 1, 0, 0, 0, 0, 2));
    const REMOTE6: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0, 0, 0, 0, 0, 0x1111));

    /// A client Echo Request as it would arrive on the TUN. Built by hand rather than through
    /// [build_reply], because a request is not a reply: the type byte differs, so reusing the reply builder
    /// would test the parse against the wrong message.
    fn client_request(
        client: IpAddr,
        remote: IpAddr,
        identifier: u16,
        sequence: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        let echo = IcmpEchoHeader {
            id: identifier,
            seq: sequence,
        };
        match (client, remote) {
            (IpAddr::V4(client), IpAddr::V4(remote)) => {
                let icmp = Icmpv4Header::with_checksum(Icmpv4Type::EchoRequest(echo), payload);
                let mut ip = Ipv4Header {
                    time_to_live: 64,
                    protocol: IpNumber::ICMP,
                    source: client.octets(),
                    destination: remote.octets(),
                    dont_fragment: true,
                    ..Default::default()
                };
                ip.set_payload_len(ECHO_HEADER_LEN + payload.len()).unwrap();
                ip.header_checksum = ip.calc_header_checksum();
                assemble(&ip.to_bytes(), &icmp.to_bytes(), payload)
            }
            (IpAddr::V6(client), IpAddr::V6(remote)) => {
                let mut ip = Ipv6Header {
                    next_header: IpNumber::IPV6_ICMP,
                    hop_limit: 64,
                    source: client.octets(),
                    destination: remote.octets(),
                    ..Default::default()
                };
                ip.set_payload_length(ECHO_HEADER_LEN + payload.len())
                    .unwrap();
                let icmp = Icmpv6Header::with_checksum(
                    Icmpv6Type::EchoRequest(echo),
                    ip.source,
                    ip.destination,
                    payload,
                )
                .unwrap();
                assemble(&ip.to_bytes(), &icmp.to_bytes(), payload)
            }
            _ => unreachable!("test addresses are same-family"),
        }
    }

    #[test]
    fn a_request_parses_into_what_the_table_keys_on() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let packet = client_request(client, remote, 0xbeef, 7, b"probe");
            let request = parse(&packet).unwrap();
            assert_eq!(request.client, client);
            assert_eq!(request.remote, remote);
            assert_eq!(request.hop_limit, 64);
            assert!(request.dont_fragment);
            assert_eq!(
                request.identity,
                Identity {
                    identifier: 0xbeef,
                    sequence: 7
                }
            );
            assert_eq!(request.payload, b"probe");
        }
    }

    #[test]
    fn a_corrupted_checksum_is_rejected_in_both_families() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let mut packet = client_request(client, remote, 1, 1, b"probe");
            // the last payload byte, which every family's checksum covers
            *packet.last_mut().unwrap() ^= 0xff;
            assert_eq!(
                parse(&packet),
                Err(Reject::Malformed(if client.is_ipv6() {
                    "ICMPv6 echo checksum mismatch"
                } else {
                    "ICMPv4 echo checksum mismatch"
                }))
            );
        }
    }

    #[test]
    fn only_echo_requests_are_accepted() {
        // An Echo *Reply* from a client is a well-formed message for somebody else, not a malformed request:
        // it must not be mistaken for something to relay.
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let packet = build_reply(client, remote, 64, None, ONE, b"probe").unwrap();
            assert_eq!(parse(&packet), Err(Reject::NotUdp));
        }
        // and a non-zero code is a request this daemon may not repeat, because the remote echoes it back
        let mut packet = client_request(CLIENT4, REMOTE4, 1, 1, b"probe");
        packet[IPV4_HEADER_LEN + 1] = 3;
        assert_eq!(
            parse(&packet),
            Err(Reject::Malformed("ICMP echo code is not zero"))
        );
    }

    #[test]
    fn a_fragmented_request_is_separated_from_an_unsupported_one() {
        let mut packet = client_request(CLIENT4, REMOTE4, 1, 1, b"probe");
        // clear DF and set a non-zero fragment offset, which is what a trailing fragment looks like
        packet[6] = 0;
        packet[7] = 1;
        assert_eq!(parse(&packet), Err(Reject::Fragmented));
        // Keep fragmentation distinguishable from an unsupported protocol so the two are counted separately.
        let mut packet = client_request(CLIENT6, REMOTE6, 1, 1, b"probe");
        packet[6] = IpNumber::IPV6_FRAGMENTATION_HEADER.0;
        assert_eq!(parse(&packet), Err(Reject::Fragmented));
    }

    #[test]
    fn a_reply_restores_the_clients_identity_and_is_writable() {
        for (client, remote) in [(CLIENT4, REMOTE4), (CLIENT6, REMOTE6)] {
            let packet = build_reply(
                remote,
                client,
                42,
                None,
                Identity {
                    identifier: 0xbeef,
                    sequence: 7,
                },
                b"probe",
            )
            .unwrap();
            assert_eq!(validate(&packet, 1500), Ok(()), "{client}");
            let header = if client.is_ipv6() {
                IPV6_HEADER_LEN
            } else {
                IPV4_HEADER_LEN
            };
            // an Echo Reply of the client's own family, carrying the identifier and sequence it chose
            assert_eq!(
                packet[header],
                if client.is_ipv6() {
                    icmpv6::TYPE_ECHO_REPLY
                } else {
                    icmpv4::TYPE_ECHO_REPLY
                }
            );
            let (reply, payload) = peek_reply(&packet[header..], client.is_ipv6()).unwrap();
            assert_eq!(reply.identifier, 0xbeef);
            assert_eq!(reply.sequence, 7);
            assert_eq!(payload, b"probe");
        }
    }

    #[test]
    fn a_reply_clears_df_exactly_when_it_carries_an_identification() {
        let atomic = build_reply(REMOTE4, CLIENT4, 42, None, ONE, b"probe").unwrap();
        assert_eq!(u16::from_be_bytes([atomic[6], atomic[7]]) & 0x4000, 0x4000);
        let fragmentable = build_reply(REMOTE4, CLIENT4, 42, Some(0x1234), ONE, b"probe").unwrap();
        assert_eq!(
            u16::from_be_bytes([fragmentable[6], fragmentable[7]]) & 0x4000,
            0
        );
        assert_eq!(
            u16::from_be_bytes([fragmentable[4], fragmentable[5]]),
            0x1234
        );
    }

    #[test]
    fn a_request_to_a_ping_socket_leaves_the_kernels_fields_alone() {
        for ipv6 in [false, true] {
            let message = build_request(ipv6, 0x2211, b"probe");
            assert_eq!(
                message[0],
                if ipv6 {
                    icmpv6::TYPE_ECHO_REQUEST
                } else {
                    icmpv4::TYPE_ECHO_REQUEST
                }
            );
            // code, then the checksum and identifier the kernel fills in, then the allocated sequence
            assert_eq!(&message[1..6], &[0, 0, 0, 0, 0]);
            assert_eq!(u16::from_be_bytes([message[6], message[7]]), 0x2211);
            assert_eq!(&message[ECHO_HEADER_LEN..], b"probe");
        }
    }

    #[test]
    fn crossing_families_is_refused() {
        assert!(build_reply(REMOTE4, CLIENT6, 42, None, ONE, b"probe").is_err());
        assert!(build_reply(REMOTE6, CLIENT4, 42, None, ONE, b"probe").is_err());
        assert_eq!(
            parse(&[]),
            Err(Reject::Malformed("not an IPv4 or IPv6 packet"))
        );
    }
}
