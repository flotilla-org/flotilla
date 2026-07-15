use std::{collections::VecDeque, time::Duration};

use crossterm::event::{EventStream, KeyEventKind};
use flotilla_protocol::DaemonEvent;
use futures::{FutureExt, StreamExt};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};

#[derive(Clone, Debug)]
pub enum Event {
    Tick,
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Daemon(Box<DaemonEvent>),
    DaemonDisconnected,
}

pub struct EventHandler {
    tx: mpsc::UnboundedSender<Event>,
    rx: mpsc::UnboundedReceiver<Event>,
    retained: VecDeque<Event>,
    tick_rate: Duration,
    terminal_task: Option<JoinHandle<()>>,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut handler = Self { tx, rx, retained: VecDeque::new(), tick_rate, terminal_task: None };
        handler.resume_terminal_input();
        handler
    }

    fn spawn_terminal_task(&self) -> JoinHandle<()> {
        let tx = self.tx.clone();
        let tick_rate = self.tick_rate;
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
                    _ = delay => { let _ = tx.send(Event::Tick); }
                    maybe = event => match maybe {
                        Some(Ok(crossterm::event::Event::Key(k)))
                            if k.kind == KeyEventKind::Press =>
                        {
                            let _ = tx.send(Event::Key(k));
                        }
                        Some(Ok(crossterm::event::Event::Mouse(m))) => {
                            let _ = tx.send(Event::Mouse(m));
                        }
                        _ => {}
                    }
                }
            }
        })
    }

    /// Stop polling the controlling terminal while a foreground child owns
    /// it. Daemon events remain queued; stale terminal input and ticks do not.
    pub async fn pause_terminal_input(&mut self) {
        if let Some(task) = self.terminal_task.take() {
            task.abort();
            let _ = task.await;
        }
        while let Ok(event) = self.rx.try_recv() {
            if matches!(event, Event::Daemon(_) | Event::DaemonDisconnected) {
                self.retained.push_back(event);
            }
        }
    }

    /// Resume terminal polling after the TUI has re-entered raw mode. The
    /// task's startup drain discards keys left behind by the attached child.
    pub fn resume_terminal_input(&mut self) {
        if self.terminal_task.is_none() {
            self.terminal_task = Some(self.spawn_terminal_task());
        }
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
                    Err(broadcast::error::RecvError::Closed) => {
                        let _ = tx.send(Event::DaemonDisconnected);
                        break;
                    }
                }
            }
        });
    }

    pub async fn next(&mut self) -> Option<Event> {
        match self.retained.pop_front() {
            Some(event) => Some(event),
            None => self.rx.recv().await,
        }
    }

    /// Non-blocking: returns the next queued event if one is available.
    pub fn try_next(&mut self) -> Option<Event> {
        self.retained.pop_front().or_else(|| self.rx.try_recv().ok())
    }
}

impl Drop for EventHandler {
    fn drop(&mut self) {
        if let Some(task) = self.terminal_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn closed_daemon_receiver_is_forwarded_to_event_loop() {
        let mut handler = EventHandler::new(Duration::from_secs(60));
        let (daemon_tx, daemon_rx) = broadcast::channel(4);
        handler.attach_daemon(daemon_rx);

        drop(daemon_tx);

        let event = tokio::time::timeout(Duration::from_secs(1), handler.next()).await.expect("disconnect event");
        assert!(matches!(event, Some(Event::DaemonDisconnected)));
    }
}
