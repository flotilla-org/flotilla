//! Event publication port for in-process services.

use flotilla_protocol::DaemonEvent;
use tokio::sync::broadcast;

/// Publishes synchronously. Sequential emissions to the same sink preserve their call order.
/// The broadcast adapter discards delivery failures.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: DaemonEvent);
}

/// Keeps the daemon's existing broadcast delivery and receiver semantics.
pub struct BroadcastEventSink {
    sender: broadcast::Sender<DaemonEvent>,
}

impl BroadcastEventSink {
    pub fn new(sender: broadcast::Sender<DaemonEvent>) -> Self {
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.sender.subscribe()
    }
}

impl EventSink for BroadcastEventSink {
    fn emit(&self, event: DaemonEvent) {
        let _ = self.sender.send(event);
    }
}
