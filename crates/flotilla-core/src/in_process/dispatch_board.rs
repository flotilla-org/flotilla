//! Slow tracker observations outlive interactive requests. One refresh per source
//! serves all Projects, retaining the last successful snapshot during outages.
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

#[cfg(test)]
use chrono::Utc;
use flotilla_protocol::{DispatchBoardRepository, IssueSource};
use futures::FutureExt;
use tokio::{sync::Mutex, time::Instant};

// Process-local identities: never persist or compare revisions across daemons.
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
// ForgeReads owns the 300s load deadline and publishes its timeout/last-good
// result. This outer watchdog must allow that completion, rather than racing
// the inner deadline and discarding its result. It also bounds bare providers.
const REFRESH_TIMEOUT: Duration = crate::forge_observation::LOAD_TIMEOUT.saturating_add(Duration::from_secs(30));

#[derive(Default)]
struct Entry {
    board: Option<Arc<DispatchBoardRepository>>,
    revision: u64,
    last_attempt: Option<Instant>,
    refreshing: bool,
    error: Option<String>,
}

type SharedEntry = Arc<Mutex<Entry>>;

#[derive(Clone, Default)]
pub(super) struct DispatchBoardCache {
    entries: Arc<Mutex<BTreeMap<IssueSource, SharedEntry>>>,
    #[cfg(test)]
    pub(in crate::in_process) reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl DispatchBoardCache {
    /// Only a complete source inventory may retire entries. An old refresh owns
    /// its retired entry, so its completion cannot overwrite a re-added source.
    pub(super) async fn retain_sources(&self, sources: &BTreeSet<IssueSource>) {
        self.entries.lock().await.retain(|source, _| sources.contains(source));
    }

    #[cfg(test)]
    pub(super) async fn read<F, Fut>(&self, source: &IssueSource, load: F) -> Result<DispatchBoardRepository, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<DispatchBoardRepository, String>> + Send,
    {
        self.read_snapshot(source, load).await.map(|(_, board)| {
            let mut board = (*board).clone();
            board.age_seconds = Utc::now().signed_duration_since(board.observed_at).num_seconds().max(0) as u64;
            board
        })
    }

    pub(super) async fn read_snapshot<F, Fut>(&self, source: &IssueSource, load: F) -> Result<(u64, Arc<DispatchBoardRepository>), String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<DispatchBoardRepository, String>> + Send,
    {
        #[cfg(test)]
        self.reads.fetch_add(1, Ordering::Relaxed);
        let entry = self.entries.lock().await.entry(source.clone()).or_default().clone();
        let mut entry_state = entry.lock().await;
        let state = &mut *entry_state;
        if !state.refreshing && state.last_attempt.is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL) {
            state.refreshing = true;
            state.last_attempt = Some(Instant::now());
            let refresh_entry = entry.clone();
            tokio::spawn(async move {
                // Catch panics both while constructing and polling the future.
                // Treat them as an observation failure so the source can retry.
                let result = match tokio::time::timeout(REFRESH_TIMEOUT, AssertUnwindSafe(async move { load().await }).catch_unwind()).await
                {
                    Ok(Ok(result)) => result,
                    Ok(Err(_)) => Err("board tracker refresh panicked".into()),
                    Err(_) => Err("board tracker refresh timed out".into()),
                };
                let mut entry = refresh_entry.lock().await;
                entry.refreshing = false;
                // Retry backoff starts on completion, including a slow failure.
                entry.last_attempt = Some(Instant::now());
                match result {
                    Ok(board) => {
                        if entry.board.as_ref().is_none_or(|previous| previous.issues != board.issues) {
                            entry.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
                        }
                        entry.board = Some(Arc::new(board));
                        entry.error = None;
                    }
                    Err(error) => {
                        if let Some(board) = &mut entry.board {
                            Arc::make_mut(board).refresh_error = Some(error.clone());
                        }
                        entry.error = Some(error);
                    }
                }
            });
        }
        let Some(board) = state.board.clone() else {
            return Err(format!(
                "board facts unavailable for {}: {}",
                source.scope,
                state.error.as_deref().unwrap_or("initial observation in progress; retry shortly")
            ));
        };
        Ok((state.revision, board))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_protocol::{DispatchBoardIssue, DispatchBoardPullRequest, IssueState};

    use super::*;

    async fn settled(cache: &DispatchBoardCache, source: &IssueSource) {
        let entry = cache.entries.lock().await.get(source).expect("entry").clone();
        settled_entry(entry).await;
    }

    async fn settled_entry(entry: SharedEntry) {
        // Bounded scheduler polling also catches wedged entries under paused
        // time, where an always-ready polling loop cannot advance a timeout.
        for _ in 0..1000 {
            if !entry.lock().await.refreshing {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("refresh did not settle");
    }

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

    // The real board watchdog allows ForgeReads to finish its earlier deadline
    // and expose the last-good board with the observation error, rather than
    // racing both 300s deadlines and replacing it with a tracker timeout.
    #[tokio::test(start_paused = true)]
    async fn layered_deadlines_preserve_observation_timeout_and_recover() {
        use crate::forge_observation::ForgeReads;
        use flotilla_resources::{Clock, InMemoryBackend, ResourceBackend, VirtualClock};
        let cache = DispatchBoardCache::default();
        let source = source("org/shared");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(flotilla_protocol::NodeId::new("owner"));
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        let mut reads = ForgeReads::new(backend, "flotilla".into()).with_clock(clock.clone());
        reads.allow_stale = true;
        let snapshot = board(&source, 1);
        reads.read(&source, flotilla_resources::ForgeReadRequest::Board, || async { Ok(snapshot.clone()) }).await.unwrap();
        clock.advance(chrono::Duration::seconds(60));
        let slow_reads = reads.clone();
        let slow_source = source.clone();
        let (entered, started) = tokio::sync::oneshot::channel();
        cache
            .read(&source, move || async move {
                // Model request/resource-store setup before the load timer.
                // Equal layered deadlines cannot tolerate this small overhead.
                tokio::time::sleep(Duration::from_secs(1)).await;
                let result = slow_reads
                    .read(&slow_source, flotilla_resources::ForgeReadRequest::Board, || async {
                        entered.send(()).unwrap();
                        std::future::pending::<Result<DispatchBoardRepository, String>>().await
                    })
                    .await;
                let mut board = result?;
                board.refresh_error = slow_reads
                    .status(&flotilla_resources::forge_read_name(&slow_source, &flotilla_resources::ForgeReadRequest::Board))
                    .await?
                    .and_then(|status| status.error);
                Ok(board)
            })
            .await
            .expect_err("cold cache");
        started.await.unwrap();
        tokio::time::advance(crate::forge_observation::LOAD_TIMEOUT - Duration::from_secs(1)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        settled(&cache, &source).await;
        let cached = cache.read(&source, || async { panic!("cached timeout") }).await.unwrap();
        assert_eq!(cached.issues, snapshot.issues);
        assert_eq!(cached.refresh_error.as_deref(), Some("forge observation timed out"));
        clock.advance(chrono::Duration::seconds(60));
        tokio::time::advance(REFRESH_INTERVAL).await;
        let recovered = snapshot.clone();
        let refresh_source = source.clone();
        cache
            .read(&source, move || async move {
                reads.read(&refresh_source, flotilla_resources::ForgeReadRequest::Board, || async { Ok(recovered) }).await
            })
            .await
            .unwrap();
        settled(&cache, &source).await;
        let cached = cache.read(&source, || async { panic!("cached recovery") }).await.unwrap();
        assert!(cached.refresh_error.is_none());
        assert_eq!(cached.issues, snapshot.issues);
        assert!(clock.now() > snapshot.observed_at);
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
        for _ in 0..1000 {
            if calls.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(20)).await;
        settled(&cache, &source).await;
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
        settled(&cache, &source).await;
        // Make the observation old without using real-time sleeps.
        cache
            .entries
            .lock()
            .await
            .get(&source)
            .expect("entry")
            .lock()
            .await
            .board
            .as_mut()
            .map(Arc::make_mut)
            .expect("board")
            .observed_at -= chrono::Duration::seconds(120);
        tokio::time::advance(REFRESH_INTERVAL).await;
        let cached = cache.read(&source, || async { Err("forge offline".into()) }).await.expect("stale board");
        assert!(cached.age_seconds >= 120);
        settled(&cache, &source).await;
        let cached = cache.read(&source, || async { panic!("failure backoff") }).await.expect("last successful board");
        assert_eq!(cached.refresh_error.as_deref(), Some("forge offline"));
        assert!(cached.issues.is_empty());
        tokio::time::advance(REFRESH_INTERVAL).await;
        let recovered = board(&source, 1);
        cache.read(&source, move || async { Ok(recovered) }).await.expect("old facts during recovery");
        settled(&cache, &source).await;
        let cached = cache.read(&source, || async { panic!("fresh observation") }).await.expect("recovered");
        assert_eq!(cached.issues.len(), 1);
        assert!(cached.refresh_error.is_none());
        assert!(cache
            .read(&IssueSource { service: "https://github.com".into(), scope: "org/other".into() }, || async {
                Err("other offline".into())
            })
            .await
            .is_err());
        settled(&cache, &IssueSource { service: "https://github.com".into(), scope: "org/other".into() }).await;
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
                settled(&cache, &source).await;
                let actual = cache.read(&source, || async { panic!("fresh source") }).await.expect("snapshot");
                assert_eq!(actual.source, expected.source);
                assert_eq!(actual.issues, expected.issues);
                assert_eq!(actual.pull_requests, expected.pull_requests);
            }
        });
    }
    // A tracker panic, during construction or polling, is a visible failure;
    // cold and last-good sources both become retryable after backoff.
    #[tokio::test(start_paused = true)]
    async fn loader_panics_release_refresh_and_recover() {
        fn construction_panic() -> std::future::Ready<Result<DispatchBoardRepository, String>> {
            panic!("tracker construction panic");
        }
        for cached in [false, true] {
            for construction in [false, true] {
                let cache = DispatchBoardCache::default();
                let source = source("org/panic");
                if cached {
                    let snapshot = board(&source, 2);
                    assert!(cache.read(&source, move || async { Ok(snapshot) }).await.is_err());
                    settled(&cache, &source).await;
                    tokio::time::advance(REFRESH_INTERVAL).await;
                }
                if construction {
                    let _ = cache.read(&source, construction_panic).await;
                } else {
                    let _ = cache.read(&source, || async { panic!("tracker polling panic") }).await;
                }
                settled(&cache, &source).await;
                let result = cache.read(&source, || async { panic!("backoff must prevent another observation") }).await;
                if cached {
                    let snapshot = result.expect("last-good snapshot");
                    assert_eq!(snapshot.issues.len(), 2);
                    assert_eq!(snapshot.refresh_error.as_deref(), Some("board tracker refresh panicked"));
                } else {
                    assert!(result.expect_err("cold failure").contains("refresh panicked"));
                }
                tokio::time::advance(REFRESH_INTERVAL).await;
                let snapshot = board(&source, 3);
                let _ = cache.read(&source, move || async { Ok(snapshot) }).await;
                settled(&cache, &source).await;
                let snapshot = cache.read(&source, || async { panic!("fresh observation") }).await.expect("recovered");
                assert_eq!(snapshot.issues.len(), 3);
                assert!(snapshot.refresh_error.is_none());
            }
        }
    }

    // #2859: cache revisions describe issue deltas, not polling time or refresh
    // errors. Failed refreshes retain item-local observation timestamps; eviction
    // and re-addition cannot reuse a cursor and hide replacement facts.
    #[tokio::test(start_paused = true)]
    async fn revisions_ignore_refresh_metadata_and_survive_readdition() {
        let cache = DispatchBoardCache::default();
        let source = source("org/revisions");
        let original = board(&source, 2);
        let initial = original.clone();
        assert!(cache.read_snapshot(&source, move || async { Ok(initial) }).await.is_err());
        settled(&cache, &source).await;
        let (revision, snapshot) = cache.read_snapshot(&source, || async { panic!("cached") }).await.expect("snapshot");
        let (_, duplicate) = cache.read_snapshot(&source, || async { panic!("cached") }).await.expect("duplicate");
        assert!(Arc::ptr_eq(&snapshot, &duplicate));
        tokio::time::advance(REFRESH_INTERVAL).await;
        let mut refreshed = original.clone();
        refreshed.observed_at += chrono::Duration::seconds(60);
        cache.read_snapshot(&source, move || async { Ok(refreshed) }).await.expect("last good");
        settled(&cache, &source).await;
        assert_eq!(cache.read_snapshot(&source, || async { panic!("cached") }).await.expect("metadata refresh").0, revision);
        tokio::time::advance(REFRESH_INTERVAL).await;
        cache.read_snapshot(&source, || async { Err("offline".into()) }).await.expect("last good");
        settled(&cache, &source).await;
        let (stale_revision, stale) = cache.read_snapshot(&source, || async { panic!("backoff") }).await.expect("stale");
        assert_eq!(stale_revision, revision);
        assert_eq!(stale.issues, original.issues);
        assert_eq!(stale.refresh_error.as_deref(), Some("offline"));
        cache.retain_sources(&BTreeSet::new()).await;
        let replaced = board(&source, 1);
        assert!(cache.read_snapshot(&source, move || async { Ok(replaced) }).await.is_err());
        settled(&cache, &source).await;
        let (next_revision, replaced) = cache.read_snapshot(&source, || async { panic!("cached") }).await.expect("readded");
        assert_ne!(next_revision, revision);
        assert_eq!(replaced.issues.len(), 1);
        assert!(replaced.refresh_error.is_none());
    }

    // Removed sources are forgotten. Completion of an old in-flight observation
    // must not recreate an evicted entry or overwrite a newly added source.
    #[tokio::test]
    async fn eviction_isolates_readded_source_from_old_refresh() {
        let cache = DispatchBoardCache::default();
        let source = source("org/readded");
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let old = board(&source, 1);
        let old_release = release.clone();
        assert!(cache
            .read(&source, move || async move {
                old_release.acquire().await.expect("release old observation").forget();
                Ok(old)
            })
            .await
            .is_err());
        let retired = cache.entries.lock().await.get(&source).expect("old entry").clone();
        cache.retain_sources(&BTreeSet::new()).await;
        assert!(cache.entries.lock().await.is_empty());
        let new = board(&source, 2);
        assert!(cache.read(&source, move || async { Ok(new) }).await.is_err());
        settled(&cache, &source).await;
        release.add_permits(1);
        settled_entry(retired).await;
        let snapshot = cache.read(&source, || async { panic!("new source stays fresh") }).await.expect("new snapshot");
        assert_eq!(snapshot.issues.len(), 2);
        assert_eq!(cache.entries.lock().await.len(), 1);
        cache.retain_sources(&BTreeSet::from([source.clone()])).await;
        assert_eq!(cache.read(&source, || async { panic!("retained source") }).await.expect("retained").issues, snapshot.issues);
    }
}
