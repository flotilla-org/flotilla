//! Bounded restarts for the daemon's namespace-following resource watches.

use flotilla_resources::{LOCAL_RETRY_BACKOFF, LOCAL_RETRY_LIMIT};
use tokio::sync::watch;
use tracing::error;

/// Failures of one watch in a row. Each restart waits [`LOCAL_RETRY_BACKOFF`] or the next
/// namespace change, whichever is first; past [`LOCAL_RETRY_LIMIT`] only the namespace change
/// restarts it.
#[derive(Default)]
pub(super) struct Resubscribe {
    failures: u32,
}

impl Resubscribe {
    /// The watch delivered an event, so it works again.
    pub(super) fn delivered(&mut self) {
        self.failures = 0;
    }

    /// Waits before the next subscription after a failed or ended watch. False when the
    /// namespace changes can no longer arrive, so the follower should stop.
    pub(super) async fn after_failure(&mut self, namespace_changes: &mut watch::Receiver<()>, watch: &'static str) -> bool {
        self.failures += 1;
        if self.failures <= LOCAL_RETRY_LIMIT {
            tokio::select! {
                changed = namespace_changes.changed() => changed.is_ok(),
                () = tokio::time::sleep(LOCAL_RETRY_BACKOFF.delay(self.failures)) => true,
            }
        } else {
            error!(failures = self.failures, watch, "watch keeps failing; resubscribing at the next namespace change");
            namespace_changes.changed().await.is_ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn restarts_back_off_then_wait_for_a_namespace_change() {
        let (changes, mut namespace_changes) = watch::channel(());
        let mut restarts = Resubscribe::default();
        for failure in 1..=LOCAL_RETRY_LIMIT {
            let started = tokio::time::Instant::now();
            assert!(restarts.after_failure(&mut namespace_changes, "test").await);
            assert_eq!(started.elapsed(), LOCAL_RETRY_BACKOFF.delay(failure));
        }
        // Past the limit no time restarts it; the next namespace change does.
        let mut waiting = Box::pin(restarts.after_failure(&mut namespace_changes, "test"));
        assert!(tokio::time::timeout(Duration::from_secs(24 * 60 * 60), &mut waiting).await.is_err());
        changes.send_replace(());
        assert!(waiting.await);
        drop(changes);
        assert!(!restarts.after_failure(&mut namespace_changes, "test").await, "no namespace changes can come");
    }

    #[tokio::test(start_paused = true)]
    async fn a_delivered_event_resets_the_backoff() {
        let (_changes, mut namespace_changes) = watch::channel(());
        let mut restarts = Resubscribe::default();
        for _ in 0..3 {
            restarts.after_failure(&mut namespace_changes, "test").await;
        }
        restarts.delivered();
        let started = tokio::time::Instant::now();
        restarts.after_failure(&mut namespace_changes, "test").await;
        assert_eq!(started.elapsed(), LOCAL_RETRY_BACKOFF.delay(1));
    }
}
