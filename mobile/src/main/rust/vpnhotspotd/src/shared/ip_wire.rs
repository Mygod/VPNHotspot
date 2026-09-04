use etherparse::{IpNumber, Ipv4HeaderSlice, Ipv6HeaderSlice};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Ipv4Header,
    Ipv4Length,
    Ipv6Header,
    Ipv6Length,
    NotIp,
}

impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Ipv4Header => "IPv4 header does not fit",
            Self::Ipv4Length => "IPv4 total length disagrees",
            Self::Ipv6Header => "IPv6 header does not fit",
            Self::Ipv6Length => "IPv6 payload length disagrees",
            Self::NotIp => "not an IPv4 or IPv6 packet",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Ipv4 {
        header: Ipv4HeaderSlice<'a>,
        payload: &'a [u8],
    },
    Ipv6 {
        header: Ipv6HeaderSlice<'a>,
        payload: &'a [u8],
    },
}

impl<'a> Packet<'a> {
    pub fn parse(packet: &'a [u8]) -> Result<Self, Error> {
        match packet.first().map(|byte| byte >> 4) {
            Some(4) => {
                let header = Ipv4HeaderSlice::from_slice(packet).map_err(|_| Error::Ipv4Header)?;
                if usize::from(header.total_len()) != packet.len() {
                    return Err(Error::Ipv4Length);
                }
                let payload = &packet[header.slice().len()..];
                Ok(Self::Ipv4 { header, payload })
            }
            Some(6) => {
                let header = Ipv6HeaderSlice::from_slice(packet).map_err(|_| Error::Ipv6Header)?;
                let payload_length = usize::from(header.payload_length());
                if packet.len().checked_sub(header.slice().len()) != Some(payload_length) {
                    return Err(Error::Ipv6Length);
                }
                Ok(Self::Ipv6 {
                    payload: &packet[header.slice().len()..],
                    header,
                })
            }
            _ => Err(Error::NotIp),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ipv6Payload {
    Transport,
    Fragment,
    Extension,
    Other,
}

/// Classifies only the base header's next-header value. Extension walking and Fragment parsing remain with
/// their bounded owners, so a fragmented transport is not hidden by a parser that declines to slice it.
pub fn ipv6_payload(next: IpNumber, transport: IpNumber) -> Ipv6Payload {
    if next == transport {
        Ipv6Payload::Transport
    } else if next == IpNumber::IPV6_FRAGMENTATION_HEADER {
        Ipv6Payload::Fragment
    } else if next.is_ipv6_ext_header_value() {
        Ipv6Payload::Extension
    } else {
        Ipv6Payload::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ipv4() -> Vec<u8> {
        vec![
            0x45,
            0,
            0,
            20,
            0,
            0,
            0,
            0,
            64,
            IpNumber::UDP.0,
            0,
            0,
            192,
            0,
            2,
            1,
            198,
            51,
            100,
            7,
        ]
    }

    fn ipv6() -> Vec<u8> {
        let mut packet = vec![0x60, 0, 0, 0, 0, 1, IpNumber::UDP.0, 64];
        packet.extend_from_slice(&[0; 32]);
        packet.push(0);
        packet
    }

    #[test]
    fn exact_declared_length_is_required() {
        assert!(matches!(Packet::parse(&ipv4()), Ok(Packet::Ipv4 { .. })));
        assert!(matches!(Packet::parse(&ipv6()), Ok(Packet::Ipv6 { .. })));

        let mut packet = ipv4();
        packet.push(0);
        assert_eq!(Packet::parse(&packet), Err(Error::Ipv4Length));

        let mut packet = ipv6();
        packet.push(0);
        assert_eq!(Packet::parse(&packet), Err(Error::Ipv6Length));
    }

    #[test]
    fn ipv6_zero_payload_length_requires_an_empty_payload() {
        let mut packet = ipv6();
        packet[4..6].fill(0);
        assert_eq!(Packet::parse(&packet), Err(Error::Ipv6Length));
        packet.truncate(40);
        packet[6] = IpNumber::IPV6_NO_NEXT_HEADER.0;
        assert!(matches!(
            Packet::parse(&packet),
            Ok(Packet::Ipv6 { payload, .. }) if payload.is_empty()
        ));
    }
}
