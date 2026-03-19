use std::{
    io::{Error, ErrorKind, Read, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use crate::vt::{ClientCapabilities, ColorLevel, VtEngineKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub vt_engine: VtEngineKind,
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
const TAG_DETACH: u8 = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    AttachInit { cols: u16, rows: u16, capabilities: ClientCapabilities },
    Input(Vec<u8>),
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Ack,
    Busy,
    Detach,
}

impl Frame {
    pub fn read(reader: &mut impl Read) -> std::io::Result<Self> {
        let mut header = [0u8; 5];
        reader.read_exact(&mut header)?;
        let tag = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        reader.read_exact(&mut payload)?;
        Self::decode(tag, payload)
    }

    pub fn write(&self, writer: &mut impl Write) -> std::io::Result<()> {
        let (tag, payload) = self.encode();
        let mut header = [0u8; 5];
        header[0] = tag;
        header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        writer.write_all(&header)?;
        writer.write_all(&payload)
    }

    fn encode(&self) -> (u8, Vec<u8>) {
        match self {
            Frame::AttachInit { cols, rows, capabilities } => {
                let mut payload = Vec::with_capacity(5);
                payload.extend_from_slice(&cols.to_le_bytes());
                payload.extend_from_slice(&rows.to_le_bytes());
                payload.push(encode_capabilities(*capabilities));
                (TAG_ATTACH_INIT, payload)
            }
            Frame::Resize { cols, rows } => {
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&cols.to_le_bytes());
                payload.extend_from_slice(&rows.to_le_bytes());
                (TAG_RESIZE, payload)
            }
            Frame::Input(bytes) => (TAG_INPUT, bytes.clone()),
            Frame::Output(bytes) => (TAG_OUTPUT, bytes.clone()),
            Frame::Ack => (TAG_ACK, vec![]),
            Frame::Busy => (TAG_BUSY, vec![]),
            Frame::Detach => (TAG_DETACH, vec![]),
        }
    }

    fn decode(tag: u8, payload: Vec<u8>) -> std::io::Result<Self> {
        match tag {
            TAG_ATTACH_INIT => decode_attach_init(payload),
            TAG_RESIZE => decode_size_frame(payload).map(|(cols, rows)| Frame::Resize { cols, rows }),
            TAG_INPUT => Ok(Frame::Input(payload)),
            TAG_OUTPUT => Ok(Frame::Output(payload)),
            TAG_ACK => Ok(Frame::Ack),
            TAG_BUSY => Ok(Frame::Busy),
            TAG_DETACH => Ok(Frame::Detach),
            _ => Err(Error::new(ErrorKind::InvalidData, format!("unknown frame tag {tag}"))),
        }
    }
}

fn decode_size_frame(payload: Vec<u8>) -> std::io::Result<(u16, u16)> {
    if payload.len() != 4 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid size frame"));
    }
    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    Ok((cols, rows))
}

fn decode_attach_init(payload: Vec<u8>) -> std::io::Result<Frame> {
    if payload.len() != 5 {
        return Err(Error::new(ErrorKind::InvalidData, "invalid attach init frame"));
    }

    let cols = u16::from_le_bytes([payload[0], payload[1]]);
    let rows = u16::from_le_bytes([payload[2], payload[3]]);
    let capabilities = decode_capabilities(payload[4])?;

    Ok(Frame::AttachInit { cols, rows, capabilities })
}

fn encode_capabilities(capabilities: ClientCapabilities) -> u8 {
    let color_bits = match capabilities.color_level {
        ColorLevel::Sixteen => 0,
        ColorLevel::Ansi256 => 1,
        ColorLevel::TrueColor => 2,
    };
    let kitty_keyboard_bit = if capabilities.kitty_keyboard { 1 << 2 } else { 0 };
    color_bits | kitty_keyboard_bit
}

fn decode_capabilities(byte: u8) -> std::io::Result<ClientCapabilities> {
    let color_level = match byte & 0b11 {
        0 => ColorLevel::Sixteen,
        1 => ColorLevel::Ansi256,
        2 => ColorLevel::TrueColor,
        _ => {
            return Err(Error::new(ErrorKind::InvalidData, format!("invalid attach capability color level {byte:#010b}")));
        }
    };
    let kitty_keyboard = (byte & (1 << 2)) != 0;
    Ok(ClientCapabilities::new(color_level, kitty_keyboard))
}

#[cfg(test)]
mod tests {
    use super::Frame;
    use crate::vt::{ClientCapabilities, ColorLevel};

    #[test]
    fn attach_init_round_trip_preserves_capability_profile() {
        let frame = Frame::AttachInit { cols: 120, rows: 40, capabilities: ClientCapabilities::new(ColorLevel::Ansi256, true) };
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
