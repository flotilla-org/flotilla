//! Slow tracker observations outlive interactive requests. One refresh per source
//! serves all Projects, retaining the last successful snapshot during outages.
use flotilla_resources::convoy_accounting::active_crew_count;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Weak,
    },
    time::Duration,
};

use flotilla_protocol::{DispatchBoardRepository, IssueSource};
use flotilla_store::load_metrics::{attributed, caller};
use futures::{future::BoxFuture, FutureExt};
use tokio::sync::{watch, Mutex};
use tracing::Instrument;

// Process-local identities: never persist or compare revisions across daemons.
static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
// ForgeReads owns the 300s load deadline and keeps its timeout/last-good
// result. This outer watchdog must allow that completion, rather than racing
// the inner deadline and discarding its result. It also bounds bare providers.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(300).saturating_add(Duration::from_secs(30));

type BoardLoad = Arc<dyn Fn() -> BoxFuture<'static, Result<DispatchBoardRepository, String>> + Send + Sync>;

#[derive(Default)]
struct Entry {
    board: Option<Arc<DispatchBoardRepository>>,
    revision: u64,
    refreshing: bool,
    error: Option<String>,
    /// The latest reader's observation: each refresh uses the newest one.
    load: Option<BoardLoad>,
    scheduled: bool,
}

type SharedEntry = Arc<Mutex<Entry>>;

struct Shared {
    entries: Mutex<BTreeMap<IssueSource, SharedEntry>>,
    changes: watch::Sender<()>,
    on_change: Box<dyn Fn(&IssueSource) + Send + Sync>,
}

impl Shared {
    async fn retains(&self, source: &IssueSource, entry: &SharedEntry) -> bool {
        self.entries.lock().await.get(source).is_some_and(|current| Arc::ptr_eq(current, entry))
    }
}

/// The board poll is the dispatch probe: each source read once is refreshed every
/// REFRESH_INTERVAL until retired, and a refresh that changes its issues, or recovers
/// from a failure, publishes a change.
#[derive(Clone)]
pub struct DispatchBoardCache {
    shared: Arc<Shared>,
}

impl Default for DispatchBoardCache {
    fn default() -> Self {
        Self::new(|_| {})
    }
}

impl DispatchBoardCache {
    /// `on_change` runs for a changed source before the change is published, so a woken
    /// reader sees whatever it marks.
    pub fn new(on_change: impl Fn(&IssueSource) + Send + Sync + 'static) -> Self {
        Self { shared: Arc::new(Shared { entries: Mutex::default(), changes: watch::Sender::new(()), on_change: Box::new(on_change) }) }
    }

    /// Marked changed after each published change.
    pub fn changes(&self) -> watch::Receiver<()> {
        self.shared.changes.subscribe()
    }

    /// Only a complete source inventory may retire entries. An old refresh owns
    /// its retired entry, so its completion cannot overwrite a re-added source,
    /// and a retired source's refreshes stop.
    pub async fn retain_sources(&self, sources: &BTreeSet<IssueSource>) {
        self.shared.entries.lock().await.retain(|source, _| sources.contains(source));
    }

    pub async fn read<F, Fut>(&self, source: &IssueSource, load: F) -> Result<DispatchBoardRepository, String>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<DispatchBoardRepository, String>> + Send + 'static,
    {
        self.read_snapshot(source, load).await.map(|(_, board)| {
            let mut board = (*board).clone();
            board.age_seconds = flotilla_resources::clock::now().signed_duration_since(board.observed_at).num_seconds().max(0) as u64;
            board
        })
    }

    pub async fn read_snapshot<F, Fut>(&self, source: &IssueSource, load: F) -> Result<(u64, Arc<DispatchBoardRepository>), String>
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<DispatchBoardRepository, String>> + Send + 'static,
    {
        let entry = self.shared.entries.lock().await.entry(source.clone()).or_default().clone();
        let mut state = entry.lock().await;
        state.load = Some(Arc::new(move || load().boxed()));
        if !state.scheduled {
            state.scheduled = true;
            state.refreshing = true;
            let refreshed = refresh(Arc::downgrade(&self.shared), source.clone(), entry.clone());
            tokio::spawn(attributed(caller("dispatch board"), refreshed.instrument(tracing::info_span!("dispatch_board"))));
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

/// Refreshes one source until it is retired or the cache is dropped. The interval runs from
/// each completion, so a slow failure is not retried sooner.
async fn refresh(shared: Weak<Shared>, source: IssueSource, entry: SharedEntry) {
    loop {
        let load = {
            let mut state = entry.lock().await;
            state.refreshing = true;
            state.load.clone().expect("a scheduled entry holds its load")
        };
        // Catch panics both while constructing and polling the future.
        // Treat them as an observation failure so the source can retry.
        let result = match tokio::time::timeout(REFRESH_TIMEOUT, AssertUnwindSafe(async move { load().await }).catch_unwind()).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("board tracker refresh panicked".into()),
            Err(_) => Err("board tracker refresh timed out".into()),
        };
        let changed = {
            let mut state = entry.lock().await;
            state.refreshing = false;
            match result {
                Ok(board) => {
                    let changed = state.board.as_ref().is_none_or(|previous| previous.issues != board.issues);
                    if changed {
                        state.revision = NEXT_REVISION.fetch_add(1, Ordering::Relaxed);
                    }
                    state.board = Some(Arc::new(board));
                    changed || state.error.take().is_some()
                }
                Err(error) => {
                    if let Some(board) = &mut state.board {
                        Arc::make_mut(board).refresh_error = Some(error.clone());
                    }
                    state.error = Some(error);
                    false
                }
            }
        };
        let Some(cache) = shared.upgrade() else { return };
        if !cache.retains(&source, &entry).await {
            return;
        }
        if changed {
            (cache.on_change)(&source);
            cache.changes.send_replace(());
        }
        drop(cache);
        tokio::time::sleep(REFRESH_INTERVAL).await;
        let Some(cache) = shared.upgrade() else { return };
        if !cache.retains(&source, &entry).await {
            return;
        }
    }
}

use flotilla_resources::{IssueSourceResolution, Project, Repository, ResourceObject};
use flotilla_store::ResourceBackend;
pub type DispatchBoardInputs = BTreeMap<
    String,
    Result<(Vec<flotilla_resources::ResolvedIssueSourceBinding>, Vec<(u64, Arc<flotilla_protocol::DispatchBoardRepository>)>), String>,
>;

/// Indexed live inventory: retained terminal history never enters accounting reads.
pub fn live_project_convoys_query(projects: impl IntoIterator<Item = String>) -> flotilla_store::StoreQuery {
    use flotilla_store::StoreQuery;
    StoreQuery {
        field_values: BTreeMap::from([("spec.project_ref".into(), projects.into_iter().collect())]),
        ..flotilla_store::live_convoys_query()
    }
}

pub struct DispatchBoard<'a> {
    pub backend: &'a ResourceBackend,
    pub cache: &'a DispatchBoardCache,
    pub resolver: std::sync::Weak<dyn crate::IssueMaterializationResolver>,
    pub namespace: String,
}
impl DispatchBoard<'_> {
    pub async fn dispatch_board_internal(&self, project_filter: Option<&str>) -> Result<flotilla_protocol::DispatchBoardResponse, String> {
        let readiness = crate::read_projections::ReadProjections::dispatch_queue(
            self.backend,
            &self.namespace,
            project_filter,
            flotilla_resources::clock::now(),
        )
        .await?;
        let repositories = self.dispatch_board_repositories_internal(project_filter).await?;
        Ok(flotilla_protocol::DispatchBoardResponse { readiness, repositories })
    }
    pub async fn dispatch_board_repositories_internal(
        &self,
        project_filter: Option<&str>,
    ) -> Result<Vec<flotilla_protocol::DispatchBoardRepository>, String> {
        let namespace = self.namespace.clone();
        let projects = self.backend.definitions::<Project>(&namespace).list().await.map_err(|error| error.to_string())?;
        let projects =
            projects.into_iter().filter(|project| project_filter.is_none_or(|name| name == project.metadata.name)).collect::<Vec<_>>();
        let snapshot = self.collect_dispatch_board_inputs(&projects).await?;
        if project_filter.is_none() && snapshot.values().all(Result::is_ok) {
            let sources = snapshot
                .values()
                .filter_map(|input| input.as_ref().ok())
                .flat_map(|(bindings, _)| bindings.iter().map(|binding| binding.source.clone()))
                .collect();
            self.cache.retain_sources(&sources).await;
        }
        let mut repositories = BTreeMap::new();
        let mut errors = Vec::new();
        for input in snapshot.into_values() {
            match input {
                Ok((_, boards)) => {
                    for (_, board) in boards {
                        repositories.insert(board.source.clone(), board);
                    }
                }
                Err(error) => errors.push(error),
            }
        }
        if errors.is_empty() {
            Ok(repositories
                .into_values()
                .map(|board| {
                    let mut board = (*board).clone();
                    board.age_seconds =
                        flotilla_resources::clock::now().signed_duration_since(board.observed_at).num_seconds().max(0) as u64;
                    board
                })
                .collect())
        } else {
            Err(errors.join("; "))
        }
    }
    #[tracing::instrument(name = "dispatch_board", skip_all)]
    pub async fn collect_dispatch_board_inputs(&self, projects: &[ResourceObject<Project>]) -> Result<DispatchBoardInputs, String> {
        let namespace = self.namespace.clone();
        let inventory = self.backend.including_replicas::<Repository>(&namespace).list().await.map_err(|error| error.to_string())?;
        let inventory = inventory.items.into_iter().map(|record| (record.object.metadata.name.clone(), Ok(record.object))).collect();
        let mut bindings_by_project = BTreeMap::new();
        let mut sources = std::collections::BTreeSet::new();
        let mut mission_issues = std::collections::BTreeSet::new();
        // Running tickets need current fields for tier-specific crew accounting,
        // even after their ready label is removed. Do not fetch inactive backlog.
        // Fail closed on store failure: missing live tickets would omit their
        // Priority fields from fallback observations and bias tier occupancy.
        let live = self
            .backend
            .query_declared::<flotilla_resources::Convoy>(
                &namespace,
                &live_project_convoys_query(projects.iter().map(|project| project.metadata.name.clone())),
            )
            .await
            .map_err(|e| e.to_string())?;
        for record in live.items {
            if active_crew_count(record.object.status.as_ref()) > 0 {
                mission_issues.extend(record.object.spec.issues.into_iter().map(|issue| issue.reference));
            }
        }

        for project in projects {
            if let Some(policy) = &project.spec.dispatch_policy {
                mission_issues.extend(policy.missions.iter().filter_map(|mission| mission.issue.clone()));
            }
            let bindings = match flotilla_resources::resolve_project_issue_sources_from_inventory(&inventory, &project.spec) {
                IssueSourceResolution::Available { bindings } => {
                    sources.extend(bindings.iter().map(|binding| binding.source.clone()));
                    Ok(bindings)
                }
                IssueSourceResolution::Unavailable(error) => Err(format!("project {}: {error:?}", project.metadata.name)),
            };
            bindings_by_project.insert(project.metadata.name.clone(), bindings);
        }
        let mut repositories = BTreeMap::new();
        for source in sources {
            let resolver = self.resolver.clone();
            let tracker_source = source.clone();
            let mission_issues = mission_issues.clone();
            let result = self
                .cache
                .read_snapshot(&source, move || {
                    let resolver = resolver.clone();
                    let tracker_source = tracker_source.clone();
                    let mission_issues = mission_issues.clone();
                    async move {
                        let resolver = resolver.upgrade().ok_or("daemon stopped")?;
                        let provider = resolver.issue_provider_for(&tracker_source).await?;
                        let mut board = provider.dispatch_board(&tracker_source).await?;
                        for issue in &mut board.issues {
                            let reference = flotilla_protocol::IssueRef { source: tracker_source.clone(), id: issue.id.clone() };
                            if !provider.board_includes_mission_fields()
                                && (mission_issues.iter().any(|mission| {
                                    mission.id == reference.id
                                        && mission.source.scope.eq_ignore_ascii_case(&reference.source.scope)
                                        && matches!(
                                            mission.source.service.trim_end_matches('/'),
                                            "github" | "github.com" | "https://github.com"
                                        )
                                }) || issue.labels.iter().any(|label| label == "ready")
                                    || issue
                                        .issue_type
                                        .as_ref()
                                        .is_some_and(|kind| kind.eq_ignore_ascii_case("map") || kind.eq_ignore_ascii_case("mission"))
                                    || issue
                                        .labels
                                        .iter()
                                        .any(|label| label.rsplit(':').next().is_some_and(|kind| kind.eq_ignore_ascii_case("map"))))
                            {
                                // ObservedIssueProvider coalesces this source/ticket
                                // through ForgeReadRequest::Mission for 60 seconds.
                                // Only ready/live tickets and tracking issues qualify.
                                issue.mission_fields = provider.mission_fields(&reference).await?;
                            }
                        }
                        Ok(board)
                    }
                })
                .await;
            repositories.insert(source, result);
        }
        Ok(bindings_by_project
            .into_iter()
            .map(|(name, bindings)| {
                let input = bindings.and_then(|bindings| {
                    let boards = bindings.iter().map(|binding| repositories[&binding.source].clone()).collect::<Result<Vec<_>, _>>()?;
                    Ok((bindings, boards))
                });
                (name, input)
            })
            .collect())
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_protocol::{DispatchBoardIssue, DispatchBoardPullRequest, IssueState};

    use super::*;

    async fn settled(cache: &DispatchBoardCache, source: &IssueSource) {
        let entry = cache.shared.entries.lock().await.get(source).expect("entry").clone();
        settled_entry(entry).await;
    }

    async fn settled_entry(entry: SharedEntry) {
        // Lets a woken refresh start first. Bounded scheduler polling also catches
        // wedged entries under paused time, where an always-ready polling loop
        // cannot advance a timeout.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        for _ in 0..1000 {
            if !entry.lock().await.refreshing {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("refresh did not settle");
    }

    /// Runs the source's next scheduled refresh to completion.
    async fn next_refresh(cache: &DispatchBoardCache, source: &IssueSource) {
        tokio::time::advance(REFRESH_INTERVAL).await;
        settled(cache, source).await;
    }

    /// The cached snapshot, read without registering a loader.
    async fn cached(cache: &DispatchBoardCache, source: &IssueSource) -> Result<(u64, Arc<DispatchBoardRepository>), String> {
        let entry = cache.shared.entries.lock().await.get(source).expect("entry").clone();
        let state = entry.lock().await;
        state.board.clone().map(|board| (state.revision, board)).ok_or_else(|| state.error.clone().unwrap_or_default())
    }

    fn answer(
        result: Result<DispatchBoardRepository, String>,
    ) -> impl Fn() -> std::future::Ready<Result<DispatchBoardRepository, String>> + Send + Sync + 'static {
        move || std::future::ready(result.clone())
    }

    fn source(scope: &str) -> IssueSource {
        IssueSource { service: "https://github.com".into(), scope: scope.into() }
    }

    pub fn board(source: &IssueSource, count: usize) -> DispatchBoardRepository {
        DispatchBoardRepository {
            source: source.clone(),
            observed_at: flotilla_resources::clock::now(),
            age_seconds: 0,
            refresh_error: None,
            issues: (0..count)
                .map(|id| {
                    DispatchBoardIssue::builder()
                        .id(id.to_string())
                        .title(format!("Issue {id}"))
                        .state(IssueState::Open)
                        .url(format!("https://github.com/{}/issues/{id}", source.scope))
                        .updated_at(flotilla_resources::clock::now().to_rfc3339())
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
                .read(&source, move || {
                    let snapshot = snapshot.clone();
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_secs(20)).await;
                        Ok(snapshot)
                    }
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
        assert!(cache.read(&source, answer(Ok(board(&source, 0)))).await.is_err());
        settled(&cache, &source).await;
        // Make the observation old without using real-time sleeps.
        cache
            .shared
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
        let cached_board = cache.read(&source, answer(Err("forge offline".into()))).await.expect("stale board");
        assert!(cached_board.age_seconds >= 120);
        next_refresh(&cache, &source).await;
        let failed = cache.read(&source, answer(Err("forge offline".into()))).await.expect("last successful board");
        assert_eq!(failed.refresh_error.as_deref(), Some("forge offline"));
        assert!(failed.issues.is_empty());
        cache.read(&source, answer(Ok(board(&source, 1)))).await.expect("old facts during recovery");
        next_refresh(&cache, &source).await;
        let (_, recovered) = cached(&cache, &source).await.expect("recovered");
        assert_eq!(recovered.issues.len(), 1);
        assert!(recovered.refresh_error.is_none());
        let other = IssueSource { service: "https://github.com".into(), scope: "org/other".into() };
        assert!(cache.read(&other, answer(Err("other offline".into()))).await.is_err());
        settled(&cache, &other).await;
        assert_eq!(cached(&cache, &source).await.expect("original source").1.refresh_error, None);
    }

    // The probe needs no readers: each retained source refreshes every interval and publishes a
    // change only when its issues change, or when it recovers from a failure. A retired source stops.
    #[tokio::test(start_paused = true)]
    async fn the_probe_refreshes_unread_sources_and_publishes_issue_changes() {
        let marked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cache = DispatchBoardCache::new({
            let marked = marked.clone();
            move |source: &IssueSource| marked.lock().unwrap().push(source.clone())
        });
        let mut changes = cache.changes();
        let source = source("org/probe");
        let calls = Arc::new(AtomicUsize::new(0));
        let first = board(&source, 1);
        let response = Arc::new(std::sync::Mutex::new(Ok(first.clone())));
        let load = {
            let (calls, response) = (calls.clone(), response.clone());
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::ready(response.lock().unwrap().clone())
            }
        };
        assert!(cache.read(&source, load).await.is_err());
        settled(&cache, &source).await;
        assert!(changes.has_changed().unwrap());
        changes.mark_unchanged();
        assert_eq!(*marked.lock().unwrap(), vec![source.clone()]);

        let mut later = first.clone();
        later.observed_at += chrono::Duration::seconds(60);
        *response.lock().unwrap() = Ok(later);
        next_refresh(&cache, &source).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!changes.has_changed().unwrap(), "same issues");

        *response.lock().unwrap() = Err("forge offline".into());
        next_refresh(&cache, &source).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(!changes.has_changed().unwrap(), "a failure keeps the last facts");

        *response.lock().unwrap() = Ok(first.clone());
        next_refresh(&cache, &source).await;
        assert!(changes.has_changed().unwrap(), "recovery");
        changes.mark_unchanged();

        *response.lock().unwrap() = Ok(board(&source, 2));
        next_refresh(&cache, &source).await;
        assert!(changes.has_changed().unwrap(), "new issues");
        assert_eq!(marked.lock().unwrap().len(), 3);

        cache.retain_sources(&BTreeSet::new()).await;
        for _ in 0..3 {
            tokio::time::advance(REFRESH_INTERVAL).await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 5);
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
                assert!(cache.read(&source, answer(Ok(expected.clone()))).await.is_err());
                settled(&cache, &source).await;
                let (_, actual) = cached(&cache, &source).await.expect("snapshot");
                assert_eq!(actual.source, expected.source);
                assert_eq!(actual.issues, expected.issues);
                assert_eq!(actual.pull_requests, expected.pull_requests);
            }
        });
    }
    // A tracker panic, during construction or polling, is a visible failure;
    // cold and last-good sources both recover on a later refresh.
    #[tokio::test(start_paused = true)]
    async fn loader_panics_release_refresh_and_recover() {
        fn construction_panic() -> std::future::Ready<Result<DispatchBoardRepository, String>> {
            panic!("tracker construction panic");
        }
        for last_good in [false, true] {
            for construction in [false, true] {
                let cache = DispatchBoardCache::default();
                let source = source("org/panic");
                if last_good {
                    assert!(cache.read(&source, answer(Ok(board(&source, 2)))).await.is_err());
                    settled(&cache, &source).await;
                }
                if construction {
                    let _ = cache.read(&source, construction_panic).await;
                } else {
                    let _ = cache.read(&source, || async { panic!("tracker polling panic") }).await;
                }
                if last_good {
                    next_refresh(&cache, &source).await;
                } else {
                    settled(&cache, &source).await;
                }
                let result = cached(&cache, &source).await;
                if last_good {
                    let (_, snapshot) = result.expect("last-good snapshot");
                    assert_eq!(snapshot.issues.len(), 2);
                    assert_eq!(snapshot.refresh_error.as_deref(), Some("board tracker refresh panicked"));
                } else {
                    assert!(result.expect_err("cold failure").contains("refresh panicked"));
                }
                let _ = cache.read(&source, answer(Ok(board(&source, 3)))).await;
                next_refresh(&cache, &source).await;
                let (_, snapshot) = cached(&cache, &source).await.expect("recovered");
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
        assert!(cache.read_snapshot(&source, answer(Ok(original.clone()))).await.is_err());
        settled(&cache, &source).await;
        let (revision, snapshot) = cache.read_snapshot(&source, answer(Ok(original.clone()))).await.expect("snapshot");
        let (_, duplicate) = cached(&cache, &source).await.expect("duplicate");
        assert!(Arc::ptr_eq(&snapshot, &duplicate));
        let mut refreshed = original.clone();
        refreshed.observed_at += chrono::Duration::seconds(60);
        cache.read_snapshot(&source, answer(Ok(refreshed))).await.expect("last good");
        next_refresh(&cache, &source).await;
        assert_eq!(cached(&cache, &source).await.expect("metadata refresh").0, revision);
        cache.read_snapshot(&source, answer(Err("offline".into()))).await.expect("last good");
        next_refresh(&cache, &source).await;
        let (stale_revision, stale) = cached(&cache, &source).await.expect("stale");
        assert_eq!(stale_revision, revision);
        assert_eq!(stale.issues, original.issues);
        assert_eq!(stale.refresh_error.as_deref(), Some("offline"));
        cache.retain_sources(&BTreeSet::new()).await;
        assert!(cache.read_snapshot(&source, answer(Ok(board(&source, 1)))).await.is_err());
        settled(&cache, &source).await;
        let (next_revision, replaced) = cached(&cache, &source).await.expect("readded");
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
            .read(&source, move || {
                let old = old.clone();
                let old_release = old_release.clone();
                async move {
                    old_release.acquire().await.expect("release old observation").forget();
                    Ok(old)
                }
            })
            .await
            .is_err());
        let retired = cache.shared.entries.lock().await.get(&source).expect("old entry").clone();
        cache.retain_sources(&BTreeSet::new()).await;
        assert!(cache.shared.entries.lock().await.is_empty());
        assert!(cache.read(&source, answer(Ok(board(&source, 2)))).await.is_err());
        settled(&cache, &source).await;
        release.add_permits(1);
        settled_entry(retired).await;
        let (_, snapshot) = cached(&cache, &source).await.expect("new snapshot");
        assert_eq!(snapshot.issues.len(), 2);
        assert_eq!(cache.shared.entries.lock().await.len(), 1);
        cache.retain_sources(&BTreeSet::from([source.clone()])).await;
        assert_eq!(cached(&cache, &source).await.expect("retained").1.issues, snapshot.issues);
    }
}

#[cfg(test)]
mod query_contract {
    use crate::dispatch_board::live_project_convoys_query;
    // WORKLOAD.md: retained history must not multiply decoded live-accounting
    // objects. Compare zero history to 1,000 terminal records using the real
    // indexed store; unrelated projects and missing-status convoys are included.
    #[tokio::test]
    async fn live_inventory_reads_are_flat_with_terminal_history() {
        use flotilla_resources::{Convoy, ConvoyPhase, ConvoySpec, ConvoyStatus, InputMeta};
        use flotilla_store::ResourceBackend;
        use flotilla_store_memory::InMemoryBackend;
        use flotilla_store_testkit::{assert_flat_reads, measure_reads, ReadCountsBackendExt};
        assert_flat_reads([0, 1000], |history| async move {
            let backend = ResourceBackend::new(InMemoryBackend::default()).with_read_counts();
            let convoys = backend.using::<Convoy>("ns");
            for index in 0..history + 3 {
                let record = convoys
                    .create(
                        &InputMeta::builder().name(format!("c-{index}")).build(),
                        &ConvoySpec::builder()
                            .workflow_ref("workflow".into())
                            .project_ref(if index == history + 2 { "other" } else { "p" }.into())
                            .build(),
                    )
                    .await
                    .expect("convoy");
                if index < history {
                    convoys
                        .update_status(
                            &record.metadata.name,
                            &record.metadata.resource_version,
                            &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
                        )
                        .await
                        .expect("terminal history");
                }
            }
            let (rows, reads) = measure_reads(backend.query_declared::<Convoy>("ns", &live_project_convoys_query(["p".into()]))).await;
            assert_eq!(rows.expect("live query").items.len(), 2);
            assert!(backend
                .query_declared::<Convoy>("ns", &live_project_convoys_query([]))
                .await
                .expect("empty projects")
                .items
                .is_empty());
            reads.of(&backend)
        })
        .await;
    }
}
