use flotilla_protocol::Message;

use crate::memory::{memory_session_pair, Session};

pub struct MessageSession {
    inner: Session<Message>,
}

impl MessageSession {
    pub async fn read(&self) -> Result<Option<Message>, String> {
        self.inner.reader.recv().await
    }

    pub async fn write(&self, msg: Message) -> Result<(), String> {
        self.inner.writer.send(msg).await
    }
}

pub fn message_session_pair() -> (MessageSession, MessageSession) {
    let (left, right) = memory_session_pair();
    (MessageSession { inner: left }, MessageSession { inner: right })
}
