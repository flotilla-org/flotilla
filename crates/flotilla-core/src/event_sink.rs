//! Event publication port for in-process services.

#[cfg(test)]
use std::sync::{Arc, Mutex};

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

/// Broadcast adapter for tests that exercise subscriptions or asynchronous delivery.
#[cfg(test)]
pub(crate) fn broadcast_test_sink(sender: broadcast::Sender<DaemonEvent>) -> Arc<dyn EventSink> {
    Arc::new(BroadcastEventSink::new(sender))
}

/// Retains events in this sink's emission order without requiring subscribers.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingEventSink {
    events: Mutex<Vec<DaemonEvent>>,
}

#[cfg(test)]
impl RecordingEventSink {
    pub fn events(&self) -> Vec<DaemonEvent> {
        self.events.lock().expect("recording event sink lock poisoned").clone()
    }
}

#[cfg(test)]
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

    // Behaviour (#2255): publication preserves order and duplicates, including
    // empty batches and batches crossing the daemon channel's 256-event capacity.
    #[hegel::test]
    fn recording_sink_preserves_generated_sequences(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(257));
        // Small values deliberately produce repeated events.
        let paths: Vec<_> = (0..count).map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(3)).to_string()).collect();
        let sink = RecordingEventSink::default();
        for path in &paths {
            sink.emit(event(path));
        }
        let observed: Vec<_> = sink
            .events()
            .into_iter()
            .map(|event| match event {
                DaemonEvent::RepoUntracked { repo_identity, .. } => repo_identity.path,
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(observed, paths);
    }

    // Behaviour (#2255): broadcast errors remain discarded, late subscribers
    // receive no replay, and lagging receivers keep Tokio's original lag semantics.
    #[test]
    fn broadcast_sink_preserves_absent_and_lagging_subscriber_semantics() {
        let (sender, receiver) = broadcast::channel(2);
        drop(receiver);
        let sink = BroadcastEventSink::new(sender);
        sink.emit(event("no subscribers"));
        let mut slow = sink.subscribe();
        let mut fast = sink.subscribe();
        assert!(matches!(slow.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        for path in ["first", "second", "third"] {
            sink.emit(event(path));
            assert!(
                matches!(fast.try_recv().expect("live event"), DaemonEvent::RepoUntracked { repo_identity, .. } if repo_identity.path == path)
            );
        }
        assert!(matches!(slow.try_recv(), Err(broadcast::error::TryRecvError::Lagged(1))));
        for path in ["second", "third"] {
            assert!(
                matches!(slow.try_recv().expect("retained event"), DaemonEvent::RepoUntracked { repo_identity, .. } if repo_identity.path == path)
            );
        }
        drop(slow);
        drop(fast);
        sink.emit(event("no subscribers again"));
        assert!(matches!(sink.subscribe().try_recv(), Err(broadcast::error::TryRecvError::Empty)));
    }
}
