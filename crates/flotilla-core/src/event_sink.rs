//! Event publication port for in-process services.

use std::sync::Mutex;

use flotilla_protocol::DaemonEvent;
use tokio::sync::broadcast;

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
}

impl EventSink for BroadcastEventSink {
    fn emit(&self, event: DaemonEvent) {
        let _ = self.sender.send(event);
    }
}

// Unit tests keep their existing broadcast subscriptions while injecting the
// publishing port into leaf and step execution.
#[cfg(test)]
impl EventSink for broadcast::Sender<DaemonEvent> {
    fn emit(&self, event: DaemonEvent) {
        let _ = self.send(event);
    }
}

/// A test sink that retains events in emission order without requiring subscribers.
#[derive(Default)]
pub struct RecordingEventSink {
    events: Mutex<Vec<DaemonEvent>>,
}

impl RecordingEventSink {
    pub fn events(&self) -> Vec<DaemonEvent> {
        self.events.lock().expect("recording event sink lock poisoned").clone()
    }
}

impl EventSink for RecordingEventSink {
    fn emit(&self, event: DaemonEvent) {
        self.events.lock().expect("recording event sink lock poisoned").push(event);
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::RepoIdentity;

    use super::*;

    fn event(path: &str) -> DaemonEvent {
        DaemonEvent::RepoUntracked { repo_identity: RepoIdentity { authority: "local".into(), path: path.into() }, path: None }
    }

    #[test]
    fn recording_sink_preserves_emission_order() {
        let sink = RecordingEventSink::default();
        sink.emit(event("first"));
        sink.emit(event("second"));

        let paths = sink
            .events()
            .into_iter()
            .map(|event| match event {
                DaemonEvent::RepoUntracked { repo_identity, .. } => repo_identity.path,
                _ => panic!("unexpected event"),
            })
            .collect::<Vec<_>>();
        assert_eq!(paths, ["first", "second"]);
    }

    #[tokio::test]
    async fn broadcast_sink_retains_existing_delivery() {
        let (sender, mut receiver) = broadcast::channel(1);
        let sink = BroadcastEventSink::new(sender);
        sink.emit(event("repo"));

        assert!(matches!(receiver.recv().await.expect("broadcast event"), DaemonEvent::RepoUntracked { .. }));
    }
}
