//! Slow tracker observations outlive interactive requests. One refresh per source
//! serves all Projects, retaining the last successful snapshot during outages.
use std::{collections::BTreeMap, future::Future, sync::Arc, time::Duration};

use chrono::Utc;
use flotilla_protocol::{DispatchBoardRepository, IssueSource};
use tokio::{sync::Mutex, time::Instant};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct Entry {
    board: Option<DispatchBoardRepository>,
    last_attempt: Option<Instant>,
    refreshing: bool,
    error: Option<String>,
}

#[derive(Clone, Default)]
pub(super) struct DispatchBoardCache(Arc<Mutex<BTreeMap<IssueSource, Entry>>>);

impl DispatchBoardCache {
    pub(super) async fn read<F, Fut>(&self, source: &IssueSource, load: F) -> Result<DispatchBoardRepository, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<DispatchBoardRepository, String>> + Send,
    {
        let mut entries = self.0.lock().await;
        let entry = entries.entry(source.clone()).or_default();
        if !entry.refreshing && entry.last_attempt.is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL) {
            entry.refreshing = true;
            entry.last_attempt = Some(Instant::now());
            let cache = self.clone();
            let source = source.clone();
            tokio::spawn(async move {
                let result =
                    tokio::time::timeout(REFRESH_TIMEOUT, load()).await.unwrap_or_else(|_| Err("board tracker refresh timed out".into()));
                let mut entries = cache.0.lock().await;
                let entry = entries.get_mut(&source).expect("refresh entry");
                entry.refreshing = false;
                // Retry backoff starts on completion, including a slow failure.
                entry.last_attempt = Some(Instant::now());
                match result {
                    Ok(mut board) => {
                        board.observed_at = Utc::now();
                        entry.board = Some(board);
                        entry.error = None;
                    }
                    Err(error) => entry.error = Some(error),
                }
            });
        }
        let Some(mut board) = entry.board.clone() else {
            return Err(format!(
                "board facts unavailable for {}: {}",
                source.scope,
                entry.error.as_deref().unwrap_or("initial observation in progress; retry shortly")
            ));
        };
        board.age_seconds = Utc::now().signed_duration_since(board.observed_at).num_seconds().max(0) as u64;
        board.refresh_error = entry.error.clone();
        Ok(board)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_protocol::{DispatchBoardIssue, DispatchBoardPullRequest, IssueState};

    use super::*;

    fn source(scope: &str) -> IssueSource {
        IssueSource { service: "https://github.com".into(), scope: scope.into() }
    }

    pub(in crate::in_process) fn board(source: &IssueSource, count: usize) -> DispatchBoardRepository {
        DispatchBoardRepository {
            source: source.clone(),
            observed_at: Utc::now(),
            age_seconds: 0,
            refresh_error: None,
            issues: (0..count)
                .map(|id| {
                    DispatchBoardIssue::builder()
                        .id(id.to_string())
                        .title(format!("Issue {id}"))
                        .state(IssueState::Open)
                        .url(format!("https://github.com/{}/issues/{id}", source.scope))
                        .updated_at(Utc::now().to_rfc3339())
                        .labels(vec!["ready".into()])
                        .blocked_by(vec![])
                        .pull_requests(vec![])
                        .build()
                })
                .collect(),
            pull_requests: (0..count)
                .map(|id| {
                    DispatchBoardPullRequest::builder()
                        .id(id.to_string())
                        .url(format!("https://github.com/{}/pull/{id}", source.scope))
                        .state("open".into())
                        .ci("success".into())
                        .build()
                })
                .collect(),
        }
    }

    // #2842: hundreds of issues must not multiply tracker work. A slow forge
    // boundary stays outside the interactive read; simultaneous Projects share it.
    #[tokio::test(start_paused = true)]
    async fn large_board_coalesces_slow_tracker_observations() {
        let cache = DispatchBoardCache::default();
        let source = source("org/large");
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..400 {
            let snapshot = board(&source, 400);
            let calls = calls.clone();
            assert!(cache
                .read(&source, move || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(20)).await;
                    Ok(snapshot)
                })
                .await
                .expect_err("cold observation")
                .contains("in progress"));
        }
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(20)).await;
        tokio::task::yield_now().await;
        for _ in 0..400 {
            let snapshot = cache.read(&source, || async { panic!("cached reads must not call forge") }).await.expect("cached board");
            assert_eq!(snapshot.issues.len(), 400);
            assert!(snapshot.refresh_error.is_none());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    // #2842: stale facts carry age and refresh failures; errors neither replace
    // successful evidence nor trigger an unbounded retry storm.
    #[tokio::test(start_paused = true)]
    async fn refresh_failure_preserves_facts_and_source_isolation() {
        let cache = DispatchBoardCache::default();
        let source = source("org/small");
        let snapshot = board(&source, 0);
        assert!(cache.read(&source, move || async { Ok(snapshot) }).await.is_err());
        tokio::task::yield_now().await;
        // Make the observation old without using real-time sleeps.
        cache.0.lock().await.get_mut(&source).expect("entry").board.as_mut().expect("board").observed_at -= chrono::Duration::seconds(120);
        tokio::time::advance(REFRESH_INTERVAL).await;
        let cached = cache.read(&source, || async { Err("forge offline".into()) }).await.expect("stale board");
        assert!(cached.age_seconds >= 120);
        tokio::task::yield_now().await;
        let cached = cache.read(&source, || async { panic!("failure backoff") }).await.expect("last successful board");
        assert_eq!(cached.refresh_error.as_deref(), Some("forge offline"));
        assert!(cached.issues.is_empty());
        tokio::time::advance(REFRESH_INTERVAL).await;
        let recovered = board(&source, 1);
        cache.read(&source, move || async { Ok(recovered) }).await.expect("old facts during recovery");
        tokio::task::yield_now().await;
        let cached = cache.read(&source, || async { panic!("fresh observation") }).await.expect("recovered");
        assert_eq!(cached.issues.len(), 1);
        assert!(cached.refresh_error.is_none());
        assert!(cache
            .read(&IssueSource { service: "https://github.com".into(), scope: "org/other".into() }, || async {
                Err("other offline".into())
            })
            .await
            .is_err());
        tokio::task::yield_now().await;
        assert_eq!(
            cache.read(&source, || async { panic!("source isolation") }).await.expect("original source").refresh_error,
            cached.refresh_error
        );
    }
    // Tracker snapshots preserve every issue and remain separated by source,
    // including empty boards and hundreds of issues across overlapping IDs.
    #[hegel::test]
    fn generated_board_observations_preserve_source_facts(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Covers empty and realistically large boards, shared IDs, and multiple
        // sources. Refresh/error interleavings are covered by the scenarios above.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(500));
        let sources = tc.draw(gs::integers::<usize>().min_value(1).max_value(3));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let cache = DispatchBoardCache::default();
            for id in 0..sources {
                let source = source(&format!("org/repo-{id}"));
                let expected = board(&source, count + id);
                let observation = expected.clone();
                assert!(cache.read(&source, move || async { Ok(observation) }).await.is_err());
                tokio::task::yield_now().await;
                let actual = cache.read(&source, || async { panic!("fresh source") }).await.expect("snapshot");
                assert_eq!(actual.source, expected.source);
                assert_eq!(actual.issues, expected.issues);
                assert_eq!(actual.pull_requests, expected.pull_requests);
            }
        });
    }
}
