use tokio::sync::{mpsc, Mutex};

const DEFAULT_BUFFER: usize = 256;

pub struct SessionReader<M> {
    rx: Mutex<mpsc::Receiver<M>>,
}

impl<M> SessionReader<M> {
    pub async fn recv(&self) -> Result<Option<M>, String> {
        Ok(self.rx.lock().await.recv().await)
    }
}

#[derive(Clone)]
pub struct SessionWriter<M> {
    tx: mpsc::Sender<M>,
}

impl<M> SessionWriter<M> {
    pub async fn send(&self, msg: M) -> Result<(), String> {
        self.tx.send(msg).await.map_err(|_| "session closed".to_string())
    }
}

pub struct Session<M> {
    pub reader: SessionReader<M>,
    pub writer: SessionWriter<M>,
}

pub fn memory_session_pair<M>() -> (Session<M>, Session<M>) {
    memory_session_pair_with_buffer(DEFAULT_BUFFER)
}

pub fn memory_session_pair_with_buffer<M>(buffer: usize) -> (Session<M>, Session<M>) {
    let (left_to_right_tx, left_to_right_rx) = mpsc::channel(buffer);
    let (right_to_left_tx, right_to_left_rx) = mpsc::channel(buffer);

    let left = Session { reader: SessionReader { rx: Mutex::new(right_to_left_rx) }, writer: SessionWriter { tx: left_to_right_tx } };
    let right = Session { reader: SessionReader { rx: Mutex::new(left_to_right_rx) }, writer: SessionWriter { tx: right_to_left_tx } };

    (left, right)
}
