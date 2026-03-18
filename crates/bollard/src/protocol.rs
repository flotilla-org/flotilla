use std::{
    io::{Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub cmd: Option<String>,
    pub status: SessionStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Attached,
    Detached,
}

const TAG_ATTACH_INIT: u8 = 1;
const TAG_INPUT: u8 = 2;
const TAG_OUTPUT: u8 = 3;
const TAG_RESIZE: u8 = 4;
const TAG_ACK: u8 = 5;
const TAG_BUSY: u8 = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    AttachInit { cols: u16, rows: u16 },
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Ack,
    Busy,
}

impl Frame {
    pub fn read(reader: &mut impl Read) -> Result<Self, String> {
        let mut header = [0u8; 5];
        reader.read_exact(&mut header).map_err(|err| err.to_string())?;
        let tag = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload).map_err(|err| err.to_string())?;
        Self::decode(tag, payload)
    }

    pub fn write(&self, writer: &mut impl Write) -> Result<(), String> {
        let (tag, payload) = self.encode();
        let mut header = [0u8; 5];
        header[0] = tag;
        header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        writer.write_all(&header).map_err(|err| err.to_string())?;
        writer.write_all(&payload).map_err(|err| err.to_string())
    }

    fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            Frame::AttachInit { cols, rows } | Frame::Resize { cols, rows } => {
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&cols.to_le_bytes());
                payload.extend_from_slice(&rows.to_le_bytes());
                let tag = if matches!(self, Frame::AttachInit { .. }) { TAG_ATTACH_INIT } else { TAG_RESIZE };
                (tag, payload)
            }
            Frame::Input(bytes) => (TAG_INPUT, bytes.clone()),
            Frame::Output(bytes) => (TAG_OUTPUT, bytes.clone()),
            Frame::Ack => (TAG_ACK, vec![]),
            Frame::Busy => (TAG_BUSY, vec![]),
        }
    }

    fn decode(tag: u8, payload: Vec<u8>) -> Result<Self, String> {
        match tag {
            TAG_ATTACH_INIT => decode_size_frame(payload).map(|(cols, rows)| Frame::AttachInit { cols, rows }),
            TAG_RESIZE => decode_size_frame(payload).map(|(cols, rows)| Frame::Resize { cols, rows }),
            TAG_INPUT => Ok(Frame::Input(payload)),
            TAG_OUTPUT => Ok(Frame::Output(payload)),
            TAG_ACK => Ok(Frame::Ack),
            TAG_BUSY => Ok(Frame::Busy),
            _ => Err(format!("unknown frame tag {tag}")),
        }
    }
}

fn decode_size_frame(payload: Vec<u8>) -> Result<(u16, u16), String> {
    if payload.len() != 4 {
        return Err("invalid size frame".into());
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    Ok((cols, rows))
}

#[cfg(test)]
mod tests {
    use super::Frame;

    #[test]
    fn frame_round_trip_preserves_size_message() {
        let frame = Frame::AttachInit { cols: 120, rows: 40 };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }

    #[test]
    fn frame_round_trip_preserves_binary_payloads() {
        let frame = Frame::Output(vec![0, 1, 2, 3, 4, 5]);
        let mut bytes = Vec::new();
        frame.write(&mut bytes).expect("write frame");
        let decoded = Frame::read(&mut bytes.as_slice()).expect("read frame");
        assert_eq!(decoded, frame);
    }
}
