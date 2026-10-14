//! Stream framing: an 8-byte protocol magic, a big-endian u32 body length,
//! then one message body. Big-endian matches Jackstay's existing length
//! prefixes and bootstrap fields, the first adopter.
use std::io::{Read, Write};

use crate::{decode_message, encode_message, Error, Limits, Message};

pub const MAGIC_LEN: usize = 8;
/// Magic plus length prefix.
pub const HEADER_LEN: usize = MAGIC_LEN + 4;

/// One protocol's framing: its magic and its limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Framing {
    pub magic: [u8; MAGIC_LEN],
    pub limits: Limits,
}

impl Framing {
    pub const fn new(magic: [u8; MAGIC_LEN], limits: Limits) -> Self {
        Self { magic, limits }
    }

    /// The body length a header announces, checked against the message limit,
    /// so a caller can bound its allocation before reading the body.
    pub fn body_len(&self, header: &[u8; HEADER_LEN]) -> Result<usize, Error> {
        if header[..MAGIC_LEN] != self.magic {
            return Err(Error::BadMagic);
        }
        let len = u32::from_be_bytes(header[MAGIC_LEN..].try_into().unwrap());
        if len as u64 > self.limits.max_message as u64 {
            return Err(Error::MessageLimit { len: len.into(), limit: self.limits.max_message });
        }
        Ok(len as usize)
    }

    /// One complete frame.
    pub fn encode<M: Message>(&self, message: &M) -> Result<Vec<u8>, Error> {
        let body = encode_message(message, &self.limits)?;
        let len = u32::try_from(body.len()).map_err(|_| Error::MessageLimit { len: body.len() as u64, limit: self.limits.max_message })?;
        let mut frame = Vec::with_capacity(HEADER_LEN + body.len());
        frame.extend_from_slice(&self.magic);
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// Exactly one complete frame: nothing missing and nothing after it.
    pub fn decode<M: Message>(&self, frame: &[u8]) -> Result<M, Error> {
        let header: &[u8; HEADER_LEN] = frame.get(..HEADER_LEN).ok_or(Error::Truncated)?.try_into().unwrap();
        let len = self.body_len(header)?;
        let body = &frame[HEADER_LEN..];
        if body.len() < len {
            return Err(Error::Truncated);
        }
        if body.len() > len {
            return Err(Error::TrailingBytes);
        }
        decode_message(body, &self.limits)
    }

    /// Read one frame, allocating its body only after the length check.
    pub fn read<M: Message, R: Read + ?Sized>(&self, reader: &mut R) -> Result<M, Error> {
        let mut header = [0; HEADER_LEN];
        reader.read_exact(&mut header)?;
        let mut body = vec![0; self.body_len(&header)?];
        reader.read_exact(&mut body)?;
        decode_message(&body, &self.limits)
    }

    /// Write one frame.
    pub fn write<M: Message, W: Write + ?Sized>(&self, writer: &mut W, message: &M) -> Result<(), Error> {
        writer.write_all(&self.encode(message)?)?;
        Ok(())
    }
}
