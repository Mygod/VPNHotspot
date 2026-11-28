use std::io;

use etherparse::UdpHeader;

/// Byte offset of the checksum in the RFC 768 UDP header (source port, destination port, length,
/// checksum). Raw IPv6 UDP senders pass it as `IPV6_CHECKSUM` so the kernel fills the checksum.
pub const UDP_CHECKSUM_OFFSET: usize = 6;

/// Builds the header for a raw IPv6 UDP datagram whose socket has `IPV6_CHECKSUM` set to
/// [`UDP_CHECKSUM_OFFSET`]. The checksum field stays zero for the kernel to fill.
pub fn kernel_checksum_udp_header(
    source_port: u16,
    destination_port: u16,
    payload_len: usize,
) -> io::Result<[u8; UdpHeader::LEN]> {
    let length = u16::try_from(UdpHeader::LEN + payload_len).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "udp payload does not fit the udp length field",
        )
    })?;
    Ok(UdpHeader {
        source_port,
        destination_port,
        length,
        checksum: 0,
    }
    .to_bytes())
}

#[cfg(test)]
mod tests {
    use std::io;

    use etherparse::UdpHeader;

    use super::{kernel_checksum_udp_header, UDP_CHECKSUM_OFFSET};

    #[test]
    fn header_carries_ports_length_and_empty_checksum() {
        let header = kernel_checksum_udp_header(3478, 50000, 20).unwrap();
        assert_eq!(header, [0x0d, 0x96, 0xc3, 0x50, 0x00, 28, 0x00, 0x00]);
    }

    #[test]
    fn checksum_offset_matches_udp_layout() {
        let header = UdpHeader {
            source_port: 1,
            destination_port: 2,
            length: 8,
            checksum: 0xabcd,
        }
        .to_bytes();
        assert_eq!(
            &header[UDP_CHECKSUM_OFFSET..UDP_CHECKSUM_OFFSET + 2],
            &[0xab, 0xcd]
        );
    }

    #[test]
    fn length_field_bounds_payload() {
        let largest = usize::from(u16::MAX) - UdpHeader::LEN;
        let header = kernel_checksum_udp_header(1, 2, largest).unwrap();
        assert_eq!(&header[4..6], &[0xff, 0xff]);
        assert_eq!(
            kernel_checksum_udp_header(1, 2, largest + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
