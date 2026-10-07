//! Slow tracker observations outlive interactive requests. One refresh per source
//! serves all Projects, retaining the last successful snapshot during outages.
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    panic::AssertUnwindSafe,
    sync::Arc,
    time::Duration,
};

use chrono::Utc;
use flotilla_protocol::{DispatchBoardRepository, IssueSource};
use futures::{stream, FutureExt, StreamExt};
use tokio::{sync::Mutex, time::Instant};

use crate::providers::issue_tracker::{footprints::FootprintIndex, IssueProvider};

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
// Allow paginated large-project forge reads several minutes while bounding hung refreshes.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct Entry {
    board: Option<DispatchBoardRepository>,
    last_attempt: Option<Instant>,
    refreshing: bool,
    error: Option<String>,
    indices: Arc<Mutex<BoardIndices>>,
}

#[derive(Default)]
pub(super) struct BoardIndices {
    pub footprints: FootprintIndex,
    pub mission_fields: BTreeMap<String, flotilla_protocol::MissionFields>,
}

impl BoardIndices {
    pub(super) async fn refresh_missions(
        &mut self,
        provider: &dyn IssueProvider,
        source: &IssueSource,
        issues: &mut [flotilla_protocol::DispatchBoardIssue],
        selected: &BTreeSet<String>,
    ) {
        let ids = selected.iter().cloned().collect::<Vec<_>>();
        let fetched = stream::iter(ids)
            .map(|id: String| async move {
                let reference = flotilla_protocol::IssueRef { source: source.clone(), id: id.clone() };
                let result = tokio::time::timeout(Duration::from_secs(20), provider.mission_fields(&reference))
                    .await
                    .unwrap_or_else(|_| Err("mission field item timed out".into()));
                (id.clone(), result)
            })
            .buffer_unordered(8)
            .collect::<BTreeMap<_, _>>()
            .await;
        let present = issues.iter().map(|issue| issue.id.clone()).collect::<BTreeSet<_>>();
        self.mission_fields.retain(|id, _| present.contains(id));
        for issue in issues {
            match fetched.get(&issue.id) {
                Some(Ok(fields)) => {
                    self.mission_fields.insert(issue.id.clone(), fields.clone());
                    issue.mission_fields = fields.clone();
                    issue.mission_fields_error = None;
                }
                Some(Err(error)) => {
                    issue.mission_fields_error = Some(error.clone());
                    issue.mission_fields = self.mission_fields.get(&issue.id).cloned().unwrap_or_default();
                }
                None => {}
            }
        }
    }
}

type SharedEntry = Arc<Mutex<Entry>>;
type ProjectSources = BTreeMap<String, BTreeSet<IssueSource>>;

#[derive(Clone, Default)]
pub(super) struct DispatchBoardCache(Arc<Mutex<BTreeMap<IssueSource, SharedEntry>>>, Arc<Mutex<Option<ProjectSources>>>);

impl DispatchBoardCache {
    pub(super) async fn set_project_sources(&self, sources: ProjectSources) {
        *self.1.lock().await = Some(sources);
    }

    /// Serving does no resource inventory, forge calls, prediction or pair work.
    pub(super) async fn snapshots(&self, project: Option<&str>) -> Result<Vec<DispatchBoardRepository>, String> {
        let inventory = self.1.lock().await;
        let inventory = inventory.as_ref().ok_or("initial observation source inventory in progress")?;
        let sources = if let Some(project) = project {
            inventory.get(project).cloned().ok_or_else(|| format!("board Project {project} is not in the source inventory"))?
        } else {
            inventory.values().flatten().cloned().collect()
        };
        let entries = self.0.lock().await;
        let mut boards = Vec::new();
        for source in sources {
            let entry = entries.get(&source).ok_or_else(|| format!("initial observation in progress for {}", source.scope))?;
            let state = entry.lock().await;
            let mut board = state.board.clone().ok_or_else(|| {
                format!(
                    "board facts unavailable for {}: {}",
                    source.scope,
                    state.error.as_deref().unwrap_or("initial observation in progress")
                )
            })?;
            board.age_seconds = Utc::now().signed_duration_since(board.observed_at).num_seconds().max(0) as u64;
            board.refresh_error = state.error.clone();
            boards.push(board);
        }
        Ok(boards)
    }

    /// Only a complete source inventory may retire entries. An old refresh owns
    /// its retired entry, so its completion cannot overwrite a re-added source.
    pub(super) async fn retain_sources(&self, sources: &BTreeSet<IssueSource>) {
        self.0.lock().await.retain(|source, _| sources.contains(source));
    }

    #[cfg(test)]
    pub(super) async fn read<F, Fut>(&self, source: &IssueSource, load: F) -> Result<DispatchBoardRepository, String>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<DispatchBoardRepository, String>> + Send,
    {
        self.read_indexed(source, move |_| load()).await
    }

    pub(super) async fn read_indexed<F, Fut>(&self, source: &IssueSource, load: F) -> Result<DispatchBoardRepository, String>
    where
        F: FnOnce(Arc<Mutex<BoardIndices>>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<DispatchBoardRepository, String>> + Send,
    {
        let entry = self.0.lock().await.entry(source.clone()).or_default().clone();
        let mut entry_state = entry.lock().await;
        let state = &mut *entry_state;
        if !state.refreshing && state.last_attempt.is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL) {
            state.refreshing = true;
            state.last_attempt = Some(Instant::now());
            let refresh_entry = entry.clone();
            let indices = state.indices.clone();
            tokio::spawn(async move {
                // Catch panics both while constructing and polling the future.
                // Treat them as an observation failure so the source can retry.
                let result = match tokio::time::timeout(
                    REFRESH_TIMEOUT,
                    AssertUnwindSafe(async move { load(indices).await }).catch_unwind(),
                )
                .await
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
                    Ok(mut board) => {
                        board.observed_at = Utc::now();
                        entry.board = Some(board);
                        entry.error = None;
                    }
                    Err(error) => entry.error = Some(error),
                }
            });
        }
        let Some(mut board) = state.board.clone() else {
            return Err(format!(
                "board facts unavailable for {}: {}",
                source.scope,
                state.error.as_deref().unwrap_or("initial observation in progress; retry shortly")
            ));
        };
        board.age_seconds = Utc::now().signed_duration_since(board.observed_at).num_seconds().max(0) as u64;
        board.refresh_error = state.error.clone();
        Ok(board)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_protocol::{DispatchBoardIssue, DispatchBoardPullRequest, IssueState};

    use super::*;

    async fn settled(cache: &DispatchBoardCache, source: &IssueSource) {
        let entry = cache.0.lock().await.get(source).expect("entry").clone();
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
            footprints: None,
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

    // Failed mission-field items retain last-good values while peers/source facts
    // refresh normally. Snapshot reads invoke neither the forge nor report work.
    #[tokio::test(start_paused = true)]
    async fn mission_item_failure_and_snapshot_reads_preserve_precomputed_work_bounds() {
        use std::sync::atomic::AtomicBool;

        use async_trait::async_trait;
        use flotilla_protocol::{
            issue_query::{IssueQuery, IssueResultPage},
            Issue, IssueChangeset, IssueRef, MissionFields,
        };
        struct Forge {
            failed: AtomicBool,
            calls: AtomicUsize,
        }
        #[async_trait]
        impl IssueProvider for Forge {
            fn supports(&self, _source: &IssueSource) -> bool {
                true
            }
            async fn query(
                &self,
                _source: &IssueSource,
                _params: &IssueQuery,
                _page: u32,
                _count: usize,
            ) -> Result<IssueResultPage, String> {
                Err("unused query".into())
            }
            async fn fetch_by_id(&self, _reference: &IssueRef) -> Result<Issue, String> {
                Err("unused fetch".into())
            }
            async fn list_changed_since(&self, _source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
                Err("unused changes".into())
            }
            async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
                Err("unused browser".into())
            }
            async fn mission_fields(&self, _reference: &IssueRef) -> Result<MissionFields, String> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if self.failed.load(Ordering::SeqCst) {
                    Err("field item unavailable".into())
                } else {
                    Ok(MissionFields { value: Some(5.0.try_into().expect("finite value")), ..Default::default() })
                }
            }
        }
        let cache = DispatchBoardCache::default();
        let source = source("org/items");
        cache.set_project_sources(BTreeMap::from([("project".into(), BTreeSet::from([source.clone()]))])).await;
        let forge = Arc::new(Forge { failed: AtomicBool::new(false), calls: AtomicUsize::new(0) });
        let prepares = Arc::new(AtomicUsize::new(0));
        for pass in 0..2 {
            let snapshot = board(&source, 2);
            let load_source = source.clone();
            let provider = forge.clone();
            let count = prepares.clone();
            let _ = cache
                .read_indexed(&source, move |indices| async move {
                    let mut snapshot = snapshot;
                    let mut indices = indices.lock().await;
                    indices.refresh_missions(provider.as_ref(), &load_source, &mut snapshot.issues, &BTreeSet::from(["0".into()])).await;
                    let mut observation = flotilla_protocol::FootprintObservation::default();
                    count.fetch_add(1, Ordering::SeqCst);
                    crate::dispatch_footprints::prepare_observation(&load_source, &mut observation, &[], &BTreeMap::new());
                    snapshot.footprints = Some(observation);
                    Ok(snapshot)
                })
                .await;
            settled(&cache, &source).await;
            for _ in 0..200 {
                let snapshots = cache.snapshots(Some("project")).await.expect("served snapshot");
                let snapshot = &snapshots[0];
                assert!(snapshot.refresh_error.is_none());
                assert_eq!(snapshot.issues[0].mission_fields.value, Some(5.0.try_into().expect("value")));
                assert_eq!(snapshot.issues[0].mission_fields_error.is_some(), pass == 1);
                assert!(snapshot.issues[1].mission_fields_error.is_none());
                assert_eq!(forge.calls.load(Ordering::SeqCst), pass + 1);
                assert_eq!(prepares.load(Ordering::SeqCst), pass + 1);
            }
            forge.failed.store(true, Ordering::SeqCst);
            tokio::time::advance(REFRESH_INTERVAL).await;
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
        cache.0.lock().await.get(&source).expect("entry").lock().await.board.as_mut().expect("board").observed_at -=
            chrono::Duration::seconds(120);
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
        let retired = cache.0.lock().await.get(&source).expect("old entry").clone();
        cache.retain_sources(&BTreeSet::new()).await;
        assert!(cache.0.lock().await.is_empty());
        let new = board(&source, 2);
        assert!(cache.read(&source, move || async { Ok(new) }).await.is_err());
        settled(&cache, &source).await;
        release.add_permits(1);
        settled_entry(retired).await;
        let snapshot = cache.read(&source, || async { panic!("new source stays fresh") }).await.expect("new snapshot");
        assert_eq!(snapshot.issues.len(), 2);
        assert_eq!(cache.0.lock().await.len(), 1);
        cache.retain_sources(&BTreeSet::from([source.clone()])).await;
        assert_eq!(cache.read(&source, || async { panic!("retained source") }).await.expect("retained").issues, snapshot.issues);
    }
}
