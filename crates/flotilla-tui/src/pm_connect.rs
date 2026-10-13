//! `flotilla pm connect` — the manifest metadata-patch connector.
//!
//! One per PM instance, launched inside the PM session (design on
//! flotilla-org/flotilla#667, build #708). Dials the local daemon as a
//! client, subscribes to the aggregator's named queries — the one
//! replica-aware, fleet-merged source — and projects the rows into
//! group/identity-targeted metadata patches for the enclosing PM. flotillad
//! itself never touches a PM.
//!
//! Wheelhouse owns connection liveness: one replacing snapshot, then atomic
//! diffs without TTLs. Zellij (and older Wheelhouse) retain TTL heartbeats.
//! Delivery uncertainty is bounded; supervised daemon loss exits for restart.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub use flotilla_client::reconnect::is_incompatible_daemon_error;
use flotilla_client::resource::ResourceWatchError;
use flotilla_client::{
    reconnect::{is_permanent_daemon_error, ReconnectBackoff},
    DaemonEndpoint, SshEndpoint,
};
use flotilla_core::config::{ssh_destination, ConfigStore};
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_manifest::{
    keys::{CATALOG_TTL_MS, REASSERT_INTERVAL_MS},
    pm::PmInstance,
    projection::{project_catalog_with_ttl, Catalog, CatalogInput},
    recipe::{FlotillaRecipes, RecipeMint},
    sink::{LivenessMode, PatchSink, SnapshotAck},
    wire::MetadataPatch,
};
use flotilla_protocol::{
    result_set::{
        AwarenessGrouping, AwarenessLimit, AwarenessNode, ConvoyRow, IndependentRow, ProjectRepositoriesRow, QueryChanges, ResultDelta,
        ResultSet, Rows, StandingRoleRow,
    },
    DaemonEvent, HostName, QueryCursor, QueryId, ResourceRef,
};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

#[derive(Debug)]
enum SessionFailure {
    Daemon(String),
    PublicationDeadline,
    SessionLost(String),
}

impl SessionFailure {
    fn terminal(&self) -> bool {
        match self {
            Self::Daemon(error) => is_permanent_daemon_error(error),
            Self::PublicationDeadline | Self::SessionLost(_) => true,
        }
    }
}

impl From<String> for SessionFailure {
    fn from(error: String) -> Self {
        Self::Daemon(error)
    }
}

impl std::fmt::Display for SessionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Daemon(error) => formatter.write_str(error),
            Self::PublicationDeadline => formatter.write_str("connector publication failed: acknowledgement deadline exceeded"),
            Self::SessionLost(error) => write!(formatter, "supervised daemon session ended: {error}"),
        }
    }
}

async fn run_reconnecting<Connect, ConnectFuture, Connected, ConnectedFuture>(
    mut connect: Connect,
    mut run_connected: Connected,
) -> Result<(), String>
where
    Connect: FnMut() -> ConnectFuture,
    ConnectFuture: Future<Output = Result<Arc<dyn DaemonHandle>, String>>,
    Connected: FnMut(Arc<dyn DaemonHandle>) -> ConnectedFuture,
    ConnectedFuture: Future<Output = Result<(), SessionFailure>>,
{
    let mut session_backoff = ReconnectBackoff::default();
    loop {
        let daemon = flotilla_client::reconnect::connect_with_retry(&mut connect, |notice| match notice {
            flotilla_client::reconnect::ReconnectNotice::Attempt { attempt } => debug!(attempt, "connecting to daemon"),
            flotilla_client::reconnect::ReconnectNotice::Retry { attempt, error, delay } => {
                info!(attempt, %error, ?delay, "daemon unavailable; retrying")
            }
        })
        .await?;
        info!("connected to daemon");
        let connected_at = tokio::time::Instant::now();
        if let Err(error) = run_connected(daemon).await {
            // Resource validation refusals describe a deterministic request error;
            // reconnecting cannot make the same parameters supported.
            if error.terminal() {
                return Err(error.to_string());
            }
            // A healthy session resets the next retry; repeated short-lived
            // failures retain the shared exponential cap and jitter.
            if connected_at.elapsed() >= Duration::from_secs(30) {
                session_backoff.reset();
            }
            let delay = session_backoff.next_delay();
            info!(%error, ?delay, "daemon connection ended; reconnecting");
            tokio::time::sleep(delay).await;
        }
    }
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct PmConnectOptions {
    pub zellij_bin: Option<String>,
    pub plugin_url: Option<String>,
    pub wheelhouse_socket: Option<PathBuf>,
    /// Executable path or name minted into materialise recipes — what the PM
    /// runs on activation, resolved in the PM's own environment.
    pub flotilla_bin: String,
}

/// Resolve the PM this connector serves: explicit configuration wins, then
/// environment detection.
pub fn resolve_pm(options: &PmConnectOptions, env: &dyn Fn(&str) -> Option<String>) -> Result<PmInstance, String> {
    if let Some(socket) = &options.wheelhouse_socket {
        return Ok(PmInstance::wheelhouse(socket));
    }
    PmInstance::detect(env)
        .map(|pm| pm.with_zellij_bin(options.zellij_bin.clone()).with_plugin_url(options.plugin_url.clone()))
        .ok_or_else(|| "no presentation manager detected: run inside one or pass --wheelhouse-socket".to_owned())
}

/// What applying one daemon event did to the connector's row state.
#[derive(Debug, PartialEq, Eq)]
pub enum Applied {
    /// Rows changed; the catalog should be rebuilt and the diff published.
    Updated,
    /// Duplicate or irrelevant event; nothing to do.
    Ignored,
    /// Sequence gap (or delta before any full set) — resubscribe; the stale
    /// cursor makes the daemon emit a fresh full [`ResultSet`].
    Gap(QueryId),
}

mod lifetime;
mod resources;

/// The connector's held state: fleet-merged rows per query, per-query
/// cursors, and the catalog as last published.
pub struct ConnectorState {
    resources: resources::Records,
    awareness: Vec<AwarenessNode>,
    convoys: HashMap<ResourceRef, ConvoyRow>,
    independents: HashMap<ResourceRef, IndependentRow>,
    standing_roles: HashMap<ResourceRef, StandingRoleRow>,
    project_repositories: HashMap<ResourceRef, ProjectRepositoriesRow>,
    seqs: HashMap<QueryId, u64>,
    catalog: Arc<Mutex<Catalog>>,
    subscriber_id: uuid::Uuid,
    uncovered_services: BTreeSet<String>,
    snapshots: HashSet<QueryId>,
}

impl Default for ConnectorState {
    fn default() -> Self {
        Self {
            resources: resources::Records::default(),
            awareness: Vec::new(),
            convoys: HashMap::new(),
            independents: HashMap::new(),
            standing_roles: HashMap::new(),
            project_repositories: HashMap::new(),
            seqs: HashMap::new(),
            catalog: Arc::new(Mutex::new(Catalog::default())),
            subscriber_id: uuid::Uuid::new_v4(),
            uncovered_services: Default::default(),
            snapshots: HashSet::new(),
        }
    }
}

impl ConnectorState {
    /// Apply a resource list/watch envelope, including replicated records.
    pub fn apply_resource_records(&mut self, envelope: &flotilla_protocol::ResourceReadEnvelope) -> Result<(), String> {
        self.resources.apply(envelope)
    }

    pub fn apply_event(&mut self, event: &DaemonEvent) -> Applied {
        match event {
            DaemonEvent::ResultSet(set) => {
                self.snapshots.insert(set.query());
                self.apply_result_set(set)
            }
            DaemonEvent::ResultDelta(delta) => self.apply_delta(delta),
            _ => Applied::Ignored,
        }
    }

    fn apply_result_set(&mut self, set: &ResultSet) -> Applied {
        let query = set.query();
        if self.seqs.get(&query).is_some_and(|&seen| set.seq < seen) {
            return Applied::Ignored;
        }
        match &set.rows {
            Rows::Convoys { scope: None, rows } => {
                self.convoys = rows.iter().map(|row| (row.resource.clone(), row.clone())).collect();
            }
            Rows::Convoys { scope: Some(_), .. } => return Applied::Ignored,
            Rows::Independents { scope: None, rows } => {
                self.independents = rows.iter().map(|row| (row.resource.clone(), row.clone())).collect();
            }
            Rows::Independents { scope: Some(_), .. } => return Applied::Ignored,
            Rows::StandingRoles { scope: None, rows } => {
                self.standing_roles = rows.iter().map(|row| (row.resource.clone(), row.clone())).collect();
            }
            Rows::StandingRoles { scope: Some(_), .. } => return Applied::Ignored,
            Rows::ProjectRepositories { scope: None, rows } => {
                self.project_repositories = rows.iter().map(|row| (row.resource.clone(), row.clone())).collect();
            }
            Rows::ProjectRepositories { scope: Some(_), .. } => return Applied::Ignored,
            Rows::Awareness { rows, .. } => {
                self.awareness = rows.clone();
            }
            Rows::DispatchReady { .. } | Rows::Issues { .. } | Rows::Checkouts { .. } => return Applied::Ignored,
        }
        self.seqs.insert(query, set.seq);
        Applied::Updated
    }

    fn apply_delta(&mut self, delta: &ResultDelta) -> Applied {
        let query = delta.query();
        // These families participate in the startup barrier but have no
        // catalog rows. Their live updates cannot invalidate held rows.
        if matches!(&query, QueryId::DispatchReady { .. } | QueryId::Issues { .. } | QueryId::Checkouts { .. }) {
            return Applied::Ignored;
        }
        let Some(&seen) = self.seqs.get(&query) else {
            return Applied::Gap(query);
        };
        if delta.seq <= seen {
            return Applied::Ignored;
        }
        if delta.seq != seen + 1 {
            return Applied::Gap(query);
        }
        match &delta.changes {
            QueryChanges::Convoys { scope: None, changed: rows, removed } => {
                for row in rows {
                    self.convoys.insert(row.resource.clone(), row.clone());
                }
                for removed in removed {
                    self.convoys.remove(removed);
                }
            }
            QueryChanges::Convoys { scope: Some(_), .. } => return Applied::Ignored,
            QueryChanges::Independents { scope: None, changed: rows, removed } => {
                for row in rows {
                    self.independents.insert(row.resource.clone(), row.clone());
                }
                for removed in removed {
                    self.independents.remove(removed);
                }
            }
            QueryChanges::Independents { scope: Some(_), .. } => return Applied::Ignored,
            QueryChanges::StandingRoles { scope: None, changed: rows, removed } => {
                for row in rows {
                    self.standing_roles.insert(row.resource.clone(), row.clone());
                }
                for removed in removed {
                    self.standing_roles.remove(removed);
                }
            }
            QueryChanges::StandingRoles { scope: Some(_), .. } => return Applied::Ignored,
            QueryChanges::ProjectRepositories { scope: None, changed, removed } => {
                for row in changed {
                    self.project_repositories.insert(row.resource.clone(), row.clone());
                }
                for resource in removed {
                    self.project_repositories.remove(resource);
                }
            }
            QueryChanges::ProjectRepositories { scope: Some(_), .. } => return Applied::Ignored,
            QueryChanges::Awareness { changed: rows, removed, .. } => {
                self.awareness.retain(|node| !removed.contains(&node.id));
                for row in rows {
                    if let Some(existing) = self.awareness.iter_mut().find(|node| node.id == row.id) {
                        *existing = row.clone();
                    } else {
                        self.awareness.push(row.clone());
                    }
                }
                self.awareness.sort_by(|left, right| (&left.label, &left.id).cmp(&(&right.label, &right.id)));
            }
            QueryChanges::DispatchReady { .. } | QueryChanges::Issues { .. } | QueryChanges::Checkouts { .. } => {
                return Applied::Ignored;
            }
        }
        self.seqs.insert(query, delta.seq);
        Applied::Updated
    }

    /// Reproject the catalog from the held rows and return the patches that
    /// move the PM from the previously published catalog to the new one.
    pub fn rebuild(&mut self, mint: &dyn RecipeMint) -> Vec<MetadataPatch> {
        self.rebuild_at(mint, chrono::Utc::now())
    }

    /// Reproject at a supplied clock instant for expiry, replay and tests.
    pub fn rebuild_at(&mut self, mint: &dyn RecipeMint, now: flotilla_protocol::result_set::Timestamp) -> Vec<MetadataPatch> {
        let next = self.project_at(mint, now, Some(CATALOG_TTL_MS));
        let mut catalog = self.catalog.lock().expect("published catalog");
        let patches = next.diff_patches(&catalog);
        *catalog = next;
        patches
    }

    fn project_at(&mut self, mint: &dyn RecipeMint, now: flotilla_protocol::result_set::Timestamp, ttl_ms: Option<u64>) -> Catalog {
        let convoys: Vec<ConvoyRow> = self.convoys.values().cloned().collect();
        let independents: Vec<IndependentRow> = self.independents.values().cloned().collect();
        let standing_roles: Vec<StandingRoleRow> = self.standing_roles.values().cloned().collect();
        let project_repositories: Vec<ProjectRepositoriesRow> = self.project_repositories.values().cloned().collect();
        let awareness = (!self.awareness.is_empty()).then_some(self.awareness.as_slice());
        let mut subjects = self.resources.projection();
        subjects.now = Some(now);
        let next = project_catalog_with_ttl(
            &CatalogInput {
                subjects: Some(&subjects),
                awareness,
                convoys: &convoys,
                independents: &independents,
                standing_roles: &standing_roles,
                project_repositories: &project_repositories,
            },
            mint,
            ttl_ms,
        );
        self.uncovered_services = next.warn_new_uncovered_services(&self.uncovered_services);
        next
    }

    fn reset_sources(&mut self) {
        *self = Self {
            catalog: self.catalog.clone(),
            subscriber_id: self.subscriber_id,
            uncovered_services: self.uncovered_services.clone(),
            ..Self::default()
        };
    }

    fn snapshots_complete(&self) -> bool {
        self.cursors().iter().all(|cursor| self.snapshots.contains(&cursor.query))
    }

    /// Full re-assertion of the published catalog — the TTL heartbeat.
    pub fn reassert(&self) -> Vec<MetadataPatch> {
        self.catalog.lock().expect("published catalog").reassert_patches()
    }

    /// Resume cursors for every named query. A gapped query's cursor is
    /// stale by construction, so resubscribing with these gets it a full
    /// [`ResultSet`].
    pub fn cursors(&self) -> Vec<QueryCursor> {
        QueryId::ALWAYS_MATERIALIZED
            .iter()
            .filter(|query| !matches!(query, QueryId::Checkouts { scope: None }))
            .cloned()
            .chain([QueryId::Awareness { scope: None, grouping: AwarenessGrouping::Project, limit: AwarenessLimit::default() }])
            .map(|query| QueryCursor { since: self.seqs.get(&query).copied(), query })
            .collect()
    }
}

// Bound uncertainty, including a blocked transport. A restart replaces any
// remotely applied publication whose acknowledgement was lost.
async fn deliver(sink: &dyn PatchSink, patches: &[MetadataPatch], replace: bool) -> Result<SnapshotAck, SessionFailure> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut delay = Duration::from_millis(250);
        loop {
            let result = if replace { sink.replace(patches).await } else { sink.send_batch(patches).await.map(|()| SnapshotAck::Applied) };
            match result {
                Ok(ack) => return ack,
                Err(error) => warn!(%error, ?delay, "retrying catalog publication"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    })
    .await
    .map_err(|_| SessionFailure::PublicationDeadline)
}

async fn publish(
    state: &mut ConnectorState,
    sink: &dyn PatchSink,
    mint: &dyn RecipeMint,
    connector: &Connector,
    full: bool,
) -> Result<(), SessionFailure> {
    let mode = connector.mode(sink);
    let ttl_ms = (mode == LivenessMode::Ttl).then_some(CATALOG_TTL_MS);
    let next = state.project_at(mint, chrono::Utc::now(), ttl_ms);
    let first = !connector.published.load(std::sync::atomic::Ordering::SeqCst);
    let patches = if first && mode == LivenessMode::Connection {
        next.reassert_patches()
    } else if full && mode == LivenessMode::Ttl {
        let prior = state.catalog.lock().expect("published catalog");
        let mut patches: BTreeMap<_, _> = next.diff_patches(&prior).into_iter().map(|patch| (patch.target.clone(), patch)).collect();
        for patch in next.reassert_patches() {
            patches.entry(patch.target.clone()).and_modify(|diff| diff.set = patch.set.clone()).or_insert(patch);
        }
        patches.into_values().collect()
    } else {
        next.diff_patches(&state.catalog.lock().expect("published catalog"))
    };
    if !patches.is_empty() || first {
        let ack = if mode == LivenessMode::Ttl && sink.liveness() == LivenessMode::Connection {
            deliver_legacy(sink, &patches).await?;
            SnapshotAck::Applied
        } else {
            deliver(sink, &patches, first && mode == LivenessMode::Connection).await?
        };
        if ack == SnapshotAck::Unsupported {
            *connector.liveness.lock().expect("liveness mode") = Some(LivenessMode::Ttl);
            // The rejected replacing request wrote nothing. Reproject with TTL
            // and use the legacy transport for this entire process lifetime.
            let next = state.project_at(mint, chrono::Utc::now(), Some(CATALOG_TTL_MS));
            let patches = next.reassert_patches();
            deliver_legacy(sink, &patches).await?;
            *state.catalog.lock().expect("published catalog") = next;
        } else {
            *state.catalog.lock().expect("published catalog") = next;
        }
        connector.published.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(())
}

async fn deliver_legacy(sink: &dyn PatchSink, patches: &[MetadataPatch]) -> Result<(), SessionFailure> {
    // An older Wheelhouse accepts individual messages, not atomic arrays.
    tokio::time::timeout(Duration::from_secs(15), async {
        for patch in patches {
            loop {
                match sink.send(patch).await {
                    Ok(()) => break,
                    Err(error) => warn!(%error, "retrying legacy catalog publication"),
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    })
    .await
    .map_err(|_| SessionFailure::PublicationDeadline)
}

/// (Re)subscribe to every named query; publish only after subject bootstrap.
async fn resubscribe(daemon: &dyn DaemonHandle, state: &mut ConnectorState) -> Result<(), String> {
    for event in daemon.subscribe_queries(state.subscriber_id, &state.cursors()).await? {
        state.apply_event(&event);
    }
    Ok(())
}

async fn bootstrap_queries(
    daemon: &Arc<dyn DaemonHandle>,
    state: &mut ConnectorState,
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
) -> Result<(), String> {
    resubscribe(&**daemon, state).await?;
    while !state.snapshots_complete() {
        let restart = match events.recv().await {
            Ok(event) => matches!(state.apply_event(&event), Applied::Gap(_)),
            Err(RecvError::Lagged(_)) => true,
            Err(RecvError::Closed) => return Err("daemon event stream closed during bootstrap".to_owned()),
        };
        if restart {
            *events = daemon.subscribe();
            state.reset_sources();
            resubscribe(&**daemon, state).await?;
        }
    }
    Ok(())
}

/// Bootstrap from each merged watch's initial snapshot, never a separate list.
/// The resource layer subscribes before taking that snapshot; subsequent events
/// remain queued on the same watch. Consume the snapshot before publishing.
async fn ensure_resource_watches(
    daemon: &Arc<dyn DaemonHandle>,
    state: &mut ConnectorState,
    watched: &mut BTreeSet<String>,
    tasks: &mut tokio::task::JoinSet<()>,
    updates: &tokio::sync::mpsc::Sender<Result<flotilla_protocol::ResourceReadEnvelope, ResourceWatchError>>,
) -> Result<(), String> {
    use flotilla_client::resource::{ResourceClient, ResourceWatchRequest};
    // Scope follows the named-query graph. Namespaces with no convoy, role or
    // project membership are intentionally outside this subject projection.
    let namespaces: BTreeSet<_> = ["flotilla".to_string()]
        .into_iter()
        .chain(
            state
                .convoys
                .keys()
                .chain(state.standing_roles.keys())
                .chain(state.project_repositories.keys())
                .map(|resource| resource.namespace.clone()),
        )
        .collect();
    let client = ResourceClient::new(Arc::clone(daemon));
    for namespace in namespaces {
        if watched.contains(&namespace) {
            continue;
        }
        for kind in resources::KINDS {
            let mut watch = client
                .watch(
                    ResourceWatchRequest::builder().kind((*kind).to_string()).namespace(namespace.clone()).include_replicas(true).build(),
                )
                .await?;
            bootstrap_subject_watch(&mut watch, state, &namespace, kind).await?;
            let updates = updates.clone();
            tasks.spawn(async move {
                loop {
                    let update = match watch.next_event().await {
                        Ok(Some(envelope)) => Ok(envelope),
                        Ok(None) => Err(ResourceWatchError::Failed("subject catalog resource watch ended".to_string())),
                        Err(error) => Err(error),
                    };
                    let ended = update.is_err();
                    if updates.send(update).await.is_err() || ended {
                        break;
                    }
                }
            });
        }
        // A setup error returns to run_reconnecting, dropping this JoinSet;
        // this function never retries a partially admitted namespace in place.
        watched.insert(namespace);
    }
    Ok(())
}

// The daemon emits snapshot envelopes followed by a bookmark, then live
// events. Consume through that explicit boundary, allowing fragmentation but
// refusing live updates or a mismatched scope before setup can publish.
async fn bootstrap_subject_watch(
    watch: &mut flotilla_client::resource::ResourceWatch,
    state: &mut ConnectorState,
    namespace: &str,
    plural: &str,
) -> Result<(), String> {
    use flotilla_protocol::ResourceRecordType;
    use flotilla_resources::ResourceError;

    loop {
        let envelope = watch.next().await?.ok_or_else(|| "subject catalog resource watch ended during bootstrap".to_string())?;
        if envelope.namespace != namespace || envelope.plural != plural {
            return Err(ResourceError::invalid("subject resource snapshot scope does not match its watch").to_string());
        }
        if envelope.records.iter().any(|record| record.record_type == ResourceRecordType::Bookmark) {
            if envelope.records.len() != 1 || envelope.records[0].object.is_some() {
                return Err(ResourceError::invalid("subject resource snapshot bookmark must be a separate envelope").to_string());
            }
            return Ok(());
        }
        if envelope.records.iter().any(|record| !matches!(record.record_type, ResourceRecordType::Current | ResourceRecordType::Added)) {
            return Err(ResourceError::invalid("subject resource snapshot contains a live event before its bookmark").to_string());
        }
        state.apply_resource_records(&envelope)?;
    }
}

/// Subscribe, wait for complete snapshots, then publish acknowledged diffs.
/// The sink selects connection or TTL liveness; gaps rebuild every source.
pub async fn run_connector(
    daemon: Arc<dyn DaemonHandle>,
    sink: Arc<dyn PatchSink>,
    mint: Arc<dyn RecipeMint>,
    reassert_interval: Duration,
) -> Result<(), String> {
    Connector::default().run(daemon, sink, mint, reassert_interval).await
}

/// One PM publication lifetime. Keep this owner across daemon reconnects so a
/// fresh snapshot can retract facts removed while disconnected. Watch/query
/// state is connection-local and is always rebuilt from scratch.
#[derive(Default)]
pub struct Connector {
    catalog: Arc<Mutex<Catalog>>,
    liveness: Mutex<Option<LivenessMode>>,
    published: std::sync::atomic::AtomicBool,
}

impl Connector {
    fn mode(&self, sink: &dyn PatchSink) -> LivenessMode {
        self.liveness.lock().expect("liveness mode").unwrap_or_else(|| sink.liveness())
    }

    pub async fn run(
        &self,
        daemon: Arc<dyn DaemonHandle>,
        sink: Arc<dyn PatchSink>,
        mint: Arc<dyn RecipeMint>,
        reassert_interval: Duration,
    ) -> Result<(), String> {
        tokio::select! {
            result = self.run_session(daemon, sink.clone(), mint, reassert_interval) => result.map_err(|error| error.to_string()),
            error = monitor_health(self, &*sink) => Err(error),
        }
    }

    async fn run_session(
        &self,
        daemon: Arc<dyn DaemonHandle>,
        sink: Arc<dyn PatchSink>,
        mint: Arc<dyn RecipeMint>,
        reassert_interval: Duration,
    ) -> Result<(), SessionFailure> {
        let subscriber_id = uuid::Uuid::new_v4();
        let _queries = daemon.query_subscription(subscriber_id);
        let state = ConnectorState { subscriber_id, catalog: self.catalog.clone(), ..ConnectorState::default() };
        let result = run_connector_subscribed(&daemon, &*sink, &*mint, reassert_interval, state, self).await;
        daemon.unsubscribe_queries(subscriber_id).await;
        if self.mode(&*sink) == LivenessMode::Connection && self.published.load(std::sync::atomic::Ordering::SeqCst) {
            result.map_err(|error| if error.terminal() { error } else { SessionFailure::SessionLost(error.to_string()) })
        } else {
            result
        }
    }
}

async fn run_connector_subscribed(
    daemon: &Arc<dyn DaemonHandle>,
    sink: &dyn PatchSink,
    mint: &dyn RecipeMint,
    reassert_interval: Duration,
    mut state: ConnectorState,
    connector: &Connector,
) -> Result<(), SessionFailure> {
    // Subscribe to the broadcast before the query subscription so nothing
    // emitted in between is dropped.
    let mut events = daemon.subscribe();
    bootstrap_queries(daemon, &mut state, &mut events).await?;
    let (resource_tx, mut resource_rx) = tokio::sync::mpsc::channel(64);
    let mut resource_tasks = tokio::task::JoinSet::new();
    let mut watched_namespaces = BTreeSet::new();
    ensure_resource_watches(daemon, &mut state, &mut watched_namespaces, &mut resource_tasks, &resource_tx).await?;
    publish(&mut state, sink, mint, connector, true).await?;
    info!("pm connector subscribed; publishing catalog");

    let mut tick = tokio::time::interval(reassert_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.reset(); // the first tick fires immediately otherwise; the bootstrap publish just happened

    loop {
        tokio::select! {
            _ = tick.tick() => {
                // The landed window expires even when no resource changes.
                publish(&mut state, sink, mint, connector, connector.mode(sink) == LivenessMode::Ttl).await?;
            }
            update = resource_rx.recv() => {
                // Watch end/gap/error restarts the whole snapshot via run_reconnecting.
                // Dropping resource_tasks aborts sibling tasks and ResourceWatch::drop
                // cancels their commands; no partial subscription survives a retry.
                match update.ok_or_else(|| "subject catalog watch channel ended".to_string())? {
                    Ok(envelope) => state.apply_resource_records(&envelope)?,
                    Err(ResourceWatchError::Lagged(_)) => {
                        resnapshot(daemon, &mut state, &mut watched_namespaces, &mut resource_tasks, &mut resource_rx, &resource_tx, &mut events).await?;
                    }
                    Err(error) => return Err(error.to_string().into()),
                }
                publish(&mut state, sink, mint, connector, false).await?;
            }
            received = events.recv() => match received {
                Ok(event) => match state.apply_event(&event) {
                    Applied::Updated => {
                        ensure_resource_watches(daemon, &mut state, &mut watched_namespaces, &mut resource_tasks, &resource_tx).await?;
                        publish(&mut state, sink, mint, connector, false).await?;
                    }
                    Applied::Ignored => {}
                    Applied::Gap(query) => {
                        debug!(%query, "result stream gap; resubscribing");
                        resnapshot(daemon, &mut state, &mut watched_namespaces, &mut resource_tasks, &mut resource_rx, &resource_tx, &mut events).await?;
                        publish(&mut state, sink, mint, connector, false).await?;
                    }
                },
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "event stream lagged; resubscribing");
                    resnapshot(daemon, &mut state, &mut watched_namespaces, &mut resource_tasks, &mut resource_rx, &resource_tx, &mut events).await?;
                    publish(&mut state, sink, mint, connector, false).await?;
                }
                Err(RecvError::Closed) => {
                    return Err("daemon event stream closed".to_owned().into());
                }
            }
        }
    }
}

async fn resnapshot(
    daemon: &Arc<dyn DaemonHandle>,
    state: &mut ConnectorState,
    watched: &mut BTreeSet<String>,
    tasks: &mut tokio::task::JoinSet<()>,
    updates: &mut tokio::sync::mpsc::Receiver<Result<flotilla_protocol::ResourceReadEnvelope, ResourceWatchError>>,
    sender: &tokio::sync::mpsc::Sender<Result<flotilla_protocol::ResourceReadEnvelope, ResourceWatchError>>,
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
) -> Result<(), String> {
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    *events = daemon.subscribe();
    while updates.try_recv().is_ok() {}
    watched.clear();
    state.reset_sources();
    bootstrap_queries(daemon, state, events).await?;
    ensure_resource_watches(daemon, state, watched, tasks, sender).await
}

async fn monitor_health(connector: &Connector, sink: &dyn PatchSink) -> String {
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.reset();
    let mut failed_since = None;
    loop {
        tick.tick().await;
        if connector.mode(sink) != LivenessMode::Connection {
            failed_since = None;
            continue;
        }
        let now = tokio::time::Instant::now();
        let probe = tokio::time::timeout(Duration::from_secs(2), sink.health()).await;
        if matches!(probe, Ok(Ok(()))) {
            failed_since = None;
        } else {
            let since = failed_since.get_or_insert(now);
            if since.elapsed() >= Duration::from_secs(15) {
                return "presentation manager health connection failed for 15 seconds".to_owned();
            }
        }
    }
}

async fn with_parent<F: Future<Output = Result<(), String>>>(parent_alive: impl Fn() -> bool, run: F) -> Result<(), String> {
    tokio::pin!(run);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            result = &mut run => return result,
            _ = tick.tick() => if !parent_alive() { return Err("presentation manager parent exited".to_owned()); },
        }
    }
}

// A remote daemon's configured identity cannot establish viewer locality.
fn connector_local_host(remote: bool, configured: Option<String>, viewer: HostName) -> HostName {
    if remote {
        viewer
    } else {
        configured.map(HostName::new).unwrap_or(viewer)
    }
}

/// CLI entry: resolve the sink and supervise parent/PM lifetime across dial
/// retries. After publication, only TTL sinks reconnect within this process.
pub async fn run(
    remote: Option<SshEndpoint>,
    socket_path: &Path,
    config_dir: &Path,
    state_dir: &Path,
    require_host_daemon: bool,
    options: PmConnectOptions,
) -> Result<(), String> {
    let parent = lifetime::Parent::capture()?;
    // The connector runs headless in a PM pane: structured logs to stderr.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let sink = resolve_pm(&options, &|key| std::env::var(key).ok())?.sink();
    let config = ConfigStore::with_base(config_dir);
    let local_host = connector_local_host(remote.is_some(), config.load_daemon_config()?.host_name, HostName::local());
    let hosts = config.load_hosts()?;
    let mut ssh_hosts = BTreeMap::new();
    let mut ambiguous = HashSet::new();
    for remote in hosts.hosts.values() {
        let destination = ssh_destination(&remote.hostname, remote.user.as_deref());
        if ssh_hosts.insert(remote.expected_host_name.clone(), destination).is_some() {
            ambiguous.insert(remote.expected_host_name.clone());
        }
    }
    for host in ambiguous {
        ssh_hosts.remove(&host);
    }
    let mint: Arc<dyn RecipeMint> = Arc::new(FlotillaRecipes::new(options.flotilla_bin.clone()).with_host_routes(local_host, ssh_hosts));
    let connector = Arc::new(Connector::default());
    let health_connector = connector.clone();
    let health_sink = sink.clone();
    with_parent(|| parent.alive(), async {
        tokio::select! {
            error = monitor_health(&health_connector, &*health_sink) => Err(error),
            result = run_reconnecting(
            || async {
                let surface = flotilla_protocol::SurfaceDeclaration::ambient_for_namespace("flotilla");
                let endpoint = remote.clone().map(DaemonEndpoint::Ssh).unwrap_or_else(|| DaemonEndpoint::Local(socket_path.to_path_buf()));
                crate::socket::connect_endpoint_or_spawn_with_surface(&endpoint, config_dir, state_dir, require_host_daemon, surface)
                    .await
                    .map(|daemon| daemon as Arc<dyn DaemonHandle>)
            },
            |daemon| {
                let connector = connector.clone();
                let sink = sink.clone();
                let mint = mint.clone();
                async move { connector.run_session(daemon, sink, mint, Duration::from_millis(REASSERT_INTERVAL_MS)).await }
            },
        ) => result,
        }
    })
    .await
}

#[cfg(test)]
mod tests;
