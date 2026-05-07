use std::io;

use crate::shared::protocol::DaemonErrorReport;

const FRAME_CALL: u8 = 0;
const FRAME_CANCEL: u8 = 1;

const FRAME_REPLY: u8 = 0;
const FRAME_EVENT: u8 = 1;
const FRAME_ERROR: u8 = 2;
const FRAME_NON_FATAL: u8 = 3;
const NO_CALL_ID: u64 = 0;

#[derive(Debug)]
pub enum ClientFrame {
    Call { id: u64, packet: Vec<u8> },
    Cancel { id: u64 },
}

pub fn parse_client_frame(packet: &[u8]) -> io::Result<ClientFrame> {
    let mut parser = Parser::new(packet);
    match parser.read_u8()? {
        FRAME_CALL => Ok(ClientFrame::Call {
            id: parser.read_call_id()?,
            packet: parser.remaining().to_vec(),
        }),
        FRAME_CANCEL => {
            let id = parser.read_call_id()?;
            if !parser.remaining().is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cancel frame has payload",
                ));
            }
            Ok(ClientFrame::Cancel { id })
        }
        frame => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown client frame {frame}"),
        )),
    }
}

pub fn reply_frame(id: u64, packet: Vec<u8>) -> Vec<u8> {
    write_frame(FRAME_REPLY, id, packet)
}

pub fn event_frame(id: u64, packet: Vec<u8>) -> Vec<u8> {
    write_frame(FRAME_EVENT, id, packet)
}

pub fn error_frame(id: u64, report: DaemonErrorReport) -> Vec<u8> {
    let mut frame = write_frame_header(FRAME_ERROR, id);
    write_error_report(&mut frame, &report);
    frame
}

pub fn nonfatal_frame(id: Option<u64>, report: DaemonErrorReport) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.push(FRAME_NON_FATAL);
    frame.extend_from_slice(&id.unwrap_or(NO_CALL_ID).to_be_bytes());
    write_error_report(&mut frame, &report);
    frame
}

fn write_frame(frame_type: u8, id: u64, packet: Vec<u8>) -> Vec<u8> {
    let mut frame = write_frame_header(frame_type, id);
    frame.extend(packet);
    frame
}

fn write_frame_header(frame_type: u8, id: u64) -> Vec<u8> {
    assert!(id != NO_CALL_ID, "invalid daemon call id {id}");
    let mut frame = Vec::new();
    frame.push(frame_type);
    frame.extend_from_slice(&id.to_be_bytes());
    frame
}

fn write_error_report(packet: &mut Vec<u8>, report: &DaemonErrorReport) {
    write_utf(packet, &report.context);
    write_utf(packet, &report.message);
    packet.extend_from_slice(&report.errno.unwrap_or(-1).to_be_bytes());
    write_utf(packet, &report.kind);
    write_utf(packet, &report.file);
    packet.extend_from_slice(&report.line.to_be_bytes());
    packet.extend_from_slice(&report.column.to_be_bytes());
    packet.extend_from_slice(&report.pid.to_be_bytes());
    packet.extend_from_slice(&(report.details.len() as u32).to_be_bytes());
    for (key, value) in &report.details {
        write_utf(packet, key);
        write_utf(packet, value);
    }
}

fn write_utf(packet: &mut Vec<u8>, value: &str) {
    packet.extend_from_slice(&(value.len() as u32).to_be_bytes());
    packet.extend_from_slice(value.as_bytes());
}

struct Parser<'a> {
    packet: &'a [u8],
    offset: usize,
}

impl<'a> Parser<'a> {
    fn new(packet: &'a [u8]) -> Self {
        Self { packet, offset: 0 }
    }

    fn read_u8(&mut self) -> io::Result<u8> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_u64(&mut self) -> io::Result<u64> {
        let bytes = self.read_exact(8)?;
        Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
    }

    fn read_call_id(&mut self) -> io::Result<u64> {
        match self.read_u64()? {
            NO_CALL_ID => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid daemon call id 0",
            )),
            id => Ok(id),
        }
    }

    fn read_exact(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if self.offset + count > self.packet.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short frame"));
        }
        let bytes = &self.packet[self.offset..self.offset + count];
        self.offset += count;
        Ok(bytes)
    }

    fn remaining(&self) -> &'a [u8] {
        &self.packet[self.offset..]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_call_frame_reads_id_and_payload() {
        let mut packet = Vec::new();
        packet.push(FRAME_CALL);
        packet.extend_from_slice(&123u64.to_be_bytes());
        packet.extend_from_slice(&[1, 2, 3]);

        let ClientFrame::Call { id, packet } = parse_client_frame(&packet).unwrap() else {
            panic!("expected call frame");
        };
        assert_eq!(id, 123);
        assert_eq!(packet, vec![1, 2, 3]);
    }

    #[test]
    fn parse_cancel_frame_rejects_zero_id() {
        let mut packet = Vec::new();
        packet.push(FRAME_CANCEL);
        packet.extend_from_slice(&0u64.to_be_bytes());

        let error = parse_client_frame(&packet).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn reply_frame_encodes_id_and_payload() {
        let packet = reply_frame(9, vec![1, 2]);

        assert_eq!(packet[0], FRAME_REPLY);
        assert_eq!(u64::from_be_bytes(packet[1..9].try_into().unwrap()), 9);
        assert_eq!(&packet[9..], &[1, 2]);
    }

    #[test]
    fn event_and_error_frames_encode_id() {
        let report = DaemonErrorReport {
            context: "context".to_owned(),
            message: "message".to_owned(),
            errno: Some(libc::EIO),
            kind: "kind".to_owned(),
            file: "file.rs".to_owned(),
            line: 1,
            column: 2,
            pid: 3,
            details: Vec::new(),
        };

        let event = event_frame(10, vec![4, 5]);
        assert_eq!(event[0], FRAME_EVENT);
        assert_eq!(u64::from_be_bytes(event[1..9].try_into().unwrap()), 10);
        assert_eq!(&event[9..], &[4, 5]);
        let error = error_frame(11, report);
        assert_eq!(error[0], FRAME_ERROR);
        assert_eq!(u64::from_be_bytes(error[1..9].try_into().unwrap()), 11);
    }

    #[test]
    fn nonfatal_frame_allows_missing_id() {
        let report = DaemonErrorReport {
            context: "context".to_owned(),
            message: "message".to_owned(),
            errno: None,
            kind: "kind".to_owned(),
            file: "file.rs".to_owned(),
            line: 1,
            column: 2,
            pid: 3,
            details: Vec::new(),
        };

        let packet = nonfatal_frame(None, report);

        assert_eq!(packet[0], FRAME_NON_FATAL);
        assert_eq!(u64::from_be_bytes(packet[1..9].try_into().unwrap()), 0);
    }
}
