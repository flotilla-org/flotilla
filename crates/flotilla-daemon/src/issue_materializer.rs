//! Cancellable demand-backed materialization for the `issues{scope}` query
//! family. External provider I/O lives in per-query tasks so it cannot stall
//! the resource-store Aggregator loop.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_core::{
    aggregator_projection::AggregatorProjectionState, in_process::InProcessDaemon, providers::issue_tracker::IssueProvider,
};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    DaemonEvent, DemandBackedMetadata, IssueChangeset, IssueRef, IssueRow, IssueSource, IssueState, QueryId, QueryScope,
    ResultSetCondition, ResultSetState,
};
use futures::{future::BoxFuture, stream, FutureExt, StreamExt};
use tokio::{
    sync::{broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const PAGE_SIZE: usize = 50;
const MAX_CONCURRENT_SOURCES: usize = 8;
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[async_trait]
pub(crate) trait IssueMaterializationResolver: Send + Sync {
    async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<IssueSource>, String>;
    async fn issue_provider_for(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String>;
}

#[async_trait]
impl IssueMaterializationResolver for InProcessDaemon {
    async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<IssueSource>, String> {
        self.resolve_issue_sources(scope).await
    }

    async fn issue_provider_for(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        self.issue_provider_for_source(source).await
    }
}

enum MaterializationIntent {
    FetchMore,
    #[cfg(test)]
    Refresh,
}

struct ActiveMaterialization {
    generation: u64,
    intents: mpsc::Sender<MaterializationIntent>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl ActiveMaterialization {
    fn stop(self) {
        self.cancel.cancel();
        self.task.abort();
    }
}

pub(crate) struct IssueMaterializer {
    state: AggregatorProjectionState,
    resolver: Arc<dyn IssueMaterializationResolver>,
    event_tx: broadcast::Sender<DaemonEvent>,
    active: HashMap<QueryId, ActiveMaterialization>,
}

impl IssueMaterializer {
    pub(crate) fn new<R>(state: AggregatorProjectionState, resolver: Arc<R>, event_tx: broadcast::Sender<DaemonEvent>) -> Self
    where
        R: IssueMaterializationResolver + 'static,
    {
        Self { state, resolver, event_tx, active: HashMap::new() }
    }

    /// Reconcile complete demand, including each query's materialization
    /// generation. A generation change replaces the task even when a watch
    /// receiver coalesced the intervening stop/start edges.
    pub(crate) fn reconcile(&mut self, demanded: HashMap<QueryId, u64>) {
        let stale = self
            .active
            .iter()
            .filter(|(query, active)| demanded.get(*query) != Some(&active.generation))
            .map(|(query, _)| query.clone())
            .collect::<Vec<_>>();
        for query in stale {
            if let Some(active) = self.active.remove(&query) {
                active.stop();
            }
        }

        for (query, generation) in demanded {
            if !matches!(query, QueryId::Issues { .. }) || self.active.contains_key(&query) {
                continue;
            }
            let (intent_tx, intent_rx) = mpsc::channel(8);
            let cancel = CancellationToken::new();
            let task = tokio::spawn(run_materialization(
                query.clone(),
                generation,
                Arc::clone(&self.resolver),
                self.state.clone(),
                self.event_tx.clone(),
                cancel.clone(),
                intent_rx,
            ));
            self.active.insert(query, ActiveMaterialization { generation, intents: intent_tx, cancel, task });
        }
    }

    pub(crate) fn fetch_more(&self, query: &QueryId, generation: u64) {
        if let Some(active) = self.active.get(query).filter(|active| active.generation == generation) {
            if let Err(error) = active.intents.try_send(MaterializationIntent::FetchMore) {
                tracing::warn!(%query, %error, "could not enqueue fetch-more intent");
            }
        }
    }

    #[cfg(test)]
    fn refresh(&self, query: &QueryId) {
        self.active.get(query).expect("active materialization").intents.try_send(MaterializationIntent::Refresh).expect("enqueue refresh");
    }
}

impl Drop for IssueMaterializer {
    fn drop(&mut self) {
        for (_, active) in self.active.drain() {
            active.stop();
        }
    }
}

struct IssueSourceWindow {
    source: IssueSource,
    provider: Arc<dyn IssueProvider>,
    next_page: u32,
    has_more: bool,
    refresh_cursor: String,
    loaded_count: usize,
    rows: HashMap<IssueRef, IssueRow>,
}

struct MaterializedWindow {
    sources: Vec<IssueSourceWindow>,
    needs_full_reload: bool,
}

async fn run_materialization(
    query: QueryId,
    generation: u64,
    resolver: Arc<dyn IssueMaterializationResolver>,
    state: AggregatorProjectionState,
    event_tx: broadcast::Sender<DaemonEvent>,
    cancel: CancellationToken,
    mut intents: mpsc::Receiver<MaterializationIntent>,
) {
    let mut window = tokio::select! {
        _ = cancel.cancelled() => return,
        window = load_window(&query, generation, resolver.as_ref(), &state, &event_tx) => window,
    };
    let mut refresh = tokio::time::interval_at(tokio::time::Instant::now() + REFRESH_INTERVAL, REFRESH_INTERVAL);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            intent = intents.recv() => match intent {
                Some(MaterializationIntent::FetchMore) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = fetch_more(&query, generation, &mut window, &state, &event_tx) => {}
                    }
                }
                #[cfg(test)]
                Some(MaterializationIntent::Refresh) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = refresh_window(&query, generation, resolver.as_ref(), &mut window, &state, &event_tx) => {}
                    }
                }
                None => return,
            },
            _ = refresh.tick() => {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = refresh_window(&query, generation, resolver.as_ref(), &mut window, &state, &event_tx) => {}
                }
            }
        }
    }
}

async fn load_window(
    query: &QueryId,
    generation: u64,
    resolver: &dyn IssueMaterializationResolver,
    state: &AggregatorProjectionState,
    event_tx: &broadcast::Sender<DaemonEvent>,
) -> MaterializedWindow {
    let QueryId::Issues { scope } = query else { unreachable!("issue materializer only accepts issue queries") };
    let sources = match resolver.resolve_issue_sources(scope).await {
        Ok(sources) if !sources.is_empty() => sources,
        Ok(_) => {
            publish_window(
                query,
                generation,
                Vec::new(),
                false,
                vec![unavailable(None, "query scope has no issue source")],
                state,
                event_tx,
            );
            return MaterializedWindow { sources: Vec::new(), needs_full_reload: true };
        }
        Err(message) => {
            publish_window(query, generation, Vec::new(), false, vec![unavailable(None, message)], state, event_tx);
            return MaterializedWindow { sources: Vec::new(), needs_full_reload: true };
        }
    };

    let loaded = stream::iter(sources.into_iter().map(|source| async move {
        let provider = resolver.issue_provider_for(&source).await.map_err(|message| unavailable(Some(source.clone()), message))?;
        // Capture before the request. Re-reading changes is safe; skipping an
        // update that arrived during the request is not.
        let refresh_cursor = Utc::now().to_rfc3339();
        let page = provider
            .query(&source, &IssueQuery::default(), 1, PAGE_SIZE)
            .await
            .map_err(|message| unavailable(Some(source.clone()), message))?;
        let rows = page.items.into_iter().map(issue_row).collect::<Vec<_>>();
        let source_rows = rows.iter().cloned().map(|row| (row.reference.clone(), row)).collect::<HashMap<_, _>>();
        let loaded_count = source_rows.len();
        Ok::<_, ResultSetCondition>((
            IssueSourceWindow { source, provider, next_page: 2, has_more: page.has_more, refresh_cursor, loaded_count, rows: source_rows },
            rows,
        ))
    }))
    .buffer_unordered(MAX_CONCURRENT_SOURCES)
    .collect::<Vec<_>>()
    .await;

    let mut rows = HashMap::<IssueRef, IssueRow>::new();
    let mut windows = Vec::new();
    let mut conditions = Vec::new();
    for result in loaded {
        match result {
            Ok((window, source_rows)) => {
                rows.extend(source_rows.into_iter().map(|row| (row.reference.clone(), row)));
                windows.push(window);
            }
            Err(condition) => conditions.push(condition),
        }
    }
    let has_more = windows.iter().any(|window| window.has_more);
    let mut rows = rows.into_values().collect::<Vec<_>>();
    sort_rows(&mut rows);
    let needs_full_reload = !conditions.is_empty();
    publish_window(query, generation, rows, has_more, conditions, state, event_tx);
    MaterializedWindow { sources: windows, needs_full_reload }
}

async fn fetch_more(
    query: &QueryId,
    generation: u64,
    window: &mut MaterializedWindow,
    state: &AggregatorProjectionState,
    event_tx: &broadcast::Sender<DaemonEvent>,
) {
    let requests = window
        .sources
        .iter()
        .enumerate()
        .filter(|(_, source)| source.has_more)
        .map(|(index, source)| (index, Arc::clone(&source.provider), source.source.clone(), source.next_page))
        .collect::<Vec<_>>();
    let mut futures = Vec::<BoxFuture<'static, (usize, Result<IssueResultPage, String>)>>::with_capacity(requests.len());
    for request in requests {
        futures.push(query_page(request).boxed());
    }
    let results = stream::iter(futures).buffer_unordered(MAX_CONCURRENT_SOURCES).collect::<Vec<_>>().await;

    let mut changed = Vec::new();
    let mut conditions = Vec::new();
    for (index, result) in results {
        let source = &mut window.sources[index];
        match result {
            Ok(page) => {
                let loaded = page.items.len();
                for row in page.items.into_iter().map(issue_row) {
                    source.rows.insert(row.reference.clone(), row.clone());
                    changed.push(row);
                }
                source.loaded_count = source.loaded_count.saturating_add(loaded);
                source.next_page = source.next_page.saturating_add(1);
                source.has_more = page.has_more;
            }
            Err(message) => {
                conditions.push(unavailable(Some(source.source.clone()), message));
                window.needs_full_reload = true;
            }
        }
    }
    sort_rows(&mut changed);
    let result_state = demand_state(window.sources.iter().any(|source| source.has_more), conditions);
    // Metadata-only deltas are significant: an empty final page must still
    // clear `has_more` for clients.
    if let Some(delta) = state.apply_issue_changes(query, generation, changed, Vec::new(), result_state) {
        let _ = event_tx.send(DaemonEvent::ResultDelta(Box::new(delta)));
    }
}

async fn refresh_window(
    query: &QueryId,
    generation: u64,
    resolver: &dyn IssueMaterializationResolver,
    window: &mut MaterializedWindow,
    state: &AggregatorProjectionState,
    event_tx: &broadcast::Sender<DaemonEvent>,
) {
    if window.sources.is_empty() || window.needs_full_reload {
        *window = load_window(query, generation, resolver, state, event_tx).await;
        return;
    }

    let requests = window
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            (index, Arc::clone(&source.provider), source.source.clone(), source.refresh_cursor.clone(), Utc::now().to_rfc3339())
        })
        .collect::<Vec<_>>();
    let mut futures = Vec::<BoxFuture<'static, (usize, String, Result<IssueChangeset, String>)>>::with_capacity(requests.len());
    for request in requests {
        futures.push(changed_since(request).boxed());
    }
    let results = stream::iter(futures).buffer_unordered(MAX_CONCURRENT_SOURCES).collect::<Vec<_>>().await;

    let mut changed = HashMap::<IssueRef, IssueRow>::new();
    let mut removed = HashSet::<IssueRef>::new();
    let mut conditions = Vec::new();
    let mut overflowed = false;
    let mut boundary_invalidated = false;
    for (index, next_cursor, result) in results {
        let source = &mut window.sources[index];
        match result {
            Ok(changes) if changes.has_more => overflowed = true,
            Ok(changes) => {
                let previous = source.rows.clone();
                if source.has_more
                    && (changes.closed.iter().any(|reference| previous.contains_key(reference))
                        || changes.updated.iter().any(|issue| previous.contains_key(&issue.reference)))
                {
                    // Removing or reordering a row at a truncated boundary
                    // requires the next unseen row, which changed-since does
                    // not contain. Re-query the source window below.
                    boundary_invalidated = true;
                }
                for issue in changes.updated {
                    let reference = issue.reference.clone();
                    if issue.state == IssueState::Open {
                        source.rows.insert(reference.clone(), IssueRow { reference, issue });
                    } else {
                        source.rows.remove(&reference);
                    }
                }
                for reference in changes.closed {
                    source.rows.remove(&reference);
                }
                if source.has_more && source.rows.len() > source.loaded_count {
                    let mut retained = source.rows.values().cloned().collect::<Vec<_>>();
                    sort_rows(&mut retained);
                    retained.truncate(source.loaded_count);
                    source.rows = retained.into_iter().map(|row| (row.reference.clone(), row)).collect();
                }
                for (reference, row) in &source.rows {
                    if previous.get(reference) != Some(row) {
                        removed.remove(reference);
                        changed.insert(reference.clone(), row.clone());
                    }
                }
                for reference in previous.keys() {
                    if !source.rows.contains_key(reference) {
                        changed.remove(reference);
                        removed.insert(reference.clone());
                    }
                }
                source.refresh_cursor = next_cursor;
            }
            Err(message) => {
                conditions.push(unavailable(Some(source.source.clone()), message));
                window.needs_full_reload = true;
            }
        }
    }
    if overflowed || boundary_invalidated {
        *window = load_window(query, generation, resolver, state, event_tx).await;
        return;
    }

    let mut changed = changed.into_values().collect::<Vec<_>>();
    sort_rows(&mut changed);
    let mut removed = removed.into_iter().collect::<Vec<_>>();
    removed.sort();
    let result_state = demand_state(window.sources.iter().any(|source| source.has_more), conditions);
    if let Some(delta) = state.apply_issue_changes(query, generation, changed, removed, result_state) {
        let _ = event_tx.send(DaemonEvent::ResultDelta(Box::new(delta)));
    }
}

fn publish_window(
    query: &QueryId,
    generation: u64,
    rows: Vec<IssueRow>,
    has_more: bool,
    conditions: Vec<ResultSetCondition>,
    state: &AggregatorProjectionState,
    event_tx: &broadcast::Sender<DaemonEvent>,
) {
    if let Some(result_set) = state.replace_issues(query, generation, rows, demand_state(has_more, conditions)) {
        let _ = event_tx.send(DaemonEvent::ResultSet(Box::new(result_set)));
    }
}

fn demand_state(has_more: bool, conditions: Vec<ResultSetCondition>) -> ResultSetState {
    ResultSetState { demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more }), conditions }
}

fn unavailable(source: Option<IssueSource>, message: impl Into<String>) -> ResultSetCondition {
    ResultSetCondition::IssueSourceUnavailable { source, message: message.into() }
}

fn issue_row(issue: flotilla_protocol::Issue) -> IssueRow {
    IssueRow { reference: issue.reference.clone(), issue }
}

fn sort_rows(rows: &mut [IssueRow]) {
    rows.sort_by(|left, right| right.issue.as_of.cmp(&left.issue.as_of).then_with(|| left.reference.cmp(&right.reference)));
}

async fn query_page(
    (index, provider, source, page): (usize, Arc<dyn IssueProvider>, IssueSource, u32),
) -> (usize, Result<IssueResultPage, String>) {
    (index, provider.query(&source, &IssueQuery::default(), page, PAGE_SIZE).await)
}

async fn changed_since(
    (index, provider, source, since, next_cursor): (usize, Arc<dyn IssueProvider>, IssueSource, String, String),
) -> (usize, String, Result<IssueChangeset, String>) {
    (index, next_cursor, provider.list_changed_since(&source, &since, PAGE_SIZE).await)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use chrono::Duration as ChronoDuration;
    use flotilla_protocol::{issue_query::IssueResultPage, test_support::TestIssue, Issue, IssueChangeset, QueryCursor, RepositoryKey};
    use tokio::sync::{Mutex, Notify};
    use uuid::Uuid;

    use super::*;

    struct ScriptedProvider {
        pages: Mutex<VecDeque<IssueResultPage>>,
        changes: Mutex<VecDeque<IssueChangeset>>,
        seen_since: Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        fn new(pages: Vec<IssueResultPage>, changes: Vec<IssueChangeset>) -> Self {
            Self { pages: Mutex::new(pages.into()), changes: Mutex::new(changes.into()), seen_since: Mutex::new(Vec::new()) }
        }
    }

    #[async_trait]
    impl IssueProvider for ScriptedProvider {
        fn supports(&self, _source: &IssueSource) -> bool {
            true
        }

        async fn query(&self, source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
            let mut page = self.pages.lock().await.pop_front().expect("scripted issue page");
            for issue in &mut page.items {
                issue.reference.source = source.clone();
            }
            Ok(page)
        }

        async fn fetch_by_id(&self, _reference: &IssueRef) -> Result<Issue, String> {
            unreachable!("not used by materialization")
        }

        async fn list_changed_since(&self, source: &IssueSource, since: &str, _count: usize) -> Result<IssueChangeset, String> {
            assert!(!since.is_empty());
            self.seen_since.lock().await.push(since.to_string());
            let mut changes =
                self.changes.lock().await.pop_front().unwrap_or(IssueChangeset { updated: vec![], closed: vec![], has_more: false });
            for issue in &mut changes.updated {
                issue.reference.source = source.clone();
            }
            for reference in &mut changes.closed {
                reference.source = source.clone();
            }
            Ok(changes)
        }

        async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
            unreachable!("not used by materialization")
        }
    }

    struct FixedResolver {
        sources: Vec<IssueSource>,
        provider: Arc<dyn IssueProvider>,
    }

    struct ScopeResolver {
        provider: Arc<dyn IssueProvider>,
    }

    #[async_trait]
    impl IssueMaterializationResolver for ScopeResolver {
        async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<IssueSource>, String> {
            let QueryScope::Repository(RepositoryKey(name)) = scope else { unreachable!("repository scope") };
            Ok(vec![IssueSource { service: "https://issues.example".into(), scope: name.clone() }])
        }

        async fn issue_provider_for(&self, _source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
            Ok(Arc::clone(&self.provider))
        }
    }

    struct BlockingProvider {
        slow_started: Notify,
        release_slow: Notify,
        slow_cancelled: Notify,
    }

    struct CancellationGuard<'a> {
        cancelled: &'a Notify,
        completed: bool,
    }

    impl Drop for CancellationGuard<'_> {
        fn drop(&mut self) {
            if !self.completed {
                self.cancelled.notify_one();
            }
        }
    }

    #[async_trait]
    impl IssueProvider for BlockingProvider {
        fn supports(&self, _source: &IssueSource) -> bool {
            true
        }

        async fn query(&self, source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
            if source.scope == "slow" {
                self.slow_started.notify_one();
                let mut guard = CancellationGuard { cancelled: &self.slow_cancelled, completed: false };
                self.release_slow.notified().await;
                guard.completed = true;
            }
            let mut issue = issue(&source.scope);
            issue.reference.source = source.clone();
            Ok(IssueResultPage { items: vec![issue], total: Some(1), has_more: false })
        }

        async fn fetch_by_id(&self, _reference: &IssueRef) -> Result<Issue, String> {
            unreachable!("not used by materialization")
        }

        async fn list_changed_since(&self, _source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
            Ok(IssueChangeset { updated: vec![], closed: vec![], has_more: false })
        }

        async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
            unreachable!("not used by materialization")
        }
    }

    #[async_trait]
    impl IssueMaterializationResolver for FixedResolver {
        async fn resolve_issue_sources(&self, _scope: &QueryScope) -> Result<Vec<IssueSource>, String> {
            Ok(self.sources.clone())
        }

        async fn issue_provider_for(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
            assert!(self.sources.contains(source));
            Ok(Arc::clone(&self.provider))
        }
    }

    fn issue(id: &str) -> Issue {
        TestIssue::new(id).id(id).build()
    }

    fn page(ids: &[&str], has_more: bool) -> IssueResultPage {
        IssueResultPage { items: ids.iter().map(|id| issue(id)).collect(), total: None, has_more }
    }

    fn repository_query(name: &str) -> QueryId {
        QueryId::Issues { scope: QueryScope::Repository(RepositoryKey(name.into())) }
    }

    fn subscribe(state: &AggregatorProjectionState, query: &QueryId) -> u64 {
        state.replace_subscriber(Uuid::new_v4(), &[QueryCursor { query: query.clone(), since: None }]);
        *state.subscribe_demand().borrow().get(query).expect("query generation")
    }

    fn generation(state: &AggregatorProjectionState, query: &QueryId) -> u64 {
        *state.subscribe_demand().borrow().get(query).expect("query generation")
    }

    fn manager(
        state: &AggregatorProjectionState,
        query: &QueryId,
        sources: Vec<IssueSource>,
        provider: Arc<dyn IssueProvider>,
    ) -> (IssueMaterializer, broadcast::Receiver<DaemonEvent>) {
        let generation = subscribe(state, query);
        let resolver = Arc::new(FixedResolver { sources, provider });
        let (event_tx, event_rx) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, event_tx);
        materializer.reconcile(HashMap::from([(query.clone(), generation)]));
        (materializer, event_rx)
    }

    async fn next_event(events: &mut broadcast::Receiver<DaemonEvent>) -> DaemonEvent {
        tokio::time::timeout(Duration::from_secs(1), events.recv()).await.expect("materialization event timeout").expect("event channel")
    }

    #[tokio::test]
    async fn repository_demand_loads_a_source_qualified_first_page() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("repo_widget");
        let source = IssueSource { service: "https://issues.example".into(), scope: "widgets/api".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["WIDGET-123"], false)], vec![]));
        let (_materializer, mut events) = manager(&state, &query, vec![source.clone()], provider);

        assert!(matches!(next_event(&mut events).await, DaemonEvent::ResultSet(set) if set.query() == query));
        let result = state.result_set_for(&query).await.expect("live issue result set");
        assert_eq!(result.rows.as_issues().expect("issue rows")[0].reference, IssueRef { source, id: "WIDGET-123".into() });
        assert!(!result.state.demand.expect("demand metadata").has_more);
        assert!(result.state.conditions.is_empty());
    }

    #[tokio::test]
    async fn project_demand_unions_constituent_source_windows() {
        let state = AggregatorProjectionState::new();
        let query = QueryId::Issues { scope: QueryScope::Project { namespace: "flotilla".into(), name: "platform".into() } };
        let source_a = IssueSource { service: "https://issues.example".into(), scope: "widgets/api".into() };
        let source_b = IssueSource { service: "https://issues.example".into(), scope: "widgets/ui".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["WIDGET-123"], false), page(&["WIDGET-123"], false)], vec![]));
        let (_materializer, mut events) = manager(&state, &query, vec![source_a.clone(), source_b.clone()], provider);

        let _ = next_event(&mut events).await;
        let result = state.result_set_for(&query).await.expect("project issue result set");
        let references = result.rows.as_issues().expect("issue rows").iter().map(|row| row.reference.clone()).collect::<HashSet<_>>();
        assert_eq!(
            references,
            HashSet::from(
                [IssueRef { source: source_a, id: "WIDGET-123".into() }, IssueRef { source: source_b, id: "WIDGET-123".into() },]
            )
        );
    }

    #[tokio::test]
    async fn fetch_more_appends_rows_and_updates_metadata() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("repo_linear");
        let source = IssueSource { service: "https://linear.example".into(), scope: "widgets".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["LINEAR-A"], true), page(&["LINEAR-B"], false)], vec![]));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider);
        let _ = next_event(&mut events).await;
        let current_generation = generation(&state, &query);

        materializer.fetch_more(&query, current_generation.saturating_add(1));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), events.recv()).await.is_err(),
            "a fetch-more intent from another materialization lifetime must be ignored"
        );

        materializer.fetch_more(&query, current_generation);

        let DaemonEvent::ResultDelta(delta) = next_event(&mut events).await else { panic!("fetch-more must emit a delta") };
        assert_eq!(delta.changes.as_issues().expect("issue changes")[0].reference.id, "LINEAR-B");
        assert!(!delta.state.and_then(|state| state.demand).expect("demand metadata").has_more);
        assert_eq!(state.result_set_for(&query).await.expect("extended window").rows.as_issues().expect("issue rows").len(), 2);
    }

    #[tokio::test]
    async fn empty_final_page_emits_metadata_only_delta_that_clears_has_more() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("repo_empty_final");
        let source = IssueSource { service: "https://issues.example".into(), scope: "widgets/empty".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["ONLY"], true), page(&[], false)], vec![]));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider);
        let _ = next_event(&mut events).await;

        materializer.fetch_more(&query, generation(&state, &query));

        let DaemonEvent::ResultDelta(delta) = next_event(&mut events).await else { panic!("fetch-more must emit a delta") };
        assert!(delta.changes.as_issues().expect("issue changes").is_empty());
        assert!(!delta.state.and_then(|state| state.demand).expect("demand metadata").has_more);
    }

    #[tokio::test]
    async fn incremental_refresh_updates_and_evicts_rows() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("repo_refresh");
        let source = IssueSource { service: "https://issues.example".into(), scope: "refresh/repo".into() };
        let changes = IssueChangeset {
            updated: vec![issue("NEW-10")],
            closed: vec![IssueRef { source: source.clone(), id: "OLD-9".into() }],
            has_more: false,
        };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["OLD-9"], false)], vec![changes]));
        let (materializer, mut events) = manager(&state, &query, vec![source.clone()], provider);
        let _ = next_event(&mut events).await;

        materializer.refresh(&query);

        let DaemonEvent::ResultDelta(delta) = next_event(&mut events).await else { panic!("refresh must emit a delta") };
        assert_eq!(delta.changes.as_issues().expect("updated issues")[0].reference.id, "NEW-10");
        assert_eq!(delta.changes.removed_issues().expect("closed issues"), &[IssueRef { source, id: "OLD-9".into() }]);
        assert_eq!(
            state
                .result_set_for(&query)
                .await
                .expect("refreshed window")
                .rows
                .as_issues()
                .expect("issue rows")
                .iter()
                .map(|row| row.reference.id.as_str())
                .collect::<Vec<_>>(),
            vec!["NEW-10"]
        );
    }

    #[tokio::test]
    async fn incremental_refresh_preserves_the_loaded_page_boundary() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("repo_boundary");
        let source = IssueSource { service: "https://issues.example".into(), scope: "boundary/repo".into() };
        let base = Utc::now();
        let initial = (0..PAGE_SIZE)
            .map(|index| {
                let mut issue = issue(&format!("ISSUE-{index:02}"));
                issue.as_of = base - ChronoDuration::seconds(index as i64);
                issue
            })
            .collect::<Vec<_>>();
        let mut newest = issue("NEWEST");
        newest.as_of = base + ChronoDuration::seconds(1);
        let provider = Arc::new(ScriptedProvider::new(vec![IssueResultPage { items: initial, total: Some(51), has_more: true }], vec![
            IssueChangeset { updated: vec![newest], closed: vec![], has_more: false },
        ]));
        let (materializer, mut events) = manager(&state, &query, vec![source.clone()], provider);
        let _ = next_event(&mut events).await;

        materializer.refresh(&query);

        let DaemonEvent::ResultDelta(delta) = next_event(&mut events).await else { panic!("refresh must emit a delta") };
        assert_eq!(delta.changes.removed_issues().expect("boundary eviction"), &[IssueRef { source, id: "ISSUE-49".into() }]);
        let result = state.result_set_for(&query).await.expect("bounded window");
        let rows = result.rows.as_issues().expect("issue rows");
        assert_eq!(rows.len(), PAGE_SIZE);
        assert_eq!(rows[0].reference.id, "NEWEST");
        assert!(rows.iter().all(|row| row.reference.id != "ISSUE-49"));
    }

    #[tokio::test]
    async fn boundary_removal_reloads_to_promote_the_next_unseen_row() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("repo_boundary_removal");
        let source = IssueSource { service: "https://issues.example".into(), scope: "boundary/removal".into() };
        let initial = (0..PAGE_SIZE).map(|index| issue(&format!("ISSUE-{index:02}"))).collect::<Vec<_>>();
        let reloaded = (1..=PAGE_SIZE).map(|index| issue(&format!("ISSUE-{index:02}"))).collect::<Vec<_>>();
        let provider = Arc::new(ScriptedProvider::new(
            vec![IssueResultPage { items: initial, total: Some(51), has_more: true }, IssueResultPage {
                items: reloaded,
                total: Some(50),
                has_more: false,
            }],
            vec![IssueChangeset {
                updated: vec![],
                closed: vec![IssueRef { source: source.clone(), id: "ISSUE-00".into() }],
                has_more: false,
            }],
        ));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider);
        let _ = next_event(&mut events).await;

        materializer.refresh(&query);

        assert!(matches!(next_event(&mut events).await, DaemonEvent::ResultSet(set) if set.query() == query));
        let result = state.result_set_for(&query).await.expect("reloaded boundary");
        let rows = result.rows.as_issues().expect("issue rows");
        assert_eq!(rows.len(), PAGE_SIZE);
        assert!(rows.iter().any(|row| row.reference.id == "ISSUE-50"));
        assert!(rows.iter().all(|row| row.reference.id != "ISSUE-00"));
    }

    #[tokio::test]
    async fn closed_only_refresh_advances_the_conservative_cursor() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("repo_closed_cursor");
        let source = IssueSource { service: "https://issues.example".into(), scope: "closed/repo".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["CLOSED"], false)], vec![
            IssueChangeset { updated: vec![], closed: vec![IssueRef { source: source.clone(), id: "CLOSED".into() }], has_more: false },
            IssueChangeset { updated: vec![], closed: vec![], has_more: false },
        ]));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider.clone());
        let _ = next_event(&mut events).await;

        materializer.refresh(&query);
        let _ = next_event(&mut events).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        materializer.refresh(&query);
        let _ = next_event(&mut events).await;

        let seen = provider.seen_since.lock().await;
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0], seen[1], "successful closed-only refresh must advance its cursor");
    }

    #[tokio::test]
    async fn slow_provider_io_does_not_block_other_query_materializations() {
        let state = AggregatorProjectionState::new();
        let slow = repository_query("slow");
        let fast = repository_query("fast");
        let slow_generation = subscribe(&state, &slow);
        let fast_generation = subscribe(&state, &fast);
        let provider =
            Arc::new(BlockingProvider { slow_started: Notify::new(), release_slow: Notify::new(), slow_cancelled: Notify::new() });
        let resolver = Arc::new(ScopeResolver { provider: provider.clone() });
        let (event_tx, mut events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, event_tx);
        materializer.reconcile(HashMap::from([(slow.clone(), slow_generation)]));
        tokio::time::timeout(Duration::from_secs(1), provider.slow_started.notified()).await.expect("slow provider started");

        materializer.reconcile(HashMap::from([(slow.clone(), slow_generation), (fast.clone(), fast_generation)]));

        assert!(matches!(next_event(&mut events).await, DaemonEvent::ResultSet(set) if set.query() == fast));
        assert_eq!(
            state.result_set_for(&fast).await.expect("fast materialization").rows.as_issues().expect("issue rows")[0].reference.id,
            "fast"
        );
        provider.release_slow.notify_one();
    }

    #[tokio::test]
    async fn coalesced_generation_replacement_cancels_the_old_provider_request() {
        let state = AggregatorProjectionState::new();
        let query = repository_query("slow");
        let provider =
            Arc::new(BlockingProvider { slow_started: Notify::new(), release_slow: Notify::new(), slow_cancelled: Notify::new() });
        let resolver = Arc::new(ScopeResolver { provider: provider.clone() });
        let (event_tx, _events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state, resolver, event_tx);
        materializer.reconcile(HashMap::from([(query.clone(), 1)]));
        tokio::time::timeout(Duration::from_secs(1), provider.slow_started.notified()).await.expect("generation one started");

        materializer.reconcile(HashMap::from([(query.clone(), 2)]));

        tokio::time::timeout(Duration::from_secs(1), provider.slow_cancelled.notified()).await.expect("generation one cancelled");
        tokio::time::timeout(Duration::from_secs(1), provider.slow_started.notified()).await.expect("generation two started");
        assert_eq!(materializer.active.get(&query).expect("replacement task").generation, 2);
        provider.release_slow.notify_one();
    }

    #[tokio::test]
    async fn stale_fetch_more_intent_is_not_delivered_to_a_recreated_lifetime() {
        let state = AggregatorProjectionState::new();
        let subscriber = Uuid::new_v4();
        let query = repository_query("recreated");
        let source = IssueSource { service: "https://issues.example".into(), scope: "recreated".into() };
        let provider = Arc::new(ScriptedProvider::new(
            vec![page(&["OLD-LIFETIME"], true), page(&["NEW-LIFETIME"], true), page(&["STALE-PAGE"], false)],
            vec![],
        ));
        let resolver = Arc::new(FixedResolver { sources: vec![source], provider: provider.clone() });
        let (event_tx, mut events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, event_tx);
        state.replace_subscriber(subscriber, &[QueryCursor { query: query.clone(), since: None }]);
        let old_generation = generation(&state, &query);
        materializer.reconcile(HashMap::from([(query.clone(), old_generation)]));
        let _ = next_event(&mut events).await;

        state.remove_subscriber(subscriber);
        state.replace_subscriber(subscriber, &[QueryCursor { query: query.clone(), since: None }]);
        let new_generation = generation(&state, &query);
        materializer.reconcile(HashMap::from([(query.clone(), new_generation)]));
        let _ = next_event(&mut events).await;

        materializer.fetch_more(&query, old_generation);

        assert!(tokio::time::timeout(Duration::from_millis(50), events.recv()).await.is_err());
        assert_eq!(
            state.result_set_for(&query).await.expect("new window").rows.as_issues().expect("issue rows")[0].reference.id,
            "NEW-LIFETIME"
        );
        assert_eq!(provider.pages.lock().await.len(), 1, "stale page was not requested");
    }
}
