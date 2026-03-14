use std::time::Duration;

use crossterm::event::{EventStream, KeyEventKind};
use flotilla_protocol::DaemonEvent;
use futures::{FutureExt, StreamExt};
use tokio::sync::{broadcast, mpsc};

#[derive(Clone, Debug)]
pub enum Event {
    Tick,
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Daemon(Box<DaemonEvent>),
}

pub struct EventHandler {
    tx: mpsc::UnboundedSender<Event>,
    rx: mpsc::UnboundedReceiver<Event>,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let mut reader = EventStream::new();

            // Drain any stale input (e.g. the Enter key from launching the program)
            // by discarding events that arrive within the first 50ms.
            let drain_until = tokio::time::Instant::now() + Duration::from_millis(50);
            loop {
                let timeout = tokio::time::sleep_until(drain_until);
                let event = reader.next().fuse();
                tokio::select! {
                    _ = timeout => break,
                    _ = event => {} // discard
                }
            }

            let mut interval = tokio::time::interval(tick_rate);
            loop {
                let delay = interval.tick();
                let event = reader.next().fuse();
                tokio::select! {
                    _ = delay => { let _ = tx_clone.send(Event::Tick); }
                    maybe = event => match maybe {
                        Some(Ok(crossterm::event::Event::Key(k)))
                            if k.kind == KeyEventKind::Press =>
                        {
                            let _ = tx_clone.send(Event::Key(k));
                        }
                        Some(Ok(crossterm::event::Event::Mouse(m))) => {
                            let _ = tx_clone.send(Event::Mouse(m));
                        }
                        _ => {}
                    }
                }
            }
        });
        Self { tx, rx }
    }

    /// Forward daemon events into the unified event stream.
    pub fn attach_daemon(&self, mut daemon_rx: broadcast::Receiver<DaemonEvent>) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            loop {
                match daemon_rx.recv().await {
                    Ok(event) => {
                        let _ = tx.send(Event::Daemon(Box::new(event)));
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "daemon event receiver lagged");
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    /// Non-blocking: returns the next queued event if one is available.
    pub fn try_next(&mut self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}
