use crate::shared::failure::Failure;

const HEADER_LEN: usize = 12;
const FLAG_RESPONSE: u8 = 0x80;
const FLAG_RECURSION_DESIRED: u8 = 0x01;
const FLAG_RECURSION_AVAILABLE: u8 = 0x80;
const FLAGS_OPCODE_MASK: u8 = 0x78;
const FLAGS_AD_CD_MASK: u8 = 0x30;
const RCODE_SERVFAIL: u8 = 2;

/// The length prefix in front of every message on a DNS stream.
///
/// https://www.rfc-editor.org/rfc/rfc1035#section-4.2.2
pub const PREFIX: usize = 2;

/// A DNS message cannot exceed what its 16-bit length prefix can describe.
pub const MAX_MESSAGE: usize = u16::MAX as usize;

/// Where a framed message's bytes are stored: a buffer whose capacity was granted before it existed.
///
/// A trait rather than a buffer type because the two halves belong in different places. The storage is the
/// daemon's - admitted by its ingress owner, charged against its aggregate, counted by whoever owns it -
/// while the framing is here, on the attacker-facing side that has to be answerable without a device. What
/// every implementation promises is the one thing the bound rests on: appending never grows the buffer past
/// the capacity that was admitted for it.
pub trait Body {
    /// Appends as much of `bytes` as the granted capacity still has room for, and answers how much that was.
    fn extend_within_capacity(&mut self, bytes: &[u8]) -> usize;
}

/// What the next bytes of a DNS stream turned out to be.
pub enum Framed {
    /// A whole length prefix. The message is *announced* and not one byte of it is stored: its bytes may only
    /// be kept once that many have been admitted, which is what the caller does next.
    Length(usize),
    /// The announced message is complete - filled into the admitted body, or skipped when there was none to
    /// fill.
    Message,
    /// This chunk is spent. Ordinary: TCP does not promise that one read is one message, and a client
    /// dribbling a query one byte at a time is doing nothing wrong.
    Hungry,
    /// A zero-length message, which is not a query and never becomes one - so there is nothing to wait for,
    /// and nothing after it can be trusted to be framed either. The stream is over.
    ///
    /// Also what a body smaller than the length it was admitted for reads as, which is this daemon
    /// disagreeing with itself rather than a client doing anything: the capacity is granted from exactly the
    /// announced length, so it cannot happen, and if it ever did, storing a truncated message would be worse
    /// than ending the stream.
    Broken,
}

/// The message boundaries of a DNS-over-TCP stream, framed *before* anything is stored.
///
/// What bounds the memory is the length prefix, and the way it bounds it is the whole point of this type. The
/// prefix is parsed out of the stream first, the length it announces is admitted by the ingress owner, and
/// only then does the message have anywhere to go. So what this holds between reads is two bytes and a count:
/// never a buffer a client can grow by dribbling, and never a copy taken before anything agreed to pay for
/// it. Kept here, away from the transport, because this is the attacker-facing half of the flow and the half
/// that has to be answerable without a device.
#[derive(Default)]
pub struct DnsStream {
    /// The length prefix as it arrives, one byte at a time if that is how the client sends it.
    prefix: [u8; PREFIX],
    /// How much of that prefix is here.
    framed: usize,
    /// What the announced message still wants. `None` between messages.
    wanted: Option<usize>,
}

impl DnsStream {
    /// Whether any bytes of a message have arrived without completing one.
    ///
    /// What a half-close has to ask: a client that stops sending with a complete message boundary behind it
    /// has finished asking, while one that stops mid-length or mid-message has truncated its own request, and
    /// the two end the connection differently.
    pub fn partial(&self) -> bool {
        self.framed > 0 || self.wanted.is_some()
    }

    /// Frames the next step out of `chunk`, consuming exactly what that step used.
    ///
    /// Called until it answers [Framed::Hungry], because one read may carry several messages or a fraction of
    /// one. `body` is the buffer admitted for the length this stream last announced: `None` before anything is
    /// announced, and `None` for an announced message nothing could be granted for, whose bytes are skipped so
    /// that the stream stays framed and the client may ask again.
    pub fn advance(&mut self, chunk: &mut &[u8], body: Option<&mut dyn Body>) -> Framed {
        let Some(wanted) = self.wanted else {
            while self.framed < PREFIX {
                let Some((byte, rest)) = chunk.split_first() else {
                    return Framed::Hungry;
                };
                self.prefix[self.framed] = *byte;
                self.framed += 1;
                *chunk = rest;
            }
            let length = u16::from_be_bytes(self.prefix) as usize;
            if length == 0 {
                return Framed::Broken;
            }
            // Announced, and nothing of it is stored: the caller admits this length before the next call has
            // anywhere to put it.
            self.wanted = Some(length);
            return Framed::Length(length);
        };
        let taken = wanted.min(chunk.len());
        if let Some(body) = body {
            if body.extend_within_capacity(&chunk[..taken]) < taken {
                return Framed::Broken;
            }
        }
        *chunk = &chunk[taken..];
        match wanted - taken {
            0 => {
                self.wanted = None;
                self.framed = 0;
                Framed::Message
            }
            left => {
                self.wanted = Some(left);
                Framed::Hungry
            }
        }
    }
}

/// Length-prefixes one answer, or refuses one no prefix could describe.
pub fn frame(answer: &[u8]) -> Option<Vec<u8>> {
    let length = u16::try_from(answer.len()).ok()?;
    let mut framed = Vec::with_capacity(PREFIX + answer.len());
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(answer);
    Some(framed)
}

/// What one DNS query is answered with when the resolver did not answer it, and which failures are still the
/// daemon's own afterwards.
///
/// The resolver's own outcomes belong to the client rather than to the app. A refusal, a timeout, a full
/// per-UID limiter, a name that does not resolve: each is what one query the client chose to send answers, and
/// the client chooses how many of those it sends. So each becomes a SERVFAIL for *that* message and nothing
/// else happens - the connection carries on, because a reset per unresolvable name would end a stream the
/// client is entitled to keep using, and a report per unresolvable name would be a report flood any local app
/// could drive by looking up nonsense.
///
/// Only the daemon's own wrapper around the transaction - making the resolver's descriptor nonblocking,
/// registering it with the runtime - is this process's doing, and that is the one thing that comes back as a
/// failure for the caller to report and end the stream on. A query too malformed for a SERVFAIL to be built
/// from comes back as one too: there is no question to echo, so there is nothing to answer with.
pub fn resolved(result: Result<Vec<u8>, Failure>, query: &[u8]) -> Result<Vec<u8>, Failure> {
    match result {
        Ok(answer) => Ok(answer),
        Err(failure) if failure.reportable().is_none() => servfail_response(query).ok_or(failure),
        Err(failure) => Err(failure),
    }
}

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

    /// The daemon's admitted buffer, as this side of the framing sees one: a capacity granted before the
    /// message exists, which appending may fill and may never grow past.
    ///
    /// The daemon's own is its counted `Owned` buffer; what this stands in for is only the capacity contract,
    /// because that is the half of it the framing depends on.
    struct Admitted(Vec<u8>);

    impl Admitted {
        fn granted(capacity: usize) -> Self {
            Self(Vec::with_capacity(capacity))
        }
    }

    impl Body for Admitted {
        fn extend_within_capacity(&mut self, bytes: &[u8]) -> usize {
            let room = self.0.capacity() - self.0.len();
            let taken = room.min(bytes.len());
            self.0.extend_from_slice(&bytes[..taken]);
            taken
        }
    }

    /// A half-close with a complete message boundary behind it is a client that has finished asking; one
    /// with bytes that never completed a message has truncated its own request, and the two end differently.
    #[test]
    fn a_partial_message_is_distinguishable_from_a_clean_boundary() {
        let mut stream = DnsStream::default();
        assert!(!stream.partial(), "nothing received is not partial");

        // The prefix comes out first and stores nothing, which is what lets its length be admitted before
        // there is anywhere to put the message.
        let message = vec![0xabu8; 12];
        let framed = frame(&message).expect("framed");
        let mut chunk = &framed[..];
        let Framed::Length(length) = stream.advance(&mut chunk, None) else {
            panic!("a whole prefix announces a length")
        };
        assert_eq!(length, message.len());
        assert!(stream.partial(), "a message is being framed");

        // A whole message, filled into the buffer that length was admitted for: the boundary is clean again.
        let mut body = Admitted::granted(length);
        assert!(matches!(
            stream.advance(&mut chunk, Some(&mut body)),
            Framed::Message
        ));
        assert_eq!(body.0, message);
        assert!(chunk.is_empty(), "and the chunk is spent");
        assert!(
            !stream.partial(),
            "a consumed message leaves a clean boundary"
        );

        // Half a length prefix is partial, and so is a message short of its own length.
        let mut half = &framed[..1];
        assert!(matches!(stream.advance(&mut half, None), Framed::Hungry));
        assert!(stream.partial());
        let mut stream = DnsStream::default();
        let mut truncated = &framed[..framed.len() - 1];
        assert!(matches!(
            stream.advance(&mut truncated, None),
            Framed::Length(12)
        ));
        let mut body = Admitted::granted(12);
        assert!(matches!(
            stream.advance(&mut truncated, Some(&mut body)),
            Framed::Hungry
        ));
        assert!(stream.partial(), "a truncated message is partial");
    }

    use std::io;

    use super::*;

    /// One ordinary query for `example.com`, which is what a client's stream carries.
    fn query(id: u16) -> Vec<u8> {
        let mut query = vec![
            0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
            b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
        ];
        query[..2].copy_from_slice(&id.to_be_bytes());
        query
    }

    fn errno(errno: i32) -> io::Error {
        io::Error::from_raw_os_error(errno)
    }

    fn rcode(response: &[u8]) -> u8 {
        response[3] & 0x0f
    }

    /// Every outcome a client can drive has to become an answer *for its own message*, with that message's ID
    /// and question echoed back, because a resolver failure is not a protocol failure. `EBUSY` is the
    /// platform's per-UID limiter, which a burst of a client's own queries reaches on its own.
    #[test]
    fn a_platform_resolver_outcome_answers_its_own_query() {
        for code in [
            libc::EBUSY,
            libc::ETIMEDOUT,
            libc::ECONNREFUSED,
            libc::ENONET,
            libc::EINVAL,
        ] {
            let query = query(0x1234);
            let answered =
                resolved(Err(Failure::platform(errno(code))), &query).unwrap_or_else(|_| {
                    panic!("{code} is the client's answer, not the daemon's failure")
                });
            assert_eq!(&answered[..2], &query[..2], "{code}");
            assert_eq!(rcode(&answered), RCODE_SERVFAIL, "{code}");
            assert_eq!(&answered[HEADER_LEN..], &query[HEADER_LEN..], "{code}");
        }
        // The daemon's own wrapper is the one thing that comes back for the caller to report and reset on.
        let local = resolved(
            Err(Failure::local("resolver.register")(errno(libc::EMFILE))),
            &query(1),
        )
        .expect_err("the daemon's own setup failure is not an answer");
        assert_eq!(
            local.reportable().map(|(context, _)| context),
            Some("resolver.register")
        );
        // Neither is a query nothing can be built from: there is no question to echo back.
        assert!(resolved(Err(Failure::platform(errno(libc::EBUSY))), &[]).is_err());
    }

    /// The property a per-query SERVFAIL exists for: the connection outlives the failure. Two queries arrive
    /// in one segment, the first is one the resolver refused, and the second still has to be read and
    /// answered on the same stream.
    #[test]
    fn a_servfail_does_not_end_the_stream_it_was_answered_on() {
        let mut stream = DnsStream::default();
        let mut segment = Vec::new();
        for id in [1, 2] {
            let query = query(id);
            segment.extend_from_slice(&frame(&query).expect("a query fits its own prefix"));
        }
        let mut chunk = &segment[..];

        let first = message(&mut stream, &mut chunk).expect("a whole query in one segment");
        assert_eq!(&first[..2], &[0, 1]);
        // Refused by the platform, answered to the client, and nothing about the stream changes.
        let answer = resolved(Err(Failure::platform(errno(libc::EBUSY))), &first)
            .expect("a refusal is answered rather than reset");
        assert_eq!(rcode(&answer), RCODE_SERVFAIL);
        assert_eq!(
            frame(&answer).expect("an answer fits its prefix").len(),
            PREFIX + answer.len()
        );

        // The second query was behind it all along, and is still there to be served.
        let second = message(&mut stream, &mut chunk).expect("readable after a SERVFAIL");
        assert_eq!(&second[..2], &[0, 2]);
        assert_eq!(
            resolved(Ok(vec![9, 9]), &second).expect("an answer is an answer"),
            vec![9, 9]
        );
        assert!(chunk.is_empty());
        assert!(matches!(stream.advance(&mut chunk, None), Framed::Hungry));
    }

    /// Everything the transport does with a chunk, minus the ownership: frame it, admit exactly the length
    /// each prefix announces, and answer with the message that completed. `None` is a chunk that ran out
    /// before one did.
    fn message(stream: &mut DnsStream, chunk: &mut &[u8]) -> Option<Vec<u8>> {
        let mut body: Option<Admitted> = None;
        loop {
            match stream.advance(chunk, body.as_mut().map(|body| body as &mut dyn Body)) {
                // Admitted at exactly the announced length, which is the only capacity this message ever gets.
                Framed::Length(length) => body = Some(Admitted::granted(length)),
                Framed::Message => return Some(body.expect("a message was being filled").0),
                Framed::Hungry => return None,
                Framed::Broken => panic!("a well-formed stream does not break"),
            }
        }
    }

    /// A stream is bytes, not messages: a client may dribble one and may put several in one read, and only a
    /// length that can never complete ends it.
    #[test]
    fn a_stream_reassembles_and_only_a_zero_length_message_breaks_it() {
        let mut stream = DnsStream::default();
        let framed = frame(&query(7)).expect("a query fits its own prefix");
        let mut nothing: &[u8] = &[];
        assert!(matches!(stream.advance(&mut nothing, None), Framed::Hungry));
        // One byte at a time: the prefix announces its length as soon as its second byte lands, and every
        // byte after that goes into the buffer that length was admitted for.
        let mut body: Option<Admitted> = None;
        for byte in &framed[..framed.len() - 1] {
            let mut dribble = std::slice::from_ref(byte);
            match stream.advance(
                &mut dribble,
                body.as_mut().map(|body| body as &mut dyn Body),
            ) {
                Framed::Length(length) => {
                    assert_eq!(length, query(7).len());
                    body = Some(Admitted::granted(length));
                }
                Framed::Hungry => {}
                Framed::Message | Framed::Broken => panic!("the message is not complete yet"),
            }
            assert!(stream.partial(), "a message is being framed");
        }
        let mut last = &framed[framed.len() - 1..];
        assert!(matches!(
            stream.advance(&mut last, body.as_mut().map(|body| body as &mut dyn Body)),
            Framed::Message
        ));
        assert_eq!(body.expect("filled").0, query(7));
        assert!(!stream.partial());

        // Nothing waited for and nothing recoverable: a zero length is not a query and every byte behind it
        // is at an offset nothing can resynchronize on.
        let mut zero: &[u8] = &[0, 0, 0, 1];
        assert!(matches!(stream.advance(&mut zero, None), Framed::Broken));
        // And an answer no prefix can describe is refused rather than truncated.
        assert!(frame(&vec![0u8; MAX_MESSAGE]).is_some());
        assert!(frame(&vec![0u8; MAX_MESSAGE + 1]).is_none());
    }

    /// A message nothing could be admitted for is skipped rather than stored, and the stream stays framed
    /// behind it: the client's next query is read exactly as if the refused one had been answered.
    ///
    /// This is the shape a refusal takes on a stream. There is no back-pressure to apply and no offset to
    /// resynchronize on, so the announced bytes have to be consumed by *something* - and consuming them into
    /// a buffer nobody admitted is the allocation the admission exists to prevent.
    #[test]
    fn a_message_with_no_body_is_skipped_and_the_stream_stays_framed() {
        let mut stream = DnsStream::default();
        let mut segment = Vec::new();
        for id in [11, 12] {
            segment.extend_from_slice(&frame(&query(id)).expect("a query fits its own prefix"));
        }
        let mut chunk = &segment[..];

        // Announced, refused, and skipped: not one byte of it is kept.
        assert!(matches!(
            stream.advance(&mut chunk, None),
            Framed::Length(_)
        ));
        assert!(matches!(stream.advance(&mut chunk, None), Framed::Message));
        assert!(!stream.partial(), "and the boundary is clean again");

        // The query behind it is framed as usual.
        let next = message(&mut stream, &mut chunk).expect("the stream is still framed");
        assert_eq!(&next[..2], &[0, 12]);
    }

    /// A body smaller than the length it was admitted for is this daemon disagreeing with itself, and it ends
    /// the stream rather than storing a truncated message.
    ///
    /// Unreachable by construction - the capacity is granted from exactly the announced length - which is why
    /// it is stated here rather than assumed: what would otherwise happen is a query silently cut short and
    /// then resolved as though the client had sent it that way.
    #[test]
    fn a_body_shorter_than_its_admitted_length_breaks_the_stream() {
        let mut stream = DnsStream::default();
        let framed = frame(&query(3)).expect("a query fits its own prefix");
        let mut chunk = &framed[..];
        let Framed::Length(length) = stream.advance(&mut chunk, None) else {
            panic!("a whole prefix announces a length")
        };
        let mut body = Admitted::granted(length - 1);
        assert!(matches!(
            stream.advance(&mut chunk, Some(&mut body)),
            Framed::Broken
        ));
    }

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
