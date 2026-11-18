//! Event publication port for in-process services.

use std::sync::Mutex;

use flotilla_protocol::DaemonEvent;
use tokio::sync::{broadcast, mpsc};

/// Publishes synchronously. Sequential emissions to the same sink preserve their call order.
/// The broadcast adapter discards delivery failures.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: DaemonEvent);
}

/// Keeps the daemon's existing broadcast delivery and receiver semantics.
pub struct BroadcastEventSink {
    sender: broadcast::Sender<DaemonEvent>,
    completions: Mutex<Vec<mpsc::UnboundedSender<DaemonEvent>>>,
}

impl BroadcastEventSink {
    pub fn new(sender: broadcast::Sender<DaemonEvent>) -> Self {
        Self { sender, completions: Mutex::new(Vec::new()) }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.sender.subscribe()
    }

    /// Every `CommandFinished` emitted while the capture lives, beside the broadcast.
    /// Every command ends through this sink, so a caller that captures before it
    /// dispatches receives its own outcome however far a broadcast receiver lags.
    pub fn capture_completions(&self) -> CommandCompletions {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut completions = self.completions.lock().expect("completion captures lock");
        completions.retain(|capture| !capture.is_closed());
        completions.push(sender);
        CommandCompletions { receiver }
    }
}

impl EventSink for BroadcastEventSink {
    fn emit(&self, event: DaemonEvent) {
        if matches!(event, DaemonEvent::CommandFinished { .. }) {
            let mut completions = self.completions.lock().expect("completion captures lock");
            completions.retain(|capture| capture.send(event.clone()).is_ok());
        }
        let _ = self.sender.send(event);
    }
}

/// Command completions captured from a [`BroadcastEventSink`]; dropping it ends the capture.
pub struct CommandCompletions {
    receiver: mpsc::UnboundedReceiver<DaemonEvent>,
}

impl CommandCompletions {
    /// The `CommandFinished` of `command_id`, or `None` once the sink is gone.
    pub async fn finished(&mut self, command_id: u64) -> Option<DaemonEvent> {
        while let Some(event) = self.receiver.recv().await {
            if matches!(event, DaemonEvent::CommandFinished { command_id: finished, .. } if finished == command_id) {
                return Some(event);
            }
        }
        None
    }
}
