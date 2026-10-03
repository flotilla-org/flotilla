//! Cancellable demand-backed materialization for the `issues{scope}` query
//! family. External provider I/O lives in per-query tasks so it cannot stall
//! the resource-store Aggregator loop.

use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_core::{
    aggregator_projection::AggregatorProjectionState,
    event_sink::EventSink,
    in_process::InProcessDaemon,
    providers::{
        github_api::{core_rate_limit_reset, rate_limit_reset},
        issue_tracker::IssueProvider,
    },
};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    DaemonEvent, DemandBackedMetadata, IssueChangeset, IssueRef, IssueRow, IssueSource, IssueState, QueryId, QueryScope,
    ResultSetCondition, ResultSetState,
};
use flotilla_resources::{ConditionValue, HostCondition, ResolvedIssueSourceBinding};
use futures::{stream, StreamExt};
#[cfg(test)]
use tokio::sync::broadcast;
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const PAGE_SIZE: usize = 50;
const MAX_CONCURRENT_SOURCES: usize = 8;
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const GOVERNED_REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const QUIET_VIEWED_INTERVAL: Duration = Duration::from_secs(120);
const QUIET_GOVERNED_INTERVAL: Duration = Duration::from_secs(300);
const MAX_RATE_LIMIT_JITTER: Duration = Duration::from_secs(5);

fn is_github_source(source: &IssueSource) -> bool {
    matches!(source.service.trim_end_matches('/'), "github" | "github.com" | "https://github.com")
}

#[async_trait]
pub(crate) trait IssueMaterializationResolver: Send + Sync {
    async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String>;
    async fn issue_provider_for(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String>;
}

#[async_trait]
impl IssueMaterializationResolver for InProcessDaemon {
    async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String> {
        self.resolve_issue_source_bindings(scope).await
    }

    async fn issue_provider_for(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
        self.issue_provider_for_source(source).await
    }
}

enum MaterializationIntent {
    FetchMore,
    Refilter,
    #[cfg(test)]
    Refresh,
}

struct ActiveMaterialization {
    generation: u64,
    intents: mpsc::Sender<MaterializationIntent>,
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

struct MaterializationContext {
    resolver: Arc<dyn IssueMaterializationResolver>,
    state: AggregatorProjectionState,
    event_sink: Arc<dyn EventSink>,
    shared_refresh: Arc<SharedIssueRefresh>,
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
    event_sink: Arc<dyn EventSink>,
    active: HashMap<QueryId, ActiveMaterialization>,
    shared_refresh: Arc<SharedIssueRefresh>,
}

#[derive(Default)]
struct SharedIssueRefresh {
    sources: StdMutex<HashMap<IssueSource, Arc<SourceRefresh>>>,
    health: IssuePollingHealth,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct IssuePollingHealth {
    backoff: Arc<StdMutex<BudgetBackoff>>,
}

#[derive(Debug, Default)]
struct BudgetBackoff {
    current: Option<(DateTime<Utc>, String)>,
}

impl IssuePollingHealth {
    pub(crate) fn note(&self, message: &str) {
        if let Some(reset) = core_rate_limit_reset(message) {
            let mut backoff = self.backoff.lock().expect("issue polling health lock poisoned");
            if backoff.current.as_ref().is_none_or(|(current, _)| reset > *current) {
                backoff.current = Some((reset, message.to_string()));
            }
        }
    }

    fn active_error(&self) -> Option<String> {
        self.backoff
            .lock()
            .expect("issue polling health lock poisoned")
            .current
            .as_ref()
            .and_then(|(reset, message)| (*reset > Utc::now()).then(|| message.clone()))
    }

    pub(crate) fn condition(&self) -> Option<HostCondition> {
        let message = self.active_error()?;
        Some(
            HostCondition::builder()
                .condition_type("Forge/GitHubRateBudget")
                .value(ConditionValue::False)
                .reason("LowRemainingBudget")
                .message(message)
                .observed_at(Utc::now())
                .blocks_readiness(false)
                .build(),
        )
    }
}

#[derive(bon::Builder)]
struct SourceRefresh {
    provider: StdMutex<Arc<dyn IssueProvider>>,
    cursors: StdMutex<HashMap<QueryId, DateTime<Utc>>>,
    last: Mutex<Option<SourceRefreshResult>>,
    pages: Mutex<Vec<SourcePage>>,
    activity: StdMutex<SourceActivity>,
}

#[derive(Default)]
struct SourceActivity {
    quiet_polls: u32,
    sampled_at: Option<tokio::time::Instant>,
    demand: Option<SourceDemand>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceDemand {
    Viewed,
    Governed,
}

impl SourceRefresh {
    // Call while holding pages so replacement and invalidation are atomic.
    fn replace_provider(&self, provider: &Arc<dyn IssueProvider>, pages: &mut Vec<SourcePage>) -> bool {
        let mut current = self.provider.lock().expect("source provider lock poisoned");
        let replaced = !Arc::ptr_eq(&current, provider);
        if replaced {
            pages.clear();
            *current = provider.clone();
        }
        replaced
    }

    // Explicit issue-query demand is a viewed source; implicit fleet-awareness
    // windows are governed demand. A source uses the strongest live demand.
    fn interval(&self, state: &AggregatorProjectionState) -> Duration {
        let viewed = state.subscribed_queries();
        let is_viewed = self.cursors.lock().expect("shared issue cursors lock poisoned").keys().any(|query| viewed.contains(query));
        let (base, cap) =
            if is_viewed { (REFRESH_INTERVAL, QUIET_VIEWED_INTERVAL) } else { (GOVERNED_REFRESH_INTERVAL, QUIET_GOVERNED_INTERVAL) };
        let demand = if is_viewed { SourceDemand::Viewed } else { SourceDemand::Governed };
        let mut activity = self.activity.lock().expect("source activity lock poisoned");
        if demand == SourceDemand::Viewed && activity.demand == Some(SourceDemand::Governed) {
            activity.quiet_polls = 0;
        }
        activity.demand = Some(demand);
        base.saturating_mul(1 << activity.quiet_polls.min(4)).min(cap)
    }

    fn note_activity(&self, active: bool) {
        let mut activity = self.activity.lock().expect("source activity lock poisoned");
        if active {
            activity.quiet_polls = 0;
            activity.sampled_at = Some(tokio::time::Instant::now());
        } else if activity.sampled_at.is_none_or(|sample| sample.elapsed() >= REFRESH_INTERVAL) {
            // Several filtered windows can observe the same quiet source in one
            // cycle. Count that cycle once rather than multiplying backoff.
            activity.quiet_polls = activity.quiet_polls.saturating_add(1);
            activity.sampled_at = Some(tokio::time::Instant::now());
        }
    }
}

#[derive(bon::Builder)]
struct SourcePage {
    params: IssueQuery,
    page: u32,
    fetched_at: tokio::time::Instant,
    cursor: String,
    interval: Duration,
    result: IssueResultPage,
}

#[derive(bon::Builder)]
struct SourceRefreshResult {
    fetched_at: tokio::time::Instant,
    since: DateTime<Utc>,
    next_cursor: DateTime<Utc>,
    result: Result<IssueChangeset, String>,
}

impl SharedIssueRefresh {
    fn register(&self, source: &IssueSource, query: &QueryId, cursor: &str, provider: &Arc<dyn IssueProvider>) {
        let mut sources = self.sources.lock().expect("shared issue sources lock poisoned");
        let entry = sources.entry(source.clone()).or_insert_with(|| {
            Arc::new(
                SourceRefresh::builder()
                    .provider(StdMutex::new(Arc::clone(provider)))
                    .cursors(StdMutex::new(HashMap::new()))
                    .last(Mutex::new(None))
                    .pages(Mutex::new(Vec::new()))
                    .activity(StdMutex::new(SourceActivity::default()))
                    .build(),
            )
        });
        let cursor = cursor.parse().expect("materializer creates RFC 3339 cursors");
        entry.cursors.lock().expect("shared issue cursors lock poisoned").insert(query.clone(), cursor);
    }

    async fn page(
        &self,
        source: &IssueSource,
        params: &IssueQuery,
        page: u32,
        provider: &Arc<dyn IssueProvider>,
        state: &AggregatorProjectionState,
    ) -> Result<(String, IssueResultPage), String> {
        let entry = self
            .sources
            .lock()
            .expect("shared issue sources lock poisoned")
            .get(source)
            .cloned()
            .ok_or_else(|| format!("issue source {} is no longer registered", source.scope))?;
        let mut pages = entry.pages.lock().await;
        entry.replace_provider(provider, &mut pages);
        let interval = entry.interval(state);
        pages.retain(|cached| {
            (cached.params == *params && cached.page == page) || cached.fetched_at.elapsed() < cached.interval.min(interval)
        });
        let previous = pages.iter().find(|cached| cached.params == *params && cached.page == page).map(|cached| cached.result.clone());
        for cached in pages.iter() {
            if cached.page != page || cached.fetched_at.elapsed() >= cached.interval.min(interval) {
                continue;
            }
            if cached.params == *params {
                return Ok((cached.cursor.clone(), cached.result.clone()));
            }
            // A complete unfiltered page is a canonical source snapshot. A
            // truncated page cannot prove a label query's window is complete.
            if page == 1
                && cached.params == IssueQuery::default()
                && !cached.result.has_more
                && params.search.is_none()
                && params.match_fields.is_empty()
            {
                let mut result = cached.result.clone();
                result.items.retain(|issue| {
                    params.label.as_ref().is_none_or(|label| issue.labels.iter().any(|candidate| candidate.eq_ignore_ascii_case(label)))
                });
                result.total = None;
                return Ok((cached.cursor.clone(), result));
            }
        }
        let cursor = Utc::now().to_rfc3339();
        let result = provider.query(source, params, page, PAGE_SIZE).await?;
        if let Some(mut previous) = previous {
            let mut current = result.clone();
            // A fresh observation timestamp is not issue activity.
            for item in previous.items.iter_mut().chain(current.items.iter_mut()) {
                item.observed_at = None;
            }
            entry.note_activity(previous != current);
        } else if page > 1 {
            entry.note_activity(true);
        } else {
            entry.activity.lock().expect("source activity lock poisoned").sampled_at.get_or_insert_with(tokio::time::Instant::now);
        }
        pages.retain(|cached| cached.params != *params || cached.page != page);
        pages.push(
            SourcePage::builder()
                .params(params.clone())
                .page(page)
                .fetched_at(tokio::time::Instant::now())
                .cursor(cursor.clone())
                .interval(entry.interval(state))
                .result(result.clone())
                .build(),
        );
        Ok((cursor, result))
    }

    fn unregister(&self, query: &QueryId) {
        let mut sources = self.sources.lock().expect("shared issue sources lock poisoned");
        sources.retain(|_, entry| {
            let mut cursors = entry.cursors.lock().expect("shared issue cursors lock poisoned");
            cursors.remove(query);
            !cursors.is_empty()
        });
    }

    fn unregister_source(&self, source: &IssueSource, query: &QueryId) {
        let mut sources = self.sources.lock().expect("shared issue sources lock poisoned");
        if let Some(entry) = sources.get(source) {
            let mut cursors = entry.cursors.lock().expect("shared issue cursors lock poisoned");
            cursors.remove(query);
            if cursors.is_empty() {
                drop(cursors);
                sources.remove(source);
            }
        }
    }

    fn advance(&self, source: &IssueSource, query: &QueryId, cursor: &str) {
        if let Some(entry) = self.sources.lock().expect("shared issue sources lock poisoned").get(source) {
            let cursor = cursor.parse().expect("materializer creates RFC 3339 cursors");
            entry.cursors.lock().expect("shared issue cursors lock poisoned").insert(query.clone(), cursor);
        }
    }

    async fn changed_since(
        &self,
        source: &IssueSource,
        query: &QueryId,
        since: &str,
        resolver: &dyn IssueMaterializationResolver,
        state: &AggregatorProjectionState,
    ) -> (String, Result<IssueChangeset, String>) {
        if is_github_source(source) {
            if let Some(message) = self.health.active_error() {
                return (since.to_string(), Err(message));
            }
        }
        let Some(entry) = self.sources.lock().expect("shared issue sources lock poisoned").get(source).cloned() else {
            return (since.to_string(), Err(format!("issue source {} is no longer registered", source.scope)));
        };
        let since_time = match since.parse::<DateTime<Utc>>() {
            Ok(time) => time,
            Err(error) => return (since.to_string(), Err(format!("invalid issue refresh cursor: {error}"))),
        };
        let provider = match resolver.issue_provider_for(source).await {
            Ok(provider) => provider,
            Err(message) => return (since.to_string(), Err(message)),
        };
        let mut last = entry.last.lock().await;
        if entry.replace_provider(&provider, &mut *entry.pages.lock().await) {
            *last = None;
        }
        if let Some(cached) = last.as_ref().filter(|cached| cached.fetched_at.elapsed() < entry.interval(state)) {
            if since_time >= cached.since {
                if since_time >= cached.next_cursor {
                    return (since.to_string(), Ok(IssueChangeset { updated: vec![], closed: vec![], has_more: false }));
                }
                if since_time == cached.since {
                    return (cached.next_cursor.to_rfc3339(), cached.result.clone());
                }
                // This query loaded a newer initial page than the oldest query.
                // Updates carry timestamps and can be filtered; closures do not.
                // Keep its cursor so the next source poll can verify closures.
                let mut next_cursor = cached.next_cursor.to_rfc3339();
                let filtered = cached.result.clone().map(|mut changes| {
                    changes.updated.retain(|issue| issue.as_of >= since_time);
                    if !changes.closed.is_empty() {
                        next_cursor = since.to_string();
                    }
                    changes.closed.clear();
                    changes
                });
                return (next_cursor, filtered);
            }
        }
        let oldest = entry.cursors.lock().expect("shared issue cursors lock poisoned").values().min().copied().unwrap_or(since_time);
        let next_cursor = Utc::now();
        let result = provider.list_changed_since(source, &oldest.to_rfc3339(), PAGE_SIZE).await;
        // Errors reset quiet backoff for prompt recovery; forge reset-time
        // health backoff still suppresses requests when quota is exhausted.
        entry.note_activity(
            result.as_ref().map_or(true, |changes| !changes.updated.is_empty() || !changes.closed.is_empty() || changes.has_more),
        );
        // A new observation invalidates pages once; all query reloads caused by
        // that observation then share the replacement pages.
        entry.pages.lock().await.clear();
        if let Err(message) = &result {
            self.health.note(message);
        }
        *last = Some(
            SourceRefreshResult::builder()
                .fetched_at(tokio::time::Instant::now())
                .since(oldest)
                .next_cursor(next_cursor)
                .result(result.clone())
                .build(),
        );
        tracing::debug!(%query, source = %source.scope, "shared issue source refresh");
        (next_cursor.to_rfc3339(), result)
    }
}

impl IssueMaterializer {
    pub(crate) fn new<R>(state: AggregatorProjectionState, resolver: Arc<R>, event_sink: Arc<dyn EventSink>) -> Self
    where
        R: IssueMaterializationResolver + 'static,
    {
        Self { state, resolver, event_sink, active: HashMap::new(), shared_refresh: Arc::new(SharedIssueRefresh::default()) }
    }

    pub(crate) fn with_polling_health(mut self, health: IssuePollingHealth) -> Self {
        self.shared_refresh = Arc::new(SharedIssueRefresh { sources: StdMutex::new(HashMap::new()), health });
        self
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
                self.shared_refresh.unregister(&query);
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
                MaterializationContext {
                    resolver: Arc::clone(&self.resolver),
                    state: self.state.clone(),
                    event_sink: self.event_sink.clone(),
                    shared_refresh: Arc::clone(&self.shared_refresh),
                },
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

    pub(crate) fn refilter_active_queries(&self) {
        for (query, active) in &self.active {
            if let Err(error) = active.intents.try_send(MaterializationIntent::Refilter) {
                tracing::warn!(%query, %error, "could not enqueue issue refilter intent");
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
    query_params: IssueQuery,
    next_page: u32,
    has_more: bool,
    refresh_cursor: String,
    loaded_count: usize,
    rows: HashMap<IssueRef, IssueRow>,
}

struct MaterializedWindow {
    sources: Vec<IssueSourceWindow>,
    needs_full_reload: bool,
    conditions: Vec<ResultSetCondition>,
    suspended_until: Option<tokio::time::Instant>,
}

impl MaterializedWindow {
    fn rows(&self) -> Vec<IssueRow> {
        self.sources.iter().flat_map(|source| source.rows.values().cloned()).collect()
    }

    fn has_more(&self) -> bool {
        self.sources.iter().any(|source| source.has_more)
    }
}

fn suspension_deadline(query: &QueryId, reset: chrono::DateTime<Utc>) -> tokio::time::Instant {
    let until_reset = reset.signed_duration_since(Utc::now()).to_std().unwrap_or(Duration::ZERO);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    query.hash(&mut hasher);
    let jitter = Duration::from_secs(1 + hasher.finish() % MAX_RATE_LIMIT_JITTER.as_secs());
    tokio::time::Instant::now() + until_reset + jitter
}

fn suspended_until(query: &QueryId, message: &str) -> Option<tokio::time::Instant> {
    let reset = rate_limit_reset(message)?;
    let deadline = suspension_deadline(query, reset);
    tracing::warn!(scope = %query, reset_at = %reset, resume_at = ?deadline, "suspending forge polling after GitHub rate limit");
    Some(deadline)
}

async fn run_materialization(
    query: QueryId,
    generation: u64,
    context: MaterializationContext,
    cancel: CancellationToken,
    mut intents: mpsc::Receiver<MaterializationIntent>,
) {
    let MaterializationContext { resolver, state, event_sink, shared_refresh } = context;
    let mut window = tokio::select! {
        _ = cancel.cancelled() => return,
        window = load_window(&query, generation, resolver.as_ref(), &shared_refresh, &state, event_sink.as_ref()) => window,
    };
    let mut refresh = tokio::time::interval_at(tokio::time::Instant::now() + REFRESH_INTERVAL, REFRESH_INTERVAL);
    loop {
        let suspended = window.suspended_until;
        tokio::select! {
            _ = cancel.cancelled() => return,
            intent = intents.recv() => match intent {
                Some(MaterializationIntent::FetchMore) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = fetch_more(&query, generation, resolver.as_ref(), &shared_refresh, &mut window, &state, event_sink.as_ref()) => {}
                    }
                }
                Some(MaterializationIntent::Refilter) => {
                    publish_loaded_window(&query, generation, window.rows(), window.has_more(), window.conditions.clone(), &state, event_sink.as_ref()).await;
                }
                #[cfg(test)]
                Some(MaterializationIntent::Refresh) => {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = refresh_window(&query, generation, resolver.as_ref(), &shared_refresh, &mut window, &state, event_sink.as_ref()) => {}
                    }
                }
                None => return,
            },
            _ = refresh.tick(), if suspended.is_none_or(|deadline| deadline <= tokio::time::Instant::now()) => {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = refresh_window(&query, generation, resolver.as_ref(), &shared_refresh, &mut window, &state, event_sink.as_ref()) => {}
                }
            },
            _ = tokio::time::sleep_until(suspended.unwrap_or_else(tokio::time::Instant::now)), if suspended.is_some() => {
                window.suspended_until = None;
                refresh_window(&query, generation, resolver.as_ref(), &shared_refresh, &mut window, &state, event_sink.as_ref()).await;
            },
        }
    }
}

async fn load_window(
    query: &QueryId,
    generation: u64,
    resolver: &dyn IssueMaterializationResolver,
    shared_refresh: &SharedIssueRefresh,
    state: &AggregatorProjectionState,
    event_sink: &dyn EventSink,
) -> MaterializedWindow {
    let QueryId::Issues { scope, search, label } = query else { unreachable!("issue materializer only accepts issue queries") };
    let base_params = IssueQuery { search: search.clone(), label: label.clone(), match_fields: Default::default() };
    let bindings = match resolver.resolve_issue_sources(scope).await {
        Ok(sources) if !sources.is_empty() => sources,
        Ok(_) => {
            shared_refresh.unregister(query);
            let conditions = vec![unavailable(None, "query scope has no issue source")];
            publish_window(query, generation, Vec::new(), false, conditions.clone(), state, event_sink).await;
            return MaterializedWindow { sources: Vec::new(), needs_full_reload: true, conditions, suspended_until: None };
        }
        Err(message) => {
            shared_refresh.unregister(query);
            let conditions = vec![unavailable(None, message)];
            publish_window(query, generation, Vec::new(), false, conditions.clone(), state, event_sink).await;
            return MaterializedWindow { sources: Vec::new(), needs_full_reload: true, conditions, suspended_until: None };
        }
    };

    let loaded = stream::iter(bindings.into_iter().map(|binding| {
        let source = binding.source;
        let mut params = base_params.clone();
        params.match_fields = binding.filter.match_fields.into_iter().map(|(field, value)| (field, value.to_values())).collect();
        async move {
            if is_github_source(&source) {
                if let Some(message) = shared_refresh.health.active_error() {
                    return Err(unavailable(Some(source), message));
                }
            }
            let provider = resolver.issue_provider_for(&source).await.map_err(|message| unavailable(Some(source.clone()), message))?;
            // Capture before the request. Re-reading changes is safe; skipping an
            // update that arrived during the request is not.
            let refresh_cursor = Utc::now().to_rfc3339();
            shared_refresh.register(&source, query, &refresh_cursor, &provider);
            let (refresh_cursor, page) = shared_refresh
                .page(&source, &params, 1, &provider, state)
                .await
                .map_err(|message| unavailable(Some(source.clone()), message))?;
            let rows = page.items.into_iter().map(issue_row).collect::<Vec<_>>();
            let source_rows = rows.iter().cloned().map(|row| (row.reference.clone(), row)).collect::<HashMap<_, _>>();
            let loaded_count = source_rows.len();
            Ok::<_, ResultSetCondition>((
                IssueSourceWindow {
                    source,
                    query_params: params,
                    next_page: 2,
                    has_more: page.has_more,
                    refresh_cursor,
                    loaded_count,
                    rows: source_rows,
                },
                rows,
            ))
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_SOURCES)
    .collect::<Vec<_>>()
    .await;

    let mut rows = HashMap::<IssueRef, IssueRow>::new();
    let mut windows = Vec::new();
    let mut conditions = Vec::new();
    let mut suspension = None;
    let mut failed_sources = Vec::new();
    for result in loaded {
        match result {
            Ok((window, source_rows)) => {
                shared_refresh.advance(&window.source, query, &window.refresh_cursor);
                rows.extend(source_rows.into_iter().map(|row| (row.reference.clone(), row)));
                windows.push(window);
            }
            Err(condition) => {
                if let ResultSetCondition::IssueSourceUnavailable { source, message } = &condition {
                    if let Some(source) = source {
                        failed_sources.push(source.clone());
                    }
                    shared_refresh.health.note(message);
                    suspension = suspended_until(query, message);
                }
                conditions.push(condition);
            }
        }
    }
    for source in failed_sources {
        if !windows.iter().any(|window: &IssueSourceWindow| window.source == source) {
            shared_refresh.unregister_source(&source, query);
        }
    }
    let rows = rows.into_values().collect::<Vec<_>>();
    let needs_full_reload = !conditions.is_empty();
    publish_loaded_window(query, generation, rows, windows.iter().any(|window| window.has_more), conditions.clone(), state, event_sink)
        .await;
    MaterializedWindow { sources: windows, needs_full_reload, conditions, suspended_until: suspension }
}

async fn fetch_more(
    query: &QueryId,
    generation: u64,
    resolver: &dyn IssueMaterializationResolver,
    shared_refresh: &SharedIssueRefresh,
    window: &mut MaterializedWindow,
    state: &AggregatorProjectionState,
    event_sink: &dyn EventSink,
) {
    let requests = window
        .sources
        .iter()
        .enumerate()
        .filter(|(_, source)| source.has_more)
        .map(|(index, source)| (index, source.source.clone(), source.query_params.clone(), source.next_page))
        .collect::<Vec<_>>();
    let results = stream::iter(requests.into_iter().map(|(index, source, params, page)| async move {
        let result = async {
            let provider = resolver.issue_provider_for(&source).await?;
            shared_refresh.page(&source, &params, page, &provider, state).await.map(|(_, result)| result)
        }
        .await;
        (index, result)
    }))
    .buffer_unordered(MAX_CONCURRENT_SOURCES)
    .collect::<Vec<_>>()
    .await;

    let mut changed = Vec::new();
    let mut conditions = window.conditions.clone();
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
                let condition = unavailable(Some(source.source.clone()), message);
                if !conditions.contains(&condition) {
                    conditions.push(condition);
                }
                window.needs_full_reload = true;
            }
        }
    }
    window.conditions = conditions.clone();
    suppress_represented_rows(&mut changed, state).await;
    sort_rows(&mut changed);
    let result_state = demand_state(window.sources.iter().any(|source| source.has_more), conditions);
    // Metadata-only deltas are significant: an empty final page must still
    // clear `has_more` for clients.
    if let Some(delta) = state.apply_issue_changes(query, generation, changed, Vec::new(), result_state) {
        event_sink.emit(DaemonEvent::ResultDelta(Box::new(delta)));
        publish_awareness_sets(state, event_sink).await;
    }
}

async fn refresh_window(
    query: &QueryId,
    generation: u64,
    resolver: &dyn IssueMaterializationResolver,
    shared_refresh: &SharedIssueRefresh,
    window: &mut MaterializedWindow,
    state: &AggregatorProjectionState,
    event_sink: &dyn EventSink,
) {
    if window.suspended_until.is_some_and(|deadline| deadline > tokio::time::Instant::now()) {
        return;
    }
    if matches!(query, QueryId::Issues { search: Some(_), .. })
        || window.sources.iter().any(|source| !source.query_params.match_fields.is_empty())
    {
        // Provider-specific fields are not all present in normalized Issues,
        // so changed-since results cannot be safely filtered client-side.
        *window = load_window(query, generation, resolver, shared_refresh, state, event_sink).await;
        return;
    }
    if window.sources.is_empty() || window.needs_full_reload {
        *window = load_window(query, generation, resolver, shared_refresh, state, event_sink).await;
        return;
    }

    let requests = window
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| (index, source.source.clone(), source.refresh_cursor.clone()))
        .collect::<Vec<_>>();
    let results = stream::iter(requests.into_iter().map(|(index, source, since)| async move {
        let (next_cursor, result) = shared_refresh.changed_since(&source, query, &since, resolver, state).await;
        (index, next_cursor, result)
    }))
    .buffer_unordered(MAX_CONCURRENT_SOURCES)
    .collect::<Vec<_>>()
    .await;

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
                    if issue.state == IssueState::Open && issue_matches_query(&issue, query) {
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
                shared_refresh.advance(&source.source, query, &source.refresh_cursor);
            }
            Err(message) => {
                shared_refresh.unregister_source(&source.source, query);
                shared_refresh.health.note(&message);
                if let Some(deadline) = suspended_until(query, &message) {
                    window.suspended_until = Some(deadline);
                }
                conditions.push(unavailable(Some(source.source.clone()), message));
                window.needs_full_reload = true;
            }
        }
    }
    if overflowed || boundary_invalidated {
        *window = load_window(query, generation, resolver, shared_refresh, state, event_sink).await;
        return;
    }

    let mut changed = changed.into_values().collect::<Vec<_>>();
    suppress_represented_rows(&mut changed, state).await;
    sort_rows(&mut changed);
    let mut removed = removed.into_iter().collect::<Vec<_>>();
    removed.sort();
    window.conditions = conditions.clone();
    let result_state = demand_state(window.sources.iter().any(|source| source.has_more), conditions);
    if let Some(delta) = state.apply_issue_changes(query, generation, changed, removed, result_state) {
        event_sink.emit(DaemonEvent::ResultDelta(Box::new(delta)));
        publish_awareness_sets(state, event_sink).await;
    }
}

async fn publish_window(
    query: &QueryId,
    generation: u64,
    rows: Vec<IssueRow>,
    has_more: bool,
    conditions: Vec<ResultSetCondition>,
    state: &AggregatorProjectionState,
    event_sink: &dyn EventSink,
) {
    if let Some(result_set) = state.replace_issues(query, generation, rows, demand_state(has_more, conditions)) {
        event_sink.emit(DaemonEvent::ResultSet(Box::new(result_set)));
        publish_awareness_sets(state, event_sink).await;
    }
}

async fn publish_loaded_window(
    query: &QueryId,
    generation: u64,
    mut rows: Vec<IssueRow>,
    has_more: bool,
    conditions: Vec<ResultSetCondition>,
    state: &AggregatorProjectionState,
    event_sink: &dyn EventSink,
) {
    suppress_represented_rows(&mut rows, state).await;
    sort_rows(&mut rows);
    publish_window(query, generation, rows, has_more, conditions, state, event_sink).await;
}

async fn publish_awareness_sets(state: &AggregatorProjectionState, event_sink: &dyn EventSink) {
    for query in state.subscribed_queries() {
        if !matches!(query, QueryId::Awareness { .. }) {
            continue;
        }
        if let Some(result_set) = state.result_set_for(&query).await {
            event_sink.emit(DaemonEvent::ResultSet(Box::new(result_set)));
        }
    }
}

fn demand_state(has_more: bool, conditions: Vec<ResultSetCondition>) -> ResultSetState {
    ResultSetState { demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more }), conditions, truncated: false }
}

fn unavailable(source: Option<IssueSource>, message: impl Into<String>) -> ResultSetCondition {
    ResultSetCondition::IssueSourceUnavailable { source, message: message.into() }
}

fn issue_row(issue: flotilla_protocol::Issue) -> IssueRow {
    IssueRow { reference: issue.reference.clone(), issue }
}

fn sort_rows(rows: &mut [IssueRow]) {
    rows.sort_by(|left, right| left.reference.cmp_id_desc(&right.reference));
}

async fn suppress_represented_rows(rows: &mut Vec<IssueRow>, state: &AggregatorProjectionState) {
    let represented = state.represented_issue_refs().await;
    rows.retain(|row| !represented.contains(&row.reference));
}

fn issue_matches_query(issue: &flotilla_protocol::Issue, query: &QueryId) -> bool {
    let QueryId::Issues { label, .. } = query else { return false };
    label.as_ref().is_none_or(|label| issue.labels.iter().any(|candidate| candidate.eq_ignore_ascii_case(label)))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, path::Path, sync::Mutex as StdMutex};

    use chrono::{Duration as ChronoDuration, Utc};
    use flotilla_core::{
        event_sink::BroadcastEventSink,
        providers::{github_api::GhApiClient, issue_tracker::github::GitHubIssueProvider, ChannelLabel, CommandOutput, CommandRunner},
    };
    use flotilla_protocol::{
        issue_query::{IssueResultPage, READY_ISSUE_LABEL},
        result_set::{ConvoyIssueRow, ConvoyPhase, ConvoyRow},
        test_support::TestIssue,
        Issue, IssueChangeset, QueryCursor, ResourceRef,
    };
    use tokio::sync::{Mutex, Notify};
    use uuid::Uuid;

    use super::*;

    struct ScriptedProvider {
        pages: Mutex<VecDeque<IssueResultPage>>,
        changes: Mutex<VecDeque<IssueChangeset>>,
        seen_since: Mutex<Vec<String>>,
        seen_queries: Mutex<Vec<IssueQuery>>,
    }

    impl ScriptedProvider {
        fn new(pages: Vec<IssueResultPage>, changes: Vec<IssueChangeset>) -> Self {
            Self {
                pages: Mutex::new(pages.into()),
                changes: Mutex::new(changes.into()),
                seen_since: Mutex::new(Vec::new()),
                seen_queries: Mutex::new(Vec::new()),
            }
        }
    }

    /// A command runner for exercising GitHub's real `gh api --include`
    /// response parsing without spawning `gh`.
    struct ApiRunner {
        outputs: StdMutex<VecDeque<CommandOutput>>,
        calls: StdMutex<usize>,
    }

    impl ApiRunner {
        fn new(outputs: Vec<CommandOutput>) -> Self {
            Self { outputs: StdMutex::new(outputs.into()), calls: StdMutex::new(0) }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().expect("runner call count lock")
        }
    }

    #[async_trait]
    impl CommandRunner for ApiRunner {
        async fn run(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<String, String> {
            unreachable!("GitHub API client uses run_output")
        }

        async fn run_output(&self, _cmd: &str, _args: &[&str], _cwd: &Path, _label: &ChannelLabel) -> Result<CommandOutput, String> {
            *self.calls.lock().expect("runner call count lock") += 1;
            Ok(self.outputs.lock().expect("runner output lock").pop_front().expect("scripted gh response"))
        }

        async fn exists(&self, _cmd: &str, _args: &[&str]) -> bool {
            true
        }
    }

    #[async_trait]
    impl IssueProvider for ScriptedProvider {
        fn supports(&self, _source: &IssueSource) -> bool {
            true
        }

        async fn query(&self, source: &IssueSource, params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
            self.seen_queries.lock().await.push(params.clone());
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

    #[derive(Default)]
    struct ActivityProvider {
        polls: StdMutex<HashMap<String, usize>>,
    }

    #[async_trait]
    impl IssueProvider for ActivityProvider {
        fn supports(&self, _source: &IssueSource) -> bool {
            true
        }
        async fn query(&self, source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
            let mut item = issue("1");
            item.reference.source = source.clone();
            Ok(IssueResultPage { items: vec![item], total: None, has_more: false })
        }
        async fn fetch_by_id(&self, _reference: &IssueRef) -> Result<Issue, String> {
            unreachable!()
        }
        async fn list_changed_since(&self, source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
            *self.polls.lock().expect("polls").entry(source.scope.clone()).or_default() += 1;
            let updated = if source.scope == "busy" {
                let mut item = issue("2");
                item.reference.source = source.clone();
                item.as_of = Utc::now() + ChronoDuration::seconds(1);
                vec![item]
            } else {
                vec![]
            };
            Ok(IssueChangeset { updated, closed: vec![], has_more: false })
        }
        async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
            unreachable!()
        }
    }

    struct ActivityResolver(Arc<ActivityProvider>);
    #[async_trait]
    impl IssueMaterializationResolver for ActivityResolver {
        async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String> {
            Ok(vec![resolved_binding(IssueSource { service: "fake".into(), scope: scope.name.split('-').next().expect("source").into() })])
        }
        async fn issue_provider_for(&self, _source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
            Ok(self.0.clone())
        }
    }

    struct FilteredResolver {
        source: IssueSource,
        provider: Arc<dyn IssueProvider>,
    }

    #[async_trait]
    impl IssueMaterializationResolver for FilteredResolver {
        async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String> {
            let mut binding = resolved_binding(self.source.clone());
            if scope.name == "filtered" {
                binding.filter.match_fields.insert("milestone".into(), flotilla_resources::IssueFieldValue::One("release".into()));
            }
            Ok(vec![binding])
        }
        async fn issue_provider_for(&self, _source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
            Ok(self.provider.clone())
        }
    }

    struct TurnoverResolver {
        source: IssueSource,
        provider: StdMutex<Arc<dyn IssueProvider>>,
    }

    #[async_trait]
    impl IssueMaterializationResolver for TurnoverResolver {
        async fn resolve_issue_sources(&self, _scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String> {
            Ok(vec![resolved_binding(self.source.clone())])
        }
        async fn issue_provider_for(&self, _source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
            Ok(self.provider.lock().expect("provider").clone())
        }
    }

    struct ScopeResolver {
        provider: Arc<dyn IssueProvider>,
    }

    struct PartiallyUnavailableProvider {
        pages: Mutex<VecDeque<IssueResultPage>>,
    }

    struct RefreshUnavailableProvider {
        pages: Mutex<VecDeque<IssueResultPage>>,
    }

    #[async_trait]
    impl IssueProvider for PartiallyUnavailableProvider {
        fn supports(&self, _source: &IssueSource) -> bool {
            true
        }

        async fn query(&self, source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
            if source.scope == "unavailable" {
                return Err("source offline".into());
            }
            let mut page = self.pages.lock().await.pop_front().expect("scripted available page");
            for issue in &mut page.items {
                issue.reference.source = source.clone();
            }
            Ok(page)
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
    impl IssueProvider for RefreshUnavailableProvider {
        fn supports(&self, _source: &IssueSource) -> bool {
            true
        }

        async fn query(&self, source: &IssueSource, _params: &IssueQuery, _page: u32, _count: usize) -> Result<IssueResultPage, String> {
            let mut page = self.pages.lock().await.pop_front().expect("scripted page");
            for issue in &mut page.items {
                issue.reference.source = source.clone();
            }
            Ok(page)
        }

        async fn fetch_by_id(&self, _reference: &IssueRef) -> Result<Issue, String> {
            unreachable!("not used by materialization")
        }

        async fn list_changed_since(&self, _source: &IssueSource, _since: &str, _count: usize) -> Result<IssueChangeset, String> {
            Err("source unavailable during refresh".into())
        }

        async fn open_in_browser(&self, _reference: &IssueRef) -> Result<(), String> {
            unreachable!("not used by materialization")
        }
    }

    #[async_trait]
    impl IssueMaterializationResolver for ScopeResolver {
        async fn resolve_issue_sources(&self, scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String> {
            Ok(vec![resolved_binding(IssueSource { service: "https://issues.example".into(), scope: scope.name.clone() })])
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
        async fn resolve_issue_sources(&self, _scope: &QueryScope) -> Result<Vec<ResolvedIssueSourceBinding>, String> {
            Ok(self.sources.iter().cloned().map(resolved_binding).collect())
        }

        async fn issue_provider_for(&self, source: &IssueSource) -> Result<Arc<dyn IssueProvider>, String> {
            assert!(self.sources.contains(source));
            Ok(Arc::clone(&self.provider))
        }
    }

    fn resolved_binding(source: IssueSource) -> ResolvedIssueSourceBinding {
        ResolvedIssueSourceBinding {
            alias: source.scope.clone(),
            source,
            filter: Default::default(),
            create_with: Default::default(),
            creatable: false,
        }
    }

    fn issue(id: &str) -> Issue {
        TestIssue::new(id).id(id).build()
    }

    fn page(ids: &[&str], has_more: bool) -> IssueResultPage {
        IssueResultPage { items: ids.iter().map(|id| issue(id)).collect(), total: None, has_more }
    }

    fn project_query(name: &str) -> QueryId {
        QueryId::Issues { scope: QueryScope::new("flotilla", name), search: None, label: None }
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
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
        materializer.reconcile(HashMap::from([(query.clone(), generation)]));
        (materializer, event_rx)
    }

    async fn next_event(events: &mut broadcast::Receiver<DaemonEvent>) -> DaemonEvent {
        tokio::time::timeout(Duration::from_secs(1), events.recv()).await.expect("materialization event timeout").expect("event channel")
    }

    #[tokio::test]
    async fn project_demand_loads_a_source_qualified_first_page() {
        let state = AggregatorProjectionState::new();
        let query = project_query("repo_widget");
        let source = IssueSource { service: "https://issues.example".into(), scope: "widgets/api".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["WIDGET-123"], false)], vec![]));
        let (_materializer, mut events) = manager(&state, &query, vec![source.clone()], provider);

        assert!(matches!(next_event(&mut events).await, DaemonEvent::ResultSet(set) if set.query() == query));
        let result = state.result_set_for(&query).await.expect("live issue result set");
        assert_eq!(result.rows.as_issues().expect("issue rows")[0].reference, IssueRef { source, id: "WIDGET-123".into() });
        assert!(!result.state.demand.expect("demand metadata").has_more);
        assert!(result.state.conditions.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn provider_side_project_filters_keep_distinct_reload_and_pagination_pages() {
        let state = AggregatorProjectionState::new();
        let first = project_query("unfiltered");
        let second = project_query("filtered");
        let provider = Arc::new(ScriptedProvider::new(
            vec![page(&["1"], false), page(&["2"], true), page(&["3"], false), page(&["4"], false)],
            vec![],
        ));
        let resolver = Arc::new(FilteredResolver {
            source: IssueSource { service: "fake".into(), scope: "owner/repo".into() },
            provider: provider.clone(),
        });
        let (event_tx, mut events) = broadcast::channel(16);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
        let first_generation = subscribe(&state, &first);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation)]));
        next_event(&mut events).await;
        let second_generation = subscribe(&state, &second);
        materializer.reconcile(HashMap::from([(first, first_generation), (second.clone(), second_generation)]));
        next_event(&mut events).await;
        materializer.fetch_more(&second, second_generation);
        next_event(&mut events).await;
        let result = state.result_set_for(&second).await.expect("filtered window");
        let ids = result.rows.as_issues().expect("issues").iter().map(|row| row.reference.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["3", "2"]);
        let seen = provider.seen_queries.lock().await;
        assert_eq!(seen.len(), 3);
        assert!(seen[0].match_fields.is_empty());
        assert_eq!(seen[1].match_fields["milestone"], ["release"]);
        assert_eq!(seen[1], seen[2]);
        drop(seen);
        tokio::time::advance(REFRESH_INTERVAL + Duration::from_millis(1)).await;
        next_event(&mut events).await;
        next_event(&mut events).await;
        let reloaded = state.result_set_for(&second).await.expect("filtered reload");
        assert_eq!(reloaded.rows.as_issues().expect("issues")[0].reference.id, "4");
        assert_eq!(provider.seen_queries.lock().await.len(), 4);
    }

    #[tokio::test]
    async fn label_windows_share_only_complete_unfiltered_pages() {
        for has_more in [false, true] {
            let state = AggregatorProjectionState::new();
            let first = project_query("unfiltered");
            let QueryId::Issues { scope, .. } = project_query("label") else { unreachable!() };
            let second = QueryId::Issues { scope, search: None, label: Some("bug".into()) };
            let mut matching = issue("1");
            matching.labels = vec!["BUG".into()];
            let provider = Arc::new(ScriptedProvider::new(
                vec![IssueResultPage { items: vec![matching.clone(), issue("2")], total: None, has_more }, IssueResultPage {
                    items: vec![matching],
                    total: None,
                    has_more: false,
                }],
                vec![],
            ));
            let resolver = Arc::new(FixedResolver {
                sources: vec![IssueSource { service: "fake".into(), scope: "owner/repo".into() }],
                provider: provider.clone(),
            });
            let (event_tx, mut events) = broadcast::channel(16);
            let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
            let first_generation = subscribe(&state, &first);
            materializer.reconcile(HashMap::from([(first.clone(), first_generation)]));
            next_event(&mut events).await;
            let second_generation = subscribe(&state, &second);
            materializer.reconcile(HashMap::from([(first, first_generation), (second.clone(), second_generation)]));
            next_event(&mut events).await;
            assert_eq!(provider.seen_queries.lock().await.len(), if has_more { 2 } else { 1 });
            let result = state.result_set_for(&second).await.expect("filtered window");
            let rows = result.rows.as_issues().expect("issues");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].reference.id, "1");
            assert!(!result.state.demand.expect("demand").has_more);
        }
    }

    #[tokio::test]
    async fn provider_turnover_during_cached_refresh_invalidates_shared_pages() {
        let state = AggregatorProjectionState::new();
        let first = project_query("turnover-first");
        let second = project_query("turnover-second");
        let third = project_query("turnover-third");
        let old = Arc::new(ScriptedProvider::new(vec![page(&["1"], false), page(&["retired"], false)], vec![IssueChangeset {
            updated: vec![],
            closed: vec![],
            has_more: false,
        }]));
        let replacement = Arc::new(ScriptedProvider::new(vec![page(&["2"], false)], vec![]));
        let resolver = Arc::new(TurnoverResolver {
            source: IssueSource { service: "github".into(), scope: "owner/repo".into() },
            provider: StdMutex::new(old.clone()),
        });
        let (event_tx, mut events) = broadcast::channel(16);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver.clone(), Arc::new(BroadcastEventSink::new(event_tx)));
        let first_generation = subscribe(&state, &first);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation)]));
        next_event(&mut events).await;
        materializer.refresh(&first);
        next_event(&mut events).await;
        let second_generation = subscribe(&state, &second);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation), (second.clone(), second_generation)]));
        next_event(&mut events).await;
        *resolver.provider.lock().expect("provider") = replacement.clone();
        materializer.refresh(&first);
        next_event(&mut events).await;
        let third_generation = subscribe(&state, &third);
        materializer.reconcile(HashMap::from([(first, first_generation), (second, second_generation), (third.clone(), third_generation)]));
        next_event(&mut events).await;
        let result = state.result_set_for(&third).await.expect("replacement window");
        let ids = result.rows.as_issues().expect("issues").iter().map(|row| row.reference.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["2"], "a cached refresh must retire pages from the previous provider");
        assert_eq!(replacement.seen_queries.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn remaining_demand_uses_the_replacement_provider_for_refresh_and_pagination() {
        let state = AggregatorProjectionState::new();
        let query = project_query("turnover");
        let old = Arc::new(ScriptedProvider::new(vec![page(&["1"], true), page(&["retired"], false)], vec![]));
        let replacement = Arc::new(ScriptedProvider::new(vec![page(&["2"], false)], vec![IssueChangeset {
            updated: vec![issue("3")],
            closed: vec![],
            has_more: false,
        }]));
        let resolver = Arc::new(TurnoverResolver {
            source: IssueSource { service: "github".into(), scope: "owner/repo".into() },
            provider: StdMutex::new(old.clone()),
        });
        let (event_tx, mut events) = broadcast::channel(16);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver.clone(), Arc::new(BroadcastEventSink::new(event_tx)));
        let generation = subscribe(&state, &query);
        materializer.reconcile(HashMap::from([(query.clone(), generation)]));
        next_event(&mut events).await;
        *resolver.provider.lock().expect("provider") = replacement.clone();
        materializer.fetch_more(&query, generation);
        next_event(&mut events).await;
        materializer.refresh(&query);
        next_event(&mut events).await;
        assert!(old.seen_since.lock().await.is_empty(), "retired provider must not poll");
        assert_eq!(replacement.seen_since.lock().await.len(), 1);
        let result = state.result_set_for(&query).await.expect("window");
        let ids = result.rows.as_issues().expect("issues").iter().map(|row| row.reference.id.as_str()).collect::<Vec<_>>();
        assert_eq!(ids, ["3", "2", "1"]);
    }

    #[tokio::test]
    async fn two_project_queries_share_initial_and_overflow_reload_pages() {
        let state = AggregatorProjectionState::new();
        let first = project_query("first");
        let second = project_query("second");
        let provider = Arc::new(ScriptedProvider::new(
            vec![page(&["1"], false), page(&["2"], false), page(&["2"], false), page(&["2"], false)],
            vec![IssueChangeset { updated: vec![], closed: vec![], has_more: true }],
        ));
        let resolver = Arc::new(FixedResolver {
            sources: vec![IssueSource { service: "github".into(), scope: "owner/repo".into() }],
            provider: provider.clone(),
        });
        let (event_tx, mut events) = broadcast::channel(16);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
        let first_generation = subscribe(&state, &first);
        let second_generation = subscribe(&state, &second);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation), (second.clone(), second_generation)]));
        next_event(&mut events).await;
        next_event(&mut events).await;
        assert_eq!(provider.seen_queries.lock().await.len(), 1, "initial load belongs to the source");
        materializer.refresh(&first);
        next_event(&mut events).await;
        materializer.refresh(&second);
        next_event(&mut events).await;
        assert_eq!(provider.seen_queries.lock().await.len(), 2, "one shared full reload after overflow");
        assert_eq!(provider.seen_since.lock().await.len(), 1);
        for query in [&first, &second] {
            assert_eq!(state.result_set_for(query).await.expect("window").rows.as_issues().expect("issues")[0].reference.id, "2");
        }
    }

    async fn next_issue_event(events: &mut broadcast::Receiver<DaemonEvent>, query: &QueryId) {
        loop {
            match next_event(events).await {
                DaemonEvent::ResultSet(set) if &set.query() == query => return,
                DaemonEvent::ResultDelta(delta) if &delta.query() == query => return,
                _ => {}
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_view_preempts_governed_cadence_without_restarting_source_demand() {
        let state = AggregatorProjectionState::new();
        let scope = QueryScope::new("flotilla", "busy");
        state.replace_store_catalog(HashMap::new(), HashMap::from([(scope.clone(), vec![])])).await;
        let awareness = QueryId::Awareness { scope: None, grouping: Default::default(), limit: Default::default() };
        state.replace_subscriber(Uuid::new_v4(), &[QueryCursor { query: awareness, since: None }]);
        let query = QueryId::Issues { scope, search: None, label: Some(READY_ISSUE_LABEL.into()) };
        let provider = Arc::new(ActivityProvider::default());
        let (event_tx, mut events) = broadcast::channel(32);
        let mut materializer = IssueMaterializer::new(
            state.clone(),
            Arc::new(ActivityResolver(provider.clone())),
            Arc::new(BroadcastEventSink::new(event_tx)),
        );
        materializer.reconcile(state.subscribe_demand().borrow().clone());
        next_issue_event(&mut events, &query).await;
        tokio::time::advance(REFRESH_INTERVAL).await;
        next_issue_event(&mut events, &query).await;
        assert_eq!(provider.polls.lock().expect("polls")["busy"], 1);
        let viewer = Uuid::new_v4();
        state.replace_subscriber(viewer, &[QueryCursor { query: query.clone(), since: None }]);
        tokio::time::advance(REFRESH_INTERVAL).await;
        next_issue_event(&mut events, &query).await;
        assert_eq!(provider.polls.lock().expect("polls")["busy"], 2, "viewed demand shortens the governed interval");
        state.remove_subscriber(viewer);
        tokio::time::advance(REFRESH_INTERVAL).await;
        next_issue_event(&mut events, &query).await;
        assert_eq!(provider.polls.lock().expect("polls")["busy"], 2, "governed demand remains and resumes its longer cadence");
    }

    #[tokio::test(start_paused = true)]
    async fn opening_a_quiet_governed_source_restores_prompt_viewed_polling() {
        let state = AggregatorProjectionState::new();
        let scope = QueryScope::new("flotilla", "quiet");
        state.replace_store_catalog(HashMap::new(), HashMap::from([(scope.clone(), vec![])])).await;
        let awareness = QueryId::Awareness { scope: None, grouping: Default::default(), limit: Default::default() };
        state.replace_subscriber(Uuid::new_v4(), &[QueryCursor { query: awareness, since: None }]);
        let query = QueryId::Issues { scope, search: None, label: Some(READY_ISSUE_LABEL.into()) };
        let provider = Arc::new(ActivityProvider::default());
        let (event_tx, mut events) = broadcast::channel(32);
        let mut materializer = IssueMaterializer::new(
            state.clone(),
            Arc::new(ActivityResolver(provider.clone())),
            Arc::new(BroadcastEventSink::new(event_tx)),
        );
        materializer.reconcile(state.subscribe_demand().borrow().clone());
        next_issue_event(&mut events, &query).await;
        for _ in 0..23 {
            tokio::time::advance(REFRESH_INTERVAL).await;
            next_issue_event(&mut events, &query).await;
        }
        assert_eq!(provider.polls.lock().expect("polls")["quiet"], 4, "governed silence reaches the five-minute cap");
        state.replace_subscriber(Uuid::new_v4(), &[QueryCursor { query: query.clone(), since: None }]);
        tokio::time::advance(REFRESH_INTERVAL).await;
        next_issue_event(&mut events, &query).await;
        assert_eq!(provider.polls.lock().expect("polls")["quiet"], 5, "a newly opened view resets quiet-source backoff");
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_search_windows_adapt_their_reload_cadence() {
        let state = AggregatorProjectionState::new();
        let QueryId::Issues { scope, .. } = project_query("quiet-search") else { unreachable!() };
        let query = QueryId::Issues { scope, search: Some("text".into()), label: None };
        let provider = Arc::new(ScriptedProvider::new(std::iter::repeat_n(page(&["1"], false), 4).collect(), vec![]));
        let source = IssueSource { service: "fake".into(), scope: "owner/repo".into() };
        let (_materializer, mut events) = manager(&state, &query, vec![source], provider.clone());
        next_issue_event(&mut events, &query).await;
        for _ in 0..3 {
            tokio::time::advance(REFRESH_INTERVAL).await;
            next_issue_event(&mut events, &query).await;
        }
        assert_eq!(provider.seen_queries.lock().await.len(), 3, "initial load and two due quiet reloads");
        assert!(provider.seen_since.lock().await.is_empty(), "search filtering stays provider-side");
    }

    #[tokio::test(start_paused = true)]
    async fn busy_viewed_sources_poll_more_often_and_alias_demand_shares_each_due_fetch() {
        let state = AggregatorProjectionState::new();
        let busy = project_query("busy");
        let alias = project_query("busy-alias");
        let quiet = project_query("quiet");
        let provider = Arc::new(ActivityProvider::default());
        let (event_tx, mut events) = broadcast::channel(32);
        let mut materializer = IssueMaterializer::new(
            state.clone(),
            Arc::new(ActivityResolver(provider.clone())),
            Arc::new(BroadcastEventSink::new(event_tx)),
        );
        let demand = [&busy, &alias, &quiet].into_iter().map(|query| (query.clone(), subscribe(&state, query))).collect();
        materializer.reconcile(demand);
        for _ in 0..3 {
            next_event(&mut events).await;
        }
        for _ in 0..3 {
            tokio::time::advance(REFRESH_INTERVAL).await;
            for _ in 0..3 {
                next_event(&mut events).await;
            }
        }
        let polls = provider.polls.lock().expect("polls").clone();
        assert_eq!(polls["busy"], 3, "one poll per source despite two viewed queries");
        assert_eq!(polls["quiet"], 2, "quiet source skips the middle interval");
        materializer.reconcile(HashMap::new());
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::task::yield_now().await;
        assert_eq!(*provider.polls.lock().expect("polls"), polls, "no polling without demand");
    }

    #[tokio::test]
    async fn two_project_queries_for_one_repository_share_an_incremental_refresh() {
        let state = AggregatorProjectionState::new();
        let first = project_query("first-checkout");
        let second = project_query("second-checkout");
        let source = IssueSource { service: "https://github.com".into(), scope: "owner/repo".into() };
        let mut changed = issue("2");
        changed.as_of = Utc::now() + ChronoDuration::seconds(1);
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["1"], false), page(&["1"], false)], vec![IssueChangeset {
            updated: vec![changed],
            closed: vec![],
            has_more: false,
        }]));
        let resolver = Arc::new(FixedResolver { sources: vec![source], provider: provider.clone() });
        let (event_tx, mut events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
        let first_generation = subscribe(&state, &first);
        let second_generation = subscribe(&state, &second);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation), (second.clone(), second_generation)]));
        next_event(&mut events).await;
        next_event(&mut events).await;

        materializer.refresh(&first);
        materializer.refresh(&second);
        next_event(&mut events).await;
        next_event(&mut events).await;

        assert_eq!(provider.seen_since.lock().await.len(), 1);
        for query in [&first, &second] {
            let result = state.result_set_for(query).await.expect("materialized issues");
            assert!(result.rows.as_issues().expect("issue rows").iter().any(|row| row.reference.id == "2"));
        }
    }

    #[tokio::test]
    async fn a_new_query_does_not_apply_changes_older_than_its_initial_page() {
        let state = AggregatorProjectionState::new();
        let first = project_query("first");
        let second = project_query("second");
        let source = IssueSource { service: "https://github.com".into(), scope: "owner/repo".into() };
        let mut older = issue("1");
        older.title = "older refresh".into();
        let mut newer = issue("1");
        newer.title = "newer page".into();
        let provider = Arc::new(ScriptedProvider::new(
            vec![page(&["1"], false), IssueResultPage { items: vec![newer], total: None, has_more: false }],
            vec![IssueChangeset { updated: vec![older], closed: vec![], has_more: false }],
        ));
        let resolver = Arc::new(FixedResolver { sources: vec![source], provider });
        let (event_tx, mut events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
        let first_generation = subscribe(&state, &first);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation)]));
        next_event(&mut events).await;
        materializer.refresh(&first);
        next_event(&mut events).await;

        let second_generation = subscribe(&state, &second);
        materializer.reconcile(HashMap::from([(first, first_generation), (second.clone(), second_generation)]));
        next_event(&mut events).await;
        materializer.refresh(&second);
        next_event(&mut events).await;

        let result = state.result_set_for(&second).await.expect("second issue window");
        assert_eq!(result.rows.as_issues().expect("issue rows")[0].issue.title, "newer page");
    }

    #[tokio::test]
    async fn overlapping_query_cursors_do_not_replay_stale_updates_or_closures() {
        let state = AggregatorProjectionState::new();
        let first = project_query("first");
        let second = project_query("second");
        let source = IssueSource { service: "https://github.com".into(), scope: "owner/repo".into() };
        let mut stale = issue("1");
        stale.title = "stale update".into();
        let mut current = issue("1");
        current.title = "current page".into();
        let closed = issue("2").reference;
        let provider = Arc::new(ScriptedProvider::new(
            vec![page(&["1", "2"], false), IssueResultPage { items: vec![current, issue("2")], total: None, has_more: false }],
            vec![IssueChangeset { updated: vec![stale], closed: vec![closed], has_more: false }],
        ));
        let resolver = Arc::new(FixedResolver { sources: vec![source], provider: provider.clone() });
        let (event_tx, mut events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
        let first_generation = subscribe(&state, &first);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation)]));
        next_event(&mut events).await;
        materializer.refresh(&first);
        next_event(&mut events).await;
        tokio::time::sleep(Duration::from_millis(2)).await;
        let second_generation = subscribe(&state, &second);
        materializer.reconcile(HashMap::from([(first.clone(), first_generation), (second.clone(), second_generation)]));
        next_event(&mut events).await;

        materializer.refresh(&first);
        next_event(&mut events).await;
        materializer.refresh(&second);
        next_event(&mut events).await;

        assert_eq!(provider.seen_since.lock().await.len(), 1);
        let result = state.result_set_for(&second).await.expect("second issue window");
        let rows = result.rows.as_issues().expect("issue rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| row.issue.title == "current page"));
    }

    #[tokio::test(start_paused = true)]
    async fn github_rate_limit_suspends_a_scope_until_reset_then_resumes() {
        let state = AggregatorProjectionState::new();
        let query = project_query("rate-limited");
        let source = IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() };
        let reset = Utc::now().timestamp();
        let runner = Arc::new(ApiRunner::new(vec![
            CommandOutput {
                stdout: format!("HTTP/2 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {reset}\r\n\r\n{{\"message\":\"API rate limit exceeded\"}}"),
                stderr: "gh: API rate limit exceeded".into(),
                success: false,
            },
            CommandOutput {
                stdout: "HTTP/2 200 OK\r\n\r\n[{\"number\":896,\"title\":\"backoff\",\"state\":\"open\",\"labels\":[],\"updated_at\":\"2026-07-22T00:00:00Z\"}]".into(),
                stderr: String::new(),
                success: true,
            },
        ]));
        let api = Arc::new(GhApiClient::new(runner.clone()));
        let provider: Arc<dyn IssueProvider> = Arc::new(GitHubIssueProvider::new(api, runner.clone(), Path::new("/host")));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider);

        let DaemonEvent::ResultSet(initial) = next_event(&mut events).await else { panic!("rate limit publishes unavailable result") };
        assert!(initial.state.conditions.iter().any(|condition| matches!(condition, ResultSetCondition::IssueSourceUnavailable { message, .. } if rate_limit_reset(message).is_some())));
        assert_eq!(runner.calls(), 1);

        materializer.refresh(&query);
        tokio::task::yield_now().await;
        assert_eq!(runner.calls(), 1, "a suspended scope must not poll before reset plus jitter");

        tokio::time::advance(MAX_RATE_LIMIT_JITTER + Duration::from_secs(1)).await;
        let DaemonEvent::ResultSet(resumed) = next_event(&mut events).await else { panic!("reset resumes the suspended scope") };
        assert_eq!(runner.calls(), 2);
        assert_eq!(resumed.rows.as_issues().expect("issue rows")[0].reference.id, "896");
    }

    #[tokio::test]
    async fn refilter_republishes_loaded_issue_rows_when_convoy_representation_changes() {
        let state = AggregatorProjectionState::new();
        let query = project_query("repo_widget");
        let source = IssueSource { service: "https://issues.example".into(), scope: "widgets/api".into() };
        let represented = IssueRef { source: source.clone(), id: "WIDGET-810".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["WIDGET-809", "WIDGET-810"], false)], vec![]));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider);
        let _ = next_event(&mut events).await;
        let resource = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "batch");

        {
            let mut convoys = state.write().await;
            convoys.local_rows.insert(
                resource.clone(),
                ConvoyRow::builder()
                    .resource(resource.clone())
                    .name("batch")
                    .workflow_ref("workflow")
                    .phase(ConvoyPhase::Active)
                    .issues(vec![ConvoyIssueRow {
                        reference: represented.clone(),
                        title: "Issue WIDGET-810".into(),
                        state: IssueState::Open,
                    }])
                    .build(),
            );
        }
        materializer.refilter_active_queries();
        let DaemonEvent::ResultSet(suppressed) = next_event(&mut events).await else { panic!("refilter must emit a result set") };
        assert_eq!(
            suppressed.rows.as_issues().expect("suppressed issue rows").iter().map(|row| row.reference.id.as_str()).collect::<Vec<_>>(),
            vec!["WIDGET-809"]
        );

        state.write().await.local_rows.get_mut(&resource).expect("represented convoy row").phase = ConvoyPhase::Landed;
        materializer.refilter_active_queries();
        let DaemonEvent::ResultSet(restored) = next_event(&mut events).await else { panic!("refilter must emit a result set") };
        assert_eq!(
            restored.rows.as_issues().expect("restored issue rows").iter().map(|row| row.reference.id.as_str()).collect::<Vec<_>>(),
            vec!["WIDGET-810", "WIDGET-809"]
        );
    }

    #[tokio::test]
    async fn project_demand_unions_constituent_source_windows() {
        let state = AggregatorProjectionState::new();
        let query = QueryId::Issues { scope: QueryScope::new("flotilla", "platform"), search: None, label: None };
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
        let query = project_query("repo_linear");
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
    async fn fetch_more_preserves_an_unavailable_source_condition() {
        let state = AggregatorProjectionState::new();
        let query = project_query("partially-available");
        let available = IssueSource { service: "https://issues.example".into(), scope: "available".into() };
        let unavailable_source = IssueSource { service: "https://issues.example".into(), scope: "unavailable".into() };
        let provider = Arc::new(PartiallyUnavailableProvider {
            pages: Mutex::new(VecDeque::from([page(&["FIRST"], true), page(&["SECOND"], false)])),
        });
        let (materializer, mut events) = manager(&state, &query, vec![available, unavailable_source.clone()], provider);
        let _ = next_event(&mut events).await;

        materializer.fetch_more(&query, generation(&state, &query));

        let DaemonEvent::ResultDelta(delta) = next_event(&mut events).await else { panic!("fetch-more must emit a delta") };
        assert!(delta.state.expect("state replacement").conditions.iter().any(|condition| matches!(
            condition,
            ResultSetCondition::IssueSourceUnavailable { source: Some(source), .. } if source == &unavailable_source
        )));
    }

    #[tokio::test]
    async fn fetch_more_preserves_a_condition_discovered_by_incremental_refresh() {
        let state = AggregatorProjectionState::new();
        let query = project_query("refresh-unavailable");
        let source = IssueSource { service: "https://issues.example".into(), scope: "refresh-unavailable".into() };
        let provider =
            Arc::new(RefreshUnavailableProvider { pages: Mutex::new(VecDeque::from([page(&["FIRST"], true), page(&["SECOND"], false)])) });
        let (materializer, mut events) = manager(&state, &query, vec![source.clone()], provider);
        let _ = next_event(&mut events).await;

        materializer.refresh(&query);
        let DaemonEvent::ResultDelta(refresh_delta) = next_event(&mut events).await else { panic!("refresh must emit a delta") };
        assert!(refresh_delta.state.expect("state replacement").conditions.iter().any(|condition| matches!(
            condition,
            ResultSetCondition::IssueSourceUnavailable { source: Some(condition_source), .. } if condition_source == &source
        )));

        materializer.fetch_more(&query, generation(&state, &query));
        let DaemonEvent::ResultDelta(fetch_delta) = next_event(&mut events).await else { panic!("fetch-more must emit a delta") };
        assert!(fetch_delta.state.expect("state replacement").conditions.iter().any(|condition| matches!(
            condition,
            ResultSetCondition::IssueSourceUnavailable { source: Some(condition_source), .. } if condition_source == &source
        )));
    }

    #[tokio::test]
    async fn empty_final_page_emits_metadata_only_delta_that_clears_has_more() {
        let state = AggregatorProjectionState::new();
        let query = project_query("repo_empty_final");
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
        let query = project_query("repo_refresh");
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
    async fn incremental_refresh_reconciles_label_filter_membership() {
        let state = AggregatorProjectionState::new();
        let query = QueryId::Issues {
            scope: QueryScope::new("flotilla", "repo_ready_refresh"),
            search: None,
            label: Some(READY_ISSUE_LABEL.into()),
        };
        let source = IssueSource { service: "https://issues.example".into(), scope: "ready/refresh".into() };
        let mut loses_ready = issue("LOSES-READY");
        loses_ready.labels = vec![READY_ISSUE_LABEL.into()];
        let mut gains_ready = issue("GAINS-READY");
        gains_ready.labels = vec![READY_ISSUE_LABEL.into()];
        let provider =
            Arc::new(ScriptedProvider::new(vec![IssueResultPage { items: vec![loses_ready], total: Some(1), has_more: false }], vec![
                IssueChangeset { updated: vec![issue("LOSES-READY"), gains_ready], closed: vec![], has_more: false },
            ]));
        let (materializer, mut events) = manager(&state, &query, vec![source.clone()], provider);
        let _ = next_event(&mut events).await;

        materializer.refresh(&query);

        let DaemonEvent::ResultDelta(delta) = next_event(&mut events).await else { panic!("refresh must emit a delta") };
        assert_eq!(delta.changes.as_issues().expect("newly ready issue")[0].reference, IssueRef {
            source: source.clone(),
            id: "GAINS-READY".into()
        });
        assert_eq!(delta.changes.removed_issues().expect("issue that lost ready"), &[IssueRef { source, id: "LOSES-READY".into() }]);
        assert_eq!(
            state
                .result_set_for(&query)
                .await
                .expect("refreshed ready window")
                .rows
                .as_issues()
                .expect("ready issue rows")
                .iter()
                .map(|row| row.reference.id.as_str())
                .collect::<Vec<_>>(),
            vec!["GAINS-READY"]
        );
    }

    #[tokio::test]
    async fn incremental_refresh_keeps_rows_in_descending_id_order() {
        let state = AggregatorProjectionState::new();
        let query = project_query("repo_stable_order");
        let source = IssueSource { service: "https://issues.example".into(), scope: "stable/order".into() };
        let mut refreshed_low_id = issue("1");
        refreshed_low_id.as_of = Utc::now();
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["10", "1"], false)], vec![IssueChangeset {
            updated: vec![refreshed_low_id],
            closed: vec![],
            has_more: false,
        }]));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider);
        let _ = next_event(&mut events).await;
        let initial_window = state.result_set_for(&query).await.expect("initial window");
        let initial_ids =
            initial_window.rows.as_issues().expect("issue rows").iter().map(|row| row.reference.id.as_str()).collect::<Vec<_>>();
        assert_eq!(initial_ids, vec!["10", "1"]);

        materializer.refresh(&query);

        let _ = next_event(&mut events).await;
        let refreshed_window = state.result_set_for(&query).await.expect("refreshed window");
        let refreshed_ids =
            refreshed_window.rows.as_issues().expect("issue rows").iter().map(|row| row.reference.id.as_str()).collect::<Vec<_>>();
        assert_eq!(refreshed_ids, vec!["10", "1"], "an update timestamp must not move issue rows");
    }

    #[tokio::test]
    async fn incremental_refresh_preserves_the_loaded_page_boundary() {
        let state = AggregatorProjectionState::new();
        let query = project_query("repo_boundary");
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
        assert_eq!(delta.changes.removed_issues().expect("boundary eviction"), &[IssueRef { source, id: "ISSUE-00".into() }]);
        let result = state.result_set_for(&query).await.expect("bounded window");
        let rows = result.rows.as_issues().expect("issue rows");
        assert_eq!(rows.len(), PAGE_SIZE);
        assert_eq!(rows[0].reference.id, "NEWEST");
        assert!(rows.iter().all(|row| row.reference.id != "ISSUE-00"));
    }

    #[tokio::test]
    async fn boundary_removal_reloads_to_promote_the_next_unseen_row() {
        let state = AggregatorProjectionState::new();
        let query = project_query("repo_boundary_removal");
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

    #[tokio::test(start_paused = true)]
    async fn closed_only_refresh_advances_the_conservative_cursor() {
        let state = AggregatorProjectionState::new();
        let query = project_query("repo_closed_cursor");
        let source = IssueSource { service: "https://issues.example".into(), scope: "closed/repo".into() };
        let provider = Arc::new(ScriptedProvider::new(vec![page(&["CLOSED"], false)], vec![
            IssueChangeset { updated: vec![], closed: vec![IssueRef { source: source.clone(), id: "CLOSED".into() }], has_more: false },
            IssueChangeset { updated: vec![], closed: vec![], has_more: false },
        ]));
        let (materializer, mut events) = manager(&state, &query, vec![source], provider.clone());
        let _ = next_event(&mut events).await;

        materializer.refresh(&query);
        let _ = next_event(&mut events).await;
        std::thread::sleep(Duration::from_millis(2));
        tokio::time::advance(REFRESH_INTERVAL + Duration::from_millis(1)).await;
        let _ = next_event(&mut events).await;

        let seen = provider.seen_since.lock().await;
        assert_eq!(seen.len(), 2);
        assert_ne!(seen[0], seen[1], "successful closed-only refresh must advance its cursor");
    }

    #[tokio::test]
    async fn slow_provider_io_does_not_block_other_query_materializations() {
        let state = AggregatorProjectionState::new();
        let slow = project_query("slow");
        let fast = project_query("fast");
        let slow_generation = subscribe(&state, &slow);
        let fast_generation = subscribe(&state, &fast);
        let provider =
            Arc::new(BlockingProvider { slow_started: Notify::new(), release_slow: Notify::new(), slow_cancelled: Notify::new() });
        let resolver = Arc::new(ScopeResolver { provider: provider.clone() });
        let (event_tx, mut events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
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
        let query = project_query("slow");
        let provider =
            Arc::new(BlockingProvider { slow_started: Notify::new(), release_slow: Notify::new(), slow_cancelled: Notify::new() });
        let resolver = Arc::new(ScopeResolver { provider: provider.clone() });
        let (event_tx, _events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state, resolver, Arc::new(BroadcastEventSink::new(event_tx)));
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
        let query = project_query("recreated");
        let source = IssueSource { service: "https://issues.example".into(), scope: "recreated".into() };
        let provider = Arc::new(ScriptedProvider::new(
            vec![page(&["OLD-LIFETIME"], true), page(&["NEW-LIFETIME"], true), page(&["STALE-PAGE"], false)],
            vec![],
        ));
        let resolver = Arc::new(FixedResolver { sources: vec![source], provider: provider.clone() });
        let (event_tx, mut events) = broadcast::channel(8);
        let mut materializer = IssueMaterializer::new(state.clone(), resolver, Arc::new(BroadcastEventSink::new(event_tx)));
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
