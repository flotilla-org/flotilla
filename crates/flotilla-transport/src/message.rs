use std::path::Path;

use flotilla_protocol::{framing::write_message_line, Message};
use tokio::{
    io::{AsyncBufReadExt, BufReader, BufWriter},
    net::UnixStream,
    sync::Mutex,
};

use crate::memory::{memory_session_pair, Session};

enum MessageSessionInner {
    Memory(Session<Message>),
    Unix {
        reader: Mutex<tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>>,
        writer: Mutex<BufWriter<tokio::net::unix::OwnedWriteHalf>>,
    },
}

pub struct MessageSession {
    inner: MessageSessionInner,
}

impl MessageSession {
    pub async fn read(&self) -> Result<Option<Message>, String> {
        match &self.inner {
            MessageSessionInner::Memory(session) => session.reader.recv().await,
            MessageSessionInner::Unix { reader, .. } => match reader.lock().await.next_line().await {
                Ok(Some(line)) => serde_json::from_str(&line).map(Some).map_err(|e| format!("failed to parse message: {e}")),
                Ok(None) => Ok(None),
                Err(e) => Err(format!("failed to read message: {e}")),
            },
        }
    }

    pub async fn write(&self, msg: Message) -> Result<(), String> {
        match &self.inner {
            MessageSessionInner::Memory(session) => session.writer.send(msg).await,
            MessageSessionInner::Unix { writer, .. } => {
                let mut writer = writer.lock().await;
                write_message_line(&mut *writer, &msg).await
            }
        }
    }
}

pub async fn connect_unix_message_session(socket_path: &Path) -> Result<MessageSession, String> {
    let stream = UnixStream::connect(socket_path).await.map_err(|e| format!("failed to connect to {}: {e}", socket_path.display()))?;
    Ok(unix_message_session(stream))
}

pub fn unix_message_session(stream: UnixStream) -> MessageSession {
    let (read_half, write_half) = stream.into_split();
    MessageSession {
        inner: MessageSessionInner::Unix {
            reader: Mutex::new(BufReader::new(read_half).lines()),
            writer: Mutex::new(BufWriter::new(write_half)),
        },
    }
}

pub fn message_session_pair() -> (MessageSession, MessageSession) {
    let (left, right) = memory_session_pair();
    (MessageSession { inner: MessageSessionInner::Memory(left) }, MessageSession { inner: MessageSessionInner::Memory(right) })
}
