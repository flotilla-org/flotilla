use crossterm::event::{EventStream, KeyEventKind};
use futures::{FutureExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum Event {
    Tick,
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
}

pub struct EventHandler {
    rx: mpsc::UnboundedReceiver<Event>,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
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
        });
        Self { rx }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
