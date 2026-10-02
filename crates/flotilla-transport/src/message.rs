#[cfg(unix)]
use std::path::Path;
use std::{io::Cursor, pin::Pin};

use flotilla_protocol::{framing::write_message_line, Message};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, BufReader, BufWriter},
    sync::Mutex,
};

use crate::memory::{memory_session_pair, Session};

type StreamReader = tokio::io::Lines<BufReader<tokio::io::Chain<Cursor<Vec<u8>>, Pin<Box<dyn AsyncRead + Send>>>>>;

enum MessageSessionInner {
    Memory(Session<Message>),
    Stream { reader: Box<Mutex<StreamReader>>, writer: Mutex<BufWriter<Pin<Box<dyn AsyncWrite + Send>>>> },
}

pub struct MessageSession {
    inner: MessageSessionInner,
}

impl MessageSession {
    pub async fn read(&self) -> Result<Option<Message>, String> {
        match &self.inner {
            MessageSessionInner::Memory(session) => session.reader.recv().await,
            MessageSessionInner::Stream { reader, .. } => match reader.lock().await.next_line().await {
                // Parse failures are treated as fatal protocol errors so higher layers can
                // tear down the session instead of continuing on a desynchronized stream.
                Ok(Some(line)) => serde_json::from_str(&line).map(Some).map_err(|e| format!("failed to parse message: {e}")),
                Ok(None) => Ok(None),
                Err(e) => Err(format!("failed to read message: {e}")),
            },
        }
    }

    pub async fn write(&self, msg: Message) -> Result<(), String> {
        match &self.inner {
            MessageSessionInner::Memory(session) => session.writer.send(msg).await,
            MessageSessionInner::Stream { writer, .. } => {
                let mut writer = writer.lock().await;
                write_message_line(&mut *writer, &msg).await
            }
        }
    }
}

#[cfg(unix)]
pub async fn connect_unix_message_session(socket_path: &Path) -> Result<MessageSession, String> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| format!("failed to connect to {}: {e}", socket_path.display()))?;
    Ok(unix_message_session(stream))
}

#[cfg(unix)]
pub fn unix_message_session(stream: UnixStream) -> MessageSession {
    unix_message_session_with_prefix(stream, Vec::new())
}

#[cfg(unix)]
pub fn unix_message_session_with_prefix(stream: UnixStream, prefix: Vec<u8>) -> MessageSession {
    let (read_half, write_half) = stream.into_split();
    stream_message_session_with_prefix(read_half, write_half, prefix)
}

/// Construct a newline-delimited JSON session over independently owned streams.
pub fn stream_message_session<R, W>(reader: R, writer: W) -> MessageSession
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    stream_message_session_with_prefix(reader, writer, Vec::new())
}

/// Replay bytes consumed during protocol detection before reading the stream.
pub fn stream_message_session_with_prefix<R, W>(reader: R, writer: W, prefix: Vec<u8>) -> MessageSession
where
    R: AsyncRead + Send + 'static,
    W: AsyncWrite + Send + 'static,
{
    let reader: Pin<Box<dyn AsyncRead + Send>> = Box::pin(reader);
    let writer: Pin<Box<dyn AsyncWrite + Send>> = Box::pin(writer);
    MessageSession {
        inner: MessageSessionInner::Stream {
            reader: Box::new(Mutex::new(BufReader::new(Cursor::new(prefix).chain(reader)).lines())),
            writer: Mutex::new(BufWriter::new(writer)),
        },
    }
}

pub fn message_session_pair() -> (MessageSession, MessageSession) {
    let (left, right) = memory_session_pair();
    (MessageSession { inner: MessageSessionInner::Memory(left) }, MessageSession { inner: MessageSessionInner::Memory(right) })
}
