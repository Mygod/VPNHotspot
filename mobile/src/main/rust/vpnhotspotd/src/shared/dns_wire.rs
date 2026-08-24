const HEADER_LEN: usize = 12;
const FLAG_RESPONSE: u8 = 0x80;
const FLAG_RECURSION_DESIRED: u8 = 0x01;
const FLAG_RECURSION_AVAILABLE: u8 = 0x80;
const FLAGS_OPCODE_MASK: u8 = 0x78;
const FLAGS_AD_CD_MASK: u8 = 0x30;
const RCODE_SERVFAIL: u8 = 2;

pub fn servfail_response(query: &[u8]) -> Option<Vec<u8>> {
    if query.len() < HEADER_LEN || query[2] & FLAG_RESPONSE != 0 {
        return None;
    }
    let question_end = question_section_end(query)?;
    let mut response = Vec::with_capacity(question_end);
    response.extend_from_slice(&query[..2]);
    // Preserve the query opcode and RD bit, clear authoritative/truncated bits, and mark this as
    // a recursive server response with SERVFAIL. AD/CD are copied because clients may use them to
    // express DNSSEC validation preferences even though this daemon only forwards packets.
    response
        .push(FLAG_RESPONSE | (query[2] & FLAGS_OPCODE_MASK) | (query[2] & FLAG_RECURSION_DESIRED));
    response.push(FLAG_RECURSION_AVAILABLE | (query[3] & FLAGS_AD_CD_MASK) | RCODE_SERVFAIL);
    response.extend_from_slice(&query[4..6]);
    response.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    response.extend_from_slice(&query[HEADER_LEN..question_end]);
    Some(response)
}

fn question_section_end(query: &[u8]) -> Option<usize> {
    let questions = u16::from_be_bytes([query[4], query[5]]);
    let mut offset = HEADER_LEN;
    for _ in 0..questions {
        offset = name_end(query, offset)?;
        offset = offset.checked_add(4)?;
        if offset > query.len() {
            return None;
        }
    }
    Some(offset)
}

// Walk one DNS name in wire format without allocating. This accepts ordinary labels and compression
// pointers. Pointers terminate the encoded name at the pointer itself; we do not need to follow
// them because this helper only needs to preserve the original question bytes in the response.
fn name_end(packet: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let length = *packet.get(offset)?;
        offset += 1;
        match length & 0xC0 {
            0x00 => {
                if length == 0 {
                    return Some(offset);
                }
                offset = offset.checked_add(length as usize)?;
                if offset > packet.len() {
                    return None;
                }
            }
            0xC0 => {
                packet.get(offset)?;
                return Some(offset + 1);
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::servfail_response;

    #[test]
    fn error_response_preserves_question() {
        let query = [
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 10, b'c', b'l',
            b'o', b'u', b'd', b'f', b'l', b'a', b'r', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01,
            0x00, 0x01, 0x00, 0x00, 0x29, 0x04, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let response = servfail_response(&query).unwrap();
        assert_eq!(&response[..4], &[0x12, 0x34, 0x81, 0x82]);
        assert_eq!(
            &response[4..12],
            &[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(&response[12..32], &query[12..32]);
        assert_eq!(response.len(), 32);
    }

    #[test]
    fn error_response_handles_compressed_questions() {
        let query = [
            0x12, 0x34, 0x01, 0x10, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
            b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01, 0xC0,
            0x0C, 0x00, 0x1C, 0x00, 0x01,
        ];
        let response = servfail_response(&query).unwrap();
        assert_eq!(&response[..4], &[0x12, 0x34, 0x81, 0x92]);
        assert_eq!(&response[4..12], &[0x00, 0x02, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&response[12..], &query[12..]);
    }

    #[test]
    fn error_response_rejects_malformed_queries() {
        assert!(servfail_response(&[]).is_none());
        assert!(servfail_response(&[0; 12]).is_some());
        assert!(servfail_response(&[0, 0, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        assert!(servfail_response(&[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 3, b'w']).is_none());
    }
}
