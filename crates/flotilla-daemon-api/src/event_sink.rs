//! Event publication port for in-process services.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};

use flotilla_protocol::DaemonEvent;
use tokio::sync::{broadcast, mpsc, oneshot};

/// Publishes synchronously. Sequential emissions to the same sink preserve their call order.
/// The broadcast adapter discards delivery failures.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: DaemonEvent);
}

/// Keeps the daemon's existing broadcast delivery and receiver semantics.
pub struct BroadcastEventSink {
    sender: broadcast::Sender<DaemonEvent>,
    watches: Mutex<Vec<mpsc::UnboundedSender<DaemonEvent>>>,
    captures: Arc<Mutex<Captures>>,
}

/// The completions open captures may still ask for, held once however many captures are
/// open, and the callers waiting on one command's completion.
#[derive(Default)]
struct Captures {
    next_capture: u64,
    next_sequence: u64,
    /// Each open capture's first sequence: completions from it on are kept for it.
    open: BTreeMap<u64, u64>,
    /// Completions emitted while a capture is open, by sequence and command id.
    kept: VecDeque<(u64, u64, DaemonEvent)>,
    waiters: HashMap<u64, Vec<oneshot::Sender<DaemonEvent>>>,
}

impl Captures {
    fn close(&mut self, capture: u64) {
        self.open.remove(&capture);
        match self.open.values().min() {
            Some(&oldest) => {
                while self.kept.front().is_some_and(|(sequence, _, _)| *sequence < oldest) {
                    self.kept.pop_front();
                }
            }
            None => self.kept.clear(),
        }
    }
}

impl BroadcastEventSink {
    pub fn new(sender: broadcast::Sender<DaemonEvent>) -> Self {
        Self { sender, watches: Mutex::new(Vec::new()), captures: Arc::default() }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.sender.subscribe()
    }

    /// Every `CommandFinished` emitted while the watch lives, beside the broadcast, for a
    /// consumer that routes every command's outcome as it arrives. A caller waiting on one
    /// command uses [`Self::capture_completion`] instead.
    pub fn watch_completions(&self) -> CommandCompletions {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut watches = self.watches.lock().expect("completion watches lock");
        watches.retain(|watch| !watch.is_closed());
        watches.push(sender);
        CommandCompletions { receiver }
    }

    /// One command's completion, captured from now on: a caller that captures before it
    /// dispatches receives its command's outcome however far a broadcast receiver lags, and
    /// is sent only that outcome. Completions emitted before the command id is known are
    /// held once for all open captures, until the oldest closes.
    pub fn capture_completion(&self) -> CompletionCapture {
        let mut captures = self.captures.lock().expect("completion captures lock");
        let capture = captures.next_capture;
        captures.next_capture += 1;
        let first = captures.next_sequence;
        captures.open.insert(capture, first);
        CompletionCapture { captures: Arc::downgrade(&self.captures), capture, first }
    }
}

impl EventSink for BroadcastEventSink {
    fn emit(&self, event: DaemonEvent) {
        if let DaemonEvent::CommandFinished { command_id, .. } = &event {
            let command_id = *command_id;
            {
                let mut captures = self.captures.lock().expect("completion captures lock");
                let sequence = captures.next_sequence;
                captures.next_sequence += 1;
                if !captures.open.is_empty() {
                    captures.kept.push_back((sequence, command_id, event.clone()));
                }
                for waiter in captures.waiters.remove(&command_id).into_iter().flatten() {
                    let _ = waiter.send(event.clone());
                }
            }
            let mut watches = self.watches.lock().expect("completion watches lock");
            watches.retain(|watch| watch.send(event.clone()).is_ok());
        }
        let _ = self.sender.send(event);
    }
}

/// One command's completion from a [`BroadcastEventSink`], captured before its id was known.
/// Dropping it ends the capture.
pub struct CompletionCapture {
    captures: Weak<Mutex<Captures>>,
    capture: u64,
    first: u64,
}

impl CompletionCapture {
    /// The `CommandFinished` of `command_id`, or `None` once the sink is gone. The capture
    /// ends as this is first polled: from then on only this command's outcome is kept for it.
    pub async fn finished(self, command_id: u64) -> Option<DaemonEvent> {
        let waiting = {
            let captures = self.captures.upgrade()?;
            let mut captures = captures.lock().expect("completion captures lock");
            let kept = captures
                .kept
                .iter()
                .find(|(sequence, finished, _)| *sequence >= self.first && *finished == command_id)
                .map(|(_, _, event)| event.clone());
            if let Some(event) = kept {
                return Some(event);
            }
            let (sender, receiver) = oneshot::channel();
            let waiters = captures.waiters.entry(command_id).or_default();
            waiters.retain(|waiter| !waiter.is_closed());
            waiters.push(sender);
            receiver
        };
        drop(self);
        waiting.await.ok()
    }
}

impl Drop for CompletionCapture {
    fn drop(&mut self) {
        if let Some(captures) = self.captures.upgrade() {
            captures.lock().expect("completion captures lock").close(self.capture);
        }
    }
}

/// Command completions watched on a [`BroadcastEventSink`]; dropping it ends the watch.
pub struct CommandCompletions {
    receiver: mpsc::UnboundedReceiver<DaemonEvent>,
}

impl CommandCompletions {
    /// The next watched `CommandFinished`, of any command, or `None` once the sink is gone.
    pub async fn next(&mut self) -> Option<DaemonEvent> {
        self.receiver.recv().await
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{CommandValue, NodeId, RepoIdentity};

    use super::*;

    fn finished(id: u64) -> DaemonEvent {
        DaemonEvent::CommandFinished {
            command_id: id,
            node_id: NodeId::new("local"),
            repo_identity: RepoIdentity { authority: "local".into(), path: String::new() },
            repo: None,
            result: CommandValue::Ok,
        }
    }

    // A capture is sent its own completion and holds no other: completions are kept once,
    // only while a capture whose command id is not yet known is open.
    #[tokio::test]
    async fn a_capture_holds_only_its_own_completion() {
        let sink = BroadcastEventSink::new(broadcast::channel(4).0);
        let early = sink.capture_completion();
        let waiting = sink.capture_completion();
        sink.emit(finished(1));
        sink.emit(finished(2));
        assert_eq!(sink.captures.lock().unwrap().kept.len(), 2, "kept once for both open captures");
        assert!(matches!(early.finished(1).await, Some(DaemonEvent::CommandFinished { command_id: 1, .. })));
        let pending = tokio::spawn(waiting.finished(7));
        tokio::task::yield_now().await;
        assert!(sink.captures.lock().unwrap().kept.is_empty(), "no open capture holds anything");
        for id in 3..100 {
            sink.emit(finished(id));
        }
        assert!(sink.captures.lock().unwrap().kept.is_empty(), "a waiting capture is sent only its own outcome");
        assert!(matches!(pending.await.unwrap(), Some(DaemonEvent::CommandFinished { command_id: 7, .. })));
        assert!(sink.captures.lock().unwrap().waiters.is_empty());
    }

    #[tokio::test]
    async fn a_capture_ends_with_its_sink() {
        let sink = BroadcastEventSink::new(broadcast::channel(4).0);
        let capture = sink.capture_completion();
        let pending = tokio::spawn(capture.finished(3));
        tokio::task::yield_now().await;
        drop(sink);
        assert!(pending.await.unwrap().is_none());
    }
}
