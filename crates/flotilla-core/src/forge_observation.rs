//! Source-scoped forge ownership and replicated reads (ADR 0057).
use std::{collections::BTreeMap, future::Future, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, NodeId,
};
use flotilla_resources::{
    forge_read_name, normalize_issue_source, resolve_project_issue_sources, Clock, ForgeRead, ForgeReadHeartbeat, ForgeReadHeartbeatSpec,
    ForgeReadHeartbeatStatus, ForgeReadRequest, ForgeReadSpec, ForgeReadStatus, Host, InputMeta, IssueSourceResolution, Project,
    Repository, ResourceBackend, ResourceError, ResourceProvenance, SystemClock,
};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::Mutex;

use crate::providers::{
    change_request::{BoundObservations, ChangeRequestAdmission, ChangeRequestTracker, CrewGithubLoginsByRequest, ObservationError},
    github_api::{classified_rate_error, rate_limit_reset},
    issue_tracker::IssueProvider,
    types::ChangeRequest,
};

const HEARTBEAT_MAX_AGE: Duration = Duration::seconds(180);
const UNKNOWN_OWNER_GRACE: Duration = Duration::seconds(180);
const READ_FRESHNESS: Duration = Duration::seconds(60);
pub(crate) const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const DEMAND_MAX_AGE: Duration = Duration::seconds(180);
const DEMAND_RETENTION: Duration = Duration::seconds(3600);
const STATUS_WRITE_ATTEMPTS: usize = 8;
const READ_HEARTBEAT_INTERVAL: Duration = Duration::seconds(300);
const READ_HEARTBEAT_MAX_AGE: Duration = Duration::seconds(900);

type ReadLock = Arc<Mutex<Option<LocalRead>>>;

struct LocalRead {
    status: ForgeReadStatus,
    content_at: DateTime<Utc>,
}

// Visit only known observation fields, never arbitrary user field maps.
// Domain timestamps (as_of, updated_at, closed_at, merged_at) remain facts.
fn observation_objects(
    request: &ForgeReadRequest,
    value: &mut serde_json::Value,
    mut visit: impl FnMut(&mut serde_json::Map<String, serde_json::Value>),
) {
    let items = match request {
        ForgeReadRequest::Query { .. } => value.get_mut("items"),
        ForgeReadRequest::Changes { .. } => value.get_mut("updated"),
        ForgeReadRequest::Board | ForgeReadRequest::Issue { .. } => {
            if let Some(object) = value.as_object_mut() {
                visit(object);
            }
            return;
        }
        _ => return,
    };
    if let Some(items) = items.and_then(serde_json::Value::as_array_mut) {
        for item in items {
            if let Some(object) = item.as_object_mut() {
                visit(object);
            }
        }
    }
}
fn comparable_value(request: &ForgeReadRequest, value: &Option<serde_json::Value>) -> Option<serde_json::Value> {
    let mut value = value.clone();
    if let Some(value) = value.as_mut() {
        observation_objects(request, value, |object| {
            object.remove("observed_at");
            if matches!(request, ForgeReadRequest::Board) {
                object.remove("age_seconds");
            }
        });
    }
    value
}
fn content_changed(request: &ForgeReadRequest, previous: Option<&ForgeReadStatus>, status: &ForgeReadStatus) -> bool {
    previous.is_none_or(|previous| {
        previous.authority != status.authority
            || previous.error != status.error
            || previous.retry_at != status.retry_at
            || comparable_value(request, &previous.value) != comparable_value(request, &status.value)
    })
}
fn expire_status(mut status: Option<ForgeReadStatus>, now: DateTime<Utc>) -> Option<ForgeReadStatus> {
    if let Some(status) = status.as_mut() {
        if now.signed_duration_since(status.attempted_at) >= READ_HEARTBEAT_MAX_AGE && status.error.is_none() {
            status.error = Some("forge observation heartbeat stale".into());
        }
    }
    status
}
fn local_status(previous: Option<ForgeReadStatus>, local: Option<&LocalRead>) -> Option<ForgeReadStatus> {
    match (previous, local) {
        (Some(previous), Some(local)) if previous.authority == local.status.authority && previous.attempted_at == local.content_at => {
            Some(local.status.clone())
        }
        (previous, _) => previous,
    }
}

fn latest_status(statuses: impl IntoIterator<Item = ForgeReadStatus>) -> Option<ForgeReadStatus> {
    statuses.into_iter().max_by_key(|status| (status.attempted_at, status.authority.clone()))
}

fn unknown_health(declared_at: DateTime<Utc>, now: DateTime<Utc>) -> ObserverHealth {
    if now.signed_duration_since(declared_at) < UNKNOWN_OWNER_GRACE {
        ObserverHealth::Unknown
    } else {
        ObserverHealth::Unready
    }
}

fn canonical_source(source: &IssueSource) -> IssueSource {
    let mut source = normalize_issue_source(source);
    if source.service == "github" {
        source.service = "https://github.com".into();
    }
    source
}

fn origin(backend: &ResourceBackend, provenance: &ResourceProvenance) -> Result<NodeId, String> {
    match provenance {
        ResourceProvenance::Local => backend.local_root().map_err(|e| e.to_string()),
        ResourceProvenance::Replica { origin_root, .. } => Ok(origin_root.clone()),
    }
}

pub async fn project_home(backend: &ResourceBackend, namespace: &str, name: &str) -> Result<Option<NodeId>, String> {
    let sources = backend.including_replicas::<Project>(namespace).list_replica_sources().await.map_err(|e| e.to_string())?;
    let mut homes = Vec::new();
    for source in sources.items.into_iter().filter(|source| source.object.metadata.name == name) {
        homes.push((source.object.metadata.creation_timestamp, origin(backend, &source.provenance)?));
    }
    Ok(homes.into_iter().min().map(|(_, root)| root))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ObserverHealth {
    Ready,
    Unknown,
    Unready,
}

/// Shared sources elect once, regardless of how many Projects bind them.
/// Unknown host health preserves the preferred owner during startup; only
/// unready/stale evidence or expiry of the startup grace permits fallback.
pub async fn source_owner(backend: &ResourceBackend, namespace: &str, source: &IssueSource) -> Result<Option<NodeId>, String> {
    let mut source = canonical_source(source);
    for forge in backend.definitions::<flotilla_resources::Forge>(namespace).list().await.map_err(|e| e.to_string())? {
        if forge.spec.forge_id == source.service {
            source.service = forge.spec.https_url;
            break;
        }
    }
    let source = normalize_issue_source(&source);
    let now = Utc::now();
    // Recover every declaration home in one store listing, rather than listing
    // every Project's replica sources again for each binding and repository.
    let mut project_homes = BTreeMap::new();
    for record in backend.including_replicas::<Project>(namespace).list_replica_sources().await.map_err(|e| e.to_string())?.items {
        let candidate = (record.object.metadata.creation_timestamp, origin(backend, &record.provenance)?);
        let entry = project_homes.entry(record.object.metadata.name).or_insert_with(|| candidate.clone());
        *entry = candidate.min(entry.clone());
    }
    let mut homes = Vec::new();
    let repositories = backend.including_replicas::<Repository>(namespace);
    for project in backend.definitions::<Project>(namespace).list().await.map_err(|e| e.to_string())? {
        if project.spec.issue_source_bindings.iter().any(|binding| !binding.exclude && canonical_source(&binding.source) == source) {
            if let Some(home) = project_homes.get(&project.metadata.name) {
                homes.push(home.clone());
            }
        }
        if let IssueSourceResolution::Available { bindings } = resolve_project_issue_sources(&repositories, &project.spec).await {
            if bindings.iter().any(|binding| canonical_source(&binding.source) == source) {
                if let Some(home) = project_homes.get(&project.metadata.name) {
                    homes.push(home.clone());
                }
            }
        }
        // PR observation follows Repository forge identity even if its issue
        // source has been overridden or excluded.
        for member in &project.spec.repositories {
            if let Ok(repository) = repositories.get(&member.repo.to_string()).await {
                if repository.object.spec.forge().is_some_and(|forge| {
                    normalize_issue_source(&IssueSource { service: forge.service_url.clone(), scope: forge.repository.clone() }) == source
                }) {
                    if let Some(home) = project_homes.get(&project.metadata.name) {
                        homes.push(home.clone());
                    }
                }
            }
        }
    }
    let preferred = homes.into_iter().min();
    let mut ready = BTreeMap::new();
    for host in backend.including_replicas::<Host>(namespace).list_replica_sources().await.map_err(|e| e.to_string())?.items {
        if !matches!(host.object.spec.connection, flotilla_resources::HostConnection::Daemon) {
            continue;
        }
        let root = origin(backend, &host.provenance)?;
        let heartbeat = host.object.status.as_ref().and_then(|status| status.heartbeat_at);
        let health = match host.object.status.as_ref() {
            None => unknown_health(host.object.metadata.creation_timestamp, now),
            Some(status) if !status.ready => ObserverHealth::Unready,
            Some(_) => match heartbeat {
                Some(at) if now.signed_duration_since(at) < HEARTBEAT_MAX_AGE => ObserverHealth::Ready,
                Some(_) => ObserverHealth::Unready,
                None => unknown_health(host.object.metadata.creation_timestamp, now),
            },
        };
        // Newer heartbeat evidence wins. For equal timestamps, positive
        // Unready evidence wins conservatively; a duplicate Ready row must
        // not resurrect an explicitly unready origin (ADR 0057).
        let candidate = (heartbeat, health);
        let entry = ready.entry(root).or_insert(candidate);
        if candidate > *entry {
            *entry = candidate;
        }
    }
    if let Some((declared_at, preferred)) = preferred {
        if ready.get(&preferred).map_or_else(|| unknown_health(declared_at, now), |(_, health)| *health) != ObserverHealth::Unready {
            return Ok(Some(preferred));
        }
    } else if ready.is_empty() {
        return Ok(None);
    }
    ready
        .into_iter()
        .find_map(|(root, (_, health))| (health == ObserverHealth::Ready).then_some(root))
        .map(Some)
        .ok_or_else(|| "no ready forge observer".into())
}

pub async fn owns_source(backend: &ResourceBackend, namespace: &str, source: &IssueSource) -> Result<bool, String> {
    match source_owner(backend, namespace, source).await? {
        Some(owner) => Ok(owner == backend.local_root().map_err(|e| e.to_string())?),
        None => Ok(true),
    }
}

#[derive(Clone)]
pub struct ForgeReads {
    pub backend: ResourceBackend,
    pub namespace: String,
    pub touch_demand: bool,
    pub allow_stale: bool,
    locks: Arc<Mutex<BTreeMap<String, ReadLock>>>,
    clock: Arc<dyn Clock>,
}
impl ForgeReads {
    pub fn new(backend: ResourceBackend, namespace: String) -> Self {
        Self { backend, namespace, touch_demand: true, allow_stale: false, locks: Default::default(), clock: Arc::new(SystemClock) }
    }
    #[cfg(test)]
    pub(crate) fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    pub async fn read<T, F, Fut>(&self, source: &IssueSource, request: ForgeReadRequest, load: F) -> Result<T, String>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, String>> + Send,
    {
        let source = canonical_source(source);
        let name = forge_read_name(&source, &request);
        let lock = self.locks.lock().await.entry(name.clone()).or_default().clone();
        let mut local = lock.lock().await;
        let records = self.backend.using::<ForgeRead>(&self.namespace);
        let now = self.clock.now();
        let spec = ForgeReadSpec { source: source.clone(), request: request.clone(), demanded_at: now };
        // This spec remains a migration fallback. Renewals live in the small
        // companion so touching demand never rewrites a completed board.
        let current = match records.get(&name).await {
            Ok(current) => current,
            Err(ResourceError::NotFound { .. }) => {
                records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };
        self.touch_heartbeat(&name, current.spec.demanded_at, now).await?;
        let published = self.published_status(&name).await?;
        let previous = local_status(published.clone(), local.as_ref());
        let previous = if local.as_ref().is_some_and(|local| {
            published.as_ref().is_some_and(|status| status.attempted_at == local.content_at && status.authority == local.status.authority)
        }) {
            previous
        } else {
            self.with_heartbeat(&name, previous).await?
        };
        let owner = owns_source(&self.backend, &self.namespace, &source).await?;
        let fresh = previous.as_ref().is_some_and(|status| {
            now.signed_duration_since(status.attempted_at) < READ_FRESHNESS || status.retry_at.is_some_and(|at| at > now)
        });
        if !owner || fresh {
            let previous = if owner { previous } else { expire_status(previous, now) };
            return previous
                .ok_or_else(|| "forge observation pending at source owner".to_string())
                .and_then(|status| decode(status, self.allow_stale, &request));
        }
        // The shared per-request lock coalesces local work. Do not persist a
        // transient pending claim: cancellation or handoff must leave the last
        // completed observation usable and must never delay the next owner.
        let result = tokio::time::timeout(LOAD_TIMEOUT, load()).await.unwrap_or_else(|_| Err("forge observation timed out".into()));
        // Handoff during a slow read cannot publish facts from the former owner.
        if !owns_source(&self.backend, &self.namespace, &source).await? {
            return Err("forge observer changed during read".into());
        }
        let completed_at = self.clock.now();
        let (value, observed_at, error, retry_at) = match result {
            Ok(value) => (Some(serde_json::to_value(value).map_err(|e| e.to_string())?), Some(completed_at), None, None),
            Err(error) => (
                previous.as_ref().and_then(|s| s.value.clone()),
                previous.as_ref().and_then(|s| s.observed_at),
                Some(error.clone()),
                rate_limit_reset(&error),
            ),
        };
        let status = ForgeReadStatus {
            authority: self.backend.local_root().map_err(|e| e.to_string())?.to_string(),
            attempted_at: completed_at,
            value,
            observed_at,
            error,
            retry_at,
        };
        let changed = content_changed(&request, published.as_ref(), &status);
        let content_at = if changed {
            self.publish_status(&name, current.metadata.resource_version, &status).await?;
            status.attempted_at
        } else {
            published.expect("unchanged content has a published status").attempted_at
        };
        self.publish_heartbeat(&name, content_at, &status, changed).await?;
        *local = Some(LocalRead { status: status.clone(), content_at });
        decode(status, self.allow_stale, &request)
    }
    async fn published_status(&self, name: &str) -> Result<Option<ForgeReadStatus>, String> {
        let statuses = self.backend.including_replicas::<ForgeRead>(&self.namespace).get_all(name).await.map_err(|e| e.to_string())?;
        Ok(latest_status(statuses.items.into_iter().filter_map(|record| record.object.status)))
    }
    async fn with_heartbeat(&self, name: &str, status: Option<ForgeReadStatus>) -> Result<Option<ForgeReadStatus>, String> {
        let Some(mut status) = status else { return Ok(None) };
        let pulses =
            self.backend.including_replicas::<ForgeReadHeartbeat>(&self.namespace).get_all(name).await.map_err(|e| e.to_string())?;
        let pulse = pulses
            .items
            .into_iter()
            .filter_map(|record| record.object.status)
            .filter(|pulse| pulse.authority == status.authority && pulse.content_at == status.attempted_at)
            .max_by_key(|pulse| pulse.attempted_at);
        if let Some(pulse) = pulse {
            status.attempted_at = pulse.attempted_at;
            status.observed_at = pulse.observed_at;
        }
        Ok(expire_status(Some(status), self.clock.now()))
    }
    pub(crate) async fn status(&self, name: &str) -> Result<Option<ForgeReadStatus>, String> {
        let lock = self.locks.lock().await.entry(name.to_owned()).or_default().clone();
        let local = lock.lock().await;
        let published = self.published_status(name).await?;
        if local.as_ref().is_some_and(|local| {
            published.as_ref().is_some_and(|status| status.attempted_at == local.content_at && status.authority == local.status.authority)
        }) {
            Ok(expire_status(local_status(published, local.as_ref()), self.clock.now()))
        } else {
            self.with_heartbeat(name, published).await
        }
    }
    async fn touch_heartbeat(&self, name: &str, legacy_demand: DateTime<Utc>, now: DateTime<Utc>) -> Result<(), String> {
        let records = self.backend.using::<ForgeReadHeartbeat>(&self.namespace);
        let spec = ForgeReadHeartbeatSpec { demanded_at: if self.touch_demand { now } else { legacy_demand } };
        match records.get(name).await {
            Ok(current) if self.touch_demand && now.signed_duration_since(current.spec.demanded_at) >= READ_FRESHNESS => {
                records
                    .update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Ok(_) => {}
            Err(ResourceError::NotFound { .. }) => {
                records.create(&InputMeta::builder().name(name.to_owned()).build(), &spec).await.map_err(|e| e.to_string())?;
            }
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }
    async fn publish_heartbeat(
        &self,
        name: &str,
        content_at: DateTime<Utc>,
        status: &ForgeReadStatus,
        changed: bool,
    ) -> Result<(), String> {
        let records = self.backend.using::<ForgeReadHeartbeat>(&self.namespace);
        for attempt in 0..STATUS_WRITE_ATTEMPTS {
            let current = records.get(name).await.map_err(|e| e.to_string())?;
            if !changed
                && current.status.as_ref().is_some_and(|pulse| {
                    pulse.authority == status.authority
                        && pulse.content_at == content_at
                        && status.attempted_at.signed_duration_since(pulse.attempted_at) < READ_HEARTBEAT_INTERVAL
                })
            {
                return Ok(());
            }
            let pulse = ForgeReadHeartbeatStatus::builder()
                .authority(status.authority.clone())
                .content_at(content_at)
                .attempted_at(status.attempted_at)
                .maybe_observed_at(status.observed_at)
                .build();
            match records.update_status(name, &current.metadata.resource_version, &pulse).await {
                Ok(_) => return Ok(()),
                Err(ResourceError::Conflict { .. }) if attempt + 1 < STATUS_WRITE_ATTEMPTS => tokio::task::yield_now().await,
                Err(error) => return Err(error.to_string()),
            }
        }
        unreachable!("heartbeat writes have a bounded nonempty retry loop")
    }
    async fn publish_status(&self, name: &str, mut version: String, status: &ForgeReadStatus) -> Result<(), String> {
        let records = self.backend.using::<ForgeRead>(&self.namespace);
        for attempt in 0..STATUS_WRITE_ATTEMPTS {
            match records.update_status(name, &version, status).await {
                Ok(_) => return Ok(()),
                Err(ResourceError::Conflict { .. }) if attempt + 1 < STATUS_WRITE_ATTEMPTS => {
                    version = records.get(name).await.map_err(|e| e.to_string())?.metadata.resource_version;
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        unreachable!("status writes have a bounded nonempty retry loop")
    }
}
fn decode<T: DeserializeOwned>(status: ForgeReadStatus, allow_stale: bool, request: &ForgeReadRequest) -> Result<T, String> {
    if !allow_stale {
        if let Some(error) = &status.error {
            return Err(error.clone());
        }
    }
    if let Some(mut value) = status.value {
        if let Some(observed_at) = status.observed_at {
            observation_objects(request, &mut value, |object| {
                object.insert("observed_at".into(), serde_json::json!(observed_at));
            });
        }
        serde_json::from_value(value).map_err(|e| e.to_string())
    } else {
        Err(status.error.unwrap_or_else(|| "forge observation pending".into()))
    }
}

pub struct ObservedIssueProvider {
    pub inner: Arc<dyn IssueProvider>,
    pub reads: ForgeReads,
}
#[async_trait]
impl IssueProvider for ObservedIssueProvider {
    fn for_background_refresh(&self) -> Option<Arc<dyn IssueProvider>> {
        let mut reads = self.reads.clone();
        reads.touch_demand = false;
        Some(Arc::new(Self { inner: self.inner.clone(), reads }))
    }
    fn supports(&self, source: &IssueSource) -> bool {
        self.inner.supports(source)
    }
    async fn query(&self, source: &IssueSource, params: &IssueQuery, page: u32, count: usize) -> Result<IssueResultPage, String> {
        self.reads
            .read(source, ForgeReadRequest::Query { params: params.clone(), page, count }, || self.inner.query(source, params, page, count))
            .await
    }
    async fn fetch_by_id(&self, reference: &IssueRef) -> Result<Issue, String> {
        self.reads.read(&reference.source, ForgeReadRequest::Issue { id: reference.id.clone() }, || self.inner.fetch_by_id(reference)).await
    }
    async fn dispatch_board(&self, source: &IssueSource) -> Result<flotilla_protocol::DispatchBoardRepository, String> {
        let mut reads = self.reads.clone();
        reads.allow_stale = true;
        let mut board: flotilla_protocol::DispatchBoardRepository =
            reads.read(source, ForgeReadRequest::Board, || self.inner.dispatch_board(source)).await?;
        if let Some(status) = self.reads.status(&forge_read_name(&canonical_source(source), &ForgeReadRequest::Board)).await? {
            if let Some(at) = status.observed_at {
                board.observed_at = at;
            }
            board.refresh_error = status.error;
        }
        Ok(board)
    }
    async fn mission_fields(&self, reference: &IssueRef) -> Result<flotilla_protocol::MissionFields, String> {
        self.reads
            .read(&reference.source, ForgeReadRequest::Mission { id: reference.id.clone() }, || self.inner.mission_fields(reference))
            .await
    }
    async fn dispatch_facts(&self, reference: &IssueRef) -> Result<flotilla_protocol::DispatchIssueFacts, String> {
        self.reads
            .read(&reference.source, ForgeReadRequest::DispatchFacts { id: reference.id.clone() }, || self.inner.dispatch_facts(reference))
            .await
    }
    async fn list_changed_since(&self, source: &IssueSource, since: &str, count: usize) -> Result<IssueChangeset, String> {
        self.reads
            .read(source, ForgeReadRequest::Changes { since: since.into(), count }, || self.inner.list_changed_since(source, since, count))
            .await
    }
    async fn open_in_browser(&self, reference: &IssueRef) -> Result<(), String> {
        self.inner.open_in_browser(reference).await
    }
}

impl crate::in_process::InProcessDaemon {
    /// Service remote demand without making the requesting host a forge caller.
    pub async fn refresh_forge_read_demands(&self) -> Result<(), String> {
        let Ok(_guard) = self.forge_demand_refresh.try_lock() else { return Ok(()) };
        let backend = self.resource_backend();
        let namespace = self.provisioning_namespace_for_forge().await;
        let mut requests = BTreeMap::new();
        let mut demands = BTreeMap::new();
        for record in backend.including_replicas::<ForgeReadHeartbeat>(&namespace).list().await.map_err(|e| e.to_string())?.items {
            let demanded_at = demands.entry(record.object.metadata.name).or_insert(record.object.spec.demanded_at);
            *demanded_at = (*demanded_at).max(record.object.spec.demanded_at);
        }
        for record in backend.including_replicas::<ForgeRead>(&namespace).list().await.map_err(|e| e.to_string())?.items {
            let demanded_at = demands
                .get(&record.object.metadata.name)
                .copied()
                .unwrap_or(record.object.spec.demanded_at)
                .max(record.object.spec.demanded_at);
            if Utc::now().signed_duration_since(demanded_at) < DEMAND_MAX_AGE {
                requests.insert(record.object.metadata.name, record.object.spec);
            }
        }
        use futures::StreamExt;
        futures::stream::iter(requests.into_values())
            .for_each_concurrent(8, |spec| async {
                match owns_source(&backend, &namespace, &spec.source).await {
                    Ok(true) => {}
                    Ok(false) => return,
                    Err(error) => {
                        tracing::debug!(%error, "forge source owner unavailable");
                        return;
                    }
                }
                if matches!(
                    spec.request,
                    ForgeReadRequest::Branch { .. }
                        | ForgeReadRequest::ChangeRequests { .. }
                        | ForgeReadRequest::ChangeRequest { .. }
                        | ForgeReadRequest::MergedBranches { .. }
                ) {
                    if let Err(error) = self.service_change_request_demand(&namespace, &spec).await {
                        tracing::debug!(%error, "change request demand unavailable");
                    }
                    return;
                }
                let provider = match self.issue_provider_for_source(&spec.source).await {
                    Ok(provider) => provider.for_background_refresh().unwrap_or(provider),
                    Err(error) => {
                        tracing::debug!(%error, "forge demand provider unavailable");
                        return;
                    }
                };
                let reference = |id| IssueRef { source: spec.source.clone(), id };
                let result = match spec.request {
                    ForgeReadRequest::Board => provider.dispatch_board(&spec.source).await.map(|_| ()),
                    ForgeReadRequest::Query { params, page, count } => provider.query(&spec.source, &params, page, count).await.map(|_| ()),
                    ForgeReadRequest::Issue { id } => provider.fetch_by_id(&reference(id)).await.map(|_| ()),
                    ForgeReadRequest::Changes { since, count } => {
                        provider.list_changed_since(&spec.source, &since, count).await.map(|_| ())
                    }
                    ForgeReadRequest::Mission { id } => provider.mission_fields(&reference(id)).await.map(|_| ()),
                    ForgeReadRequest::DispatchFacts { id } => provider.dispatch_facts(&reference(id)).await.map(|_| ()),
                    ForgeReadRequest::Branch { .. }
                    | ForgeReadRequest::ChangeRequests { .. }
                    | ForgeReadRequest::ChangeRequest { .. }
                    | ForgeReadRequest::MergedBranches { .. } => unreachable!(),
                };
                if let Err(error) = result {
                    tracing::debug!(%error, "forge read demand unavailable");
                }
            })
            .await;
        // Retire only this root's idle requests; replicas follow tombstones.
        let local = backend.using::<ForgeRead>(&namespace);
        for record in local.list().await.map_err(|e| e.to_string())?.items {
            let demanded_at = demands.get(&record.metadata.name).copied().unwrap_or(record.spec.demanded_at).max(record.spec.demanded_at);
            if Utc::now().signed_duration_since(demanded_at) >= DEMAND_RETENTION {
                if let Err(error) = backend.using::<ForgeReadHeartbeat>(&namespace).delete(&record.metadata.name).await {
                    if !matches!(error, ResourceError::NotFound { .. }) {
                        tracing::debug!(name = %record.metadata.name, %error, "idle forge heartbeat cleanup failed");
                    }
                }
                if let Err(error) = local.delete(&record.metadata.name).await {
                    tracing::debug!(name = %record.metadata.name, %error, "idle forge demand cleanup failed");
                }
            }
        }
        Ok(())
    }
}

pub struct ObservedChangeRequestTracker {
    pub inner: Arc<dyn ChangeRequestTracker>,
    pub reads: ForgeReads,
    pub source: IssueSource,
}
#[async_trait]
impl ChangeRequestTracker for ObservedChangeRequestTracker {
    fn for_background_refresh(&self) -> Option<Arc<dyn ChangeRequestTracker>> {
        let mut reads = self.reads.clone();
        reads.touch_demand = false;
        Some(Arc::new(Self { inner: self.inner.clone(), reads, source: self.source.clone() }))
    }
    async fn observe_bound(&self, numbers: &[u64], logins: &CrewGithubLoginsByRequest) -> Result<BoundObservations, ObservationError> {
        if !owns_source(&self.reads.backend, &self.reads.namespace, &self.source).await? {
            return Err("bound PR observation belongs to another source owner".into());
        }
        self.inner.observe_bound(numbers, logins).await
    }
    async fn list_change_requests(&self, limit: usize) -> Result<Vec<(String, ChangeRequest)>, String> {
        self.reads.read(&self.source, ForgeReadRequest::ChangeRequests { limit }, || self.inner.list_change_requests(limit)).await
    }
    async fn find_change_request_by_branch(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        self.reads
            .read(&self.source, ForgeReadRequest::Branch { branch: branch.into() }, || async {
                self.inner.find_change_request_by_branch(branch).await.map_err(|e| e.to_string())
            })
            .await
            .map_err(classified_rate_error)
    }
    async fn find_change_request_by_branch_for_admission(&self, branch: &str) -> Result<Option<(String, ChangeRequest)>, ObservationError> {
        self.inner.find_change_request_by_branch_for_admission(branch).await
    }
    async fn get_change_request(&self, id: &str) -> Result<(String, ChangeRequest), String> {
        self.reads.read(&self.source, ForgeReadRequest::ChangeRequest { id: id.into() }, || self.inner.get_change_request(id)).await
    }
    async fn get_change_request_for_admission(&self, id: &str) -> Result<ChangeRequestAdmission, ObservationError> {
        self.inner.get_change_request_for_admission(id).await
    }
    async fn list_merged_branch_names(&self, limit: usize) -> Result<Vec<String>, String> {
        self.reads.read(&self.source, ForgeReadRequest::MergedBranches { limit }, || self.inner.list_merged_branch_names(limit)).await
    }
    async fn update_body(&self, id: &str, body: &str) -> Result<(), String> {
        self.inner.update_body(id, body).await
    }
    async fn open_in_browser(&self, id: &str) -> Result<(), String> {
        self.inner.open_in_browser(id).await
    }
    async fn close_change_request(&self, id: &str) -> Result<(), String> {
        self.inner.close_change_request(id).await
    }
    async fn merge_change_request(&self, id: &str) -> Result<(), String> {
        self.inner.merge_change_request(id).await
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::{HostSpec, HostStatus, InMemoryBackend, IssueSourceBindingSpec, ProjectSpec, Resource};

    use super::*;

    fn backend(root: &str) -> ResourceBackend {
        ResourceBackend::InMemory(InMemoryBackend::default()).with_local_root(NodeId::new(root))
    }
    fn source() -> IssueSource {
        IssueSource { service: "https://github.com".into(), scope: "org/shared".into() }
    }
    fn meta(name: &str) -> InputMeta {
        InputMeta::builder().name(name.into()).build()
    }
    async fn replicate<T: Resource>(from: &ResourceBackend, to: &ResourceBackend) {
        let list = from.using::<T>("flotilla").list().await.unwrap();
        to.replica_writer::<T>(from.local_root().unwrap(), "flotilla").replace(&list, Utc::now()).await.unwrap();
    }

    fn clocked_reads(root: &str) -> (ForgeReads, Arc<flotilla_resources::VirtualClock>) {
        let clock = Arc::new(flotilla_resources::VirtualClock::new(Utc::now()));
        let mut reads = ForgeReads::new(backend(root), "flotilla".into());
        reads.clock = clock.clone();
        (reads, clock)
    }

    // Minute refreshes retain one whole-board publication, while demand renews
    // in its small companion and observation heartbeats publish every 5 minutes.
    // Generate elapsed poll counts across heartbeat edges and domain changes.
    #[hegel::test]
    fn unchanged_boards_publish_only_low_rate_heartbeats(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let polls = tc.draw(gs::integers::<u32>().min_value(1).max_value(12));
        let changed_field = tc.draw(gs::integers::<u8>().min_value(0).max_value(2));
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (reads, clock) = clocked_reads("owner");
            let source = source();
            let name = forge_read_name(&source, &ForgeReadRequest::Board);
            let initial = clock.now();
            let mut value = serde_json::json!({"observed_at": initial, "age_seconds": 0,
                "issues": (0..400).map(|id| serde_json::json!({"id":id,"title":"open","updated_at":initial})).collect::<Vec<_>>()});
            reads.read(&source, ForgeReadRequest::Board, || async { Ok(value.clone()) }).await.unwrap();
            let records = reads.backend.using::<ForgeRead>("flotilla");
            let first = records.get(&name).await.unwrap();
            let pulses = reads.backend.using::<ForgeReadHeartbeat>("flotilla");
            let mut pulse = pulses.get(&name).await.unwrap().status.unwrap();
            for poll in 1..=polls {
                let now = clock.advance(READ_FRESHNESS);
                value["observed_at"] = serde_json::json!(now);
                value["age_seconds"] = serde_json::json!(poll);
                let returned = reads.read(&source, ForgeReadRequest::Board, || async { Ok(value.clone()) }).await.unwrap();
                assert_eq!(returned, value);
                let current = records.get(&name).await.unwrap();
                assert_eq!(
                    current.metadata.resource_version, first.metadata.resource_version,
                    "unchanged board must not replicate in full"
                );
                assert_eq!(current.status, first.status);
                let heartbeat = pulses.get(&name).await.unwrap();
                assert_eq!(heartbeat.spec.demanded_at, now);
                let next = heartbeat.status.unwrap();
                if poll % 5 == 0 {
                    assert_eq!(next.attempted_at, now);
                    assert_eq!(next.observed_at, Some(now));
                } else {
                    assert_eq!(next, pulse);
                }
                pulse = next;
                assert_eq!(reads.status(&name).await.unwrap().unwrap().observed_at, Some(now));
                // Coalescing uses completed local observations, rather than
                // the old whole-board publication timestamp.
                let _: serde_json::Value =
                    reads.read(&source, ForgeReadRequest::Board, || async { panic!("fresh local read must coalesce") }).await.unwrap();
            }
            clock.advance(READ_FRESHNESS);
            match changed_field {
                0 => value["issues"][0]["title"] = serde_json::json!("changed"),
                1 => value["issues"][0]["updated_at"] = serde_json::json!(clock.now()),
                _ => value["issues"] = serde_json::json!([]),
            }
            reads.read(&source, ForgeReadRequest::Board, || async { Ok(value) }).await.unwrap();
            assert_ne!(records.get(&name).await.unwrap().metadata.resource_version, first.metadata.resource_version);
        });
    }

    // Other whole reads carry Issue observation timestamps too. Their freshness
    // must not republish unchanged facts, and similarly named user fields and
    // as_of revision timestamps must remain semantic. Generate all issue shapes.
    #[hegel::test]
    fn issue_read_publication_preserves_user_fields_and_revision_time(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let shape = tc.draw(gs::integers::<u8>().min_value(0).max_value(2));
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (reads, clock) = clocked_reads("owner");
            let request = match shape {
                0 => ForgeReadRequest::Issue { id: "7".into() },
                1 => ForgeReadRequest::Query { params: IssueQuery::default(), page: 1, count: 1 },
                _ => ForgeReadRequest::Changes { since: "cursor".into(), count: 1 },
            };
            let wrap = |item: serde_json::Value| match shape {
                0 => item,
                1 => serde_json::json!({"items":[item]}),
                _ => serde_json::json!({"updated":[item]}),
            };
            let source = source();
            let name = forge_read_name(&source, &request);
            let mut item = serde_json::json!({"observed_at":clock.now(),"as_of":clock.now(),"fields":{"observed_at":"user fact"}});
            reads.read(&source, request.clone(), || async { Ok(wrap(item.clone())) }).await.unwrap();
            let records = reads.backend.using::<ForgeRead>("flotilla");
            let first = records.get(&name).await.unwrap();
            clock.advance(READ_FRESHNESS);
            item["observed_at"] = serde_json::json!(clock.now());
            let expected = wrap(item.clone());
            assert_eq!(reads.read(&source, request.clone(), || async { Ok(expected.clone()) }).await.unwrap(), expected);
            assert_eq!(records.get(&name).await.unwrap().metadata.resource_version, first.metadata.resource_version);
            clock.advance(READ_FRESHNESS);
            item["fields"]["observed_at"] = serde_json::json!("changed user fact");
            reads.read(&source, request.clone(), || async { Ok(wrap(item.clone())) }).await.unwrap();
            let changed = records.get(&name).await.unwrap();
            assert_ne!(changed.metadata.resource_version, first.metadata.resource_version);
            clock.advance(READ_FRESHNESS);
            item["as_of"] = serde_json::json!(clock.now());
            reads.read(&source, request, || async { Ok(wrap(item)) }).await.unwrap();
            assert_ne!(records.get(&name).await.unwrap().metadata.resource_version, changed.metadata.resource_version);
        });
    }

    // Repeated identical errors do not resend the last good board. Error
    // transitions and recovery publish immediately even with identical facts.
    #[tokio::test]
    async fn error_transitions_publish_once_and_background_does_not_renew_demand() {
        let (mut reads, clock) = clocked_reads("owner");
        let source = source();
        let name = forge_read_name(&source, &ForgeReadRequest::Board);
        reads.allow_stale = true;
        reads.read(&source, ForgeReadRequest::Board, || async { Ok(42_u64) }).await.unwrap();
        let records = reads.backend.using::<ForgeRead>("flotilla");
        let first = records.get(&name).await.unwrap();
        reads.touch_demand = false;
        clock.advance(READ_FRESHNESS);
        assert_eq!(reads.read(&source, ForgeReadRequest::Board, || async { Err::<u64, _>("outage".into()) }).await.unwrap(), 42);
        let failed = records.get(&name).await.unwrap();
        assert_ne!(first.metadata.resource_version, failed.metadata.resource_version);
        assert_eq!(failed.status.as_ref().unwrap().error.as_deref(), Some("outage"));
        clock.advance(READ_FRESHNESS);
        reads.read(&source, ForgeReadRequest::Board, || async { Err::<u64, _>("outage".into()) }).await.unwrap();
        assert_eq!(failed.metadata.resource_version, records.get(&name).await.unwrap().metadata.resource_version);
        let demand = reads.backend.using::<ForgeReadHeartbeat>("flotilla").get(&name).await.unwrap();
        assert_eq!(demand.spec.demanded_at, first.spec.demanded_at);
        clock.advance(READ_FRESHNESS);
        reads.read(&source, ForgeReadRequest::Board, || async { Ok(42_u64) }).await.unwrap();
        let recovered = records.get(&name).await.unwrap();
        assert_ne!(recovered.metadata.resource_version, failed.metadata.resource_version);
        assert!(recovered.status.unwrap().error.is_none());
    }

    // Replicated peers use the small owner's heartbeat, never call the forge,
    // and reject expired or mismatched-content heartbeats. A new owner can
    // publish equal facts immediately with its new authority (handoff test).
    #[tokio::test]
    async fn peer_freshness_comes_from_matching_heartbeat_and_expires() {
        let (owner, clock) = clocked_reads("owner");
        let source = source();
        let spec = ProjectSpec::builder()
            .display_name("Shared".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(source.clone()).alias("shared".into()).build()])
            .build();
        owner.backend.using::<Project>("flotilla").create(&meta("shared"), &spec).await.unwrap();
        let mut peer = ForgeReads::new(backend("peer"), "flotilla".into());
        peer.clock = clock.clone();
        replicate::<Project>(&owner.backend, &peer.backend).await;
        owner.read(&source, ForgeReadRequest::Board, || async { Ok(42_u64) }).await.unwrap();
        let name = forge_read_name(&source, &ForgeReadRequest::Board);
        let first = owner.backend.using::<ForgeRead>("flotilla").get(&name).await.unwrap();
        replicate::<ForgeRead>(&owner.backend, &peer.backend).await;
        replicate::<ForgeReadHeartbeat>(&owner.backend, &peer.backend).await;
        clock.advance(READ_HEARTBEAT_INTERVAL);
        owner.read(&source, ForgeReadRequest::Board, || async { Ok(42_u64) }).await.unwrap();
        assert_eq!(
            first.metadata.resource_version,
            owner.backend.using::<ForgeRead>("flotilla").get(&name).await.unwrap().metadata.resource_version
        );
        replicate::<ForgeReadHeartbeat>(&owner.backend, &peer.backend).await;
        assert_eq!(
            peer.read::<u64, _, _>(&source, ForgeReadRequest::Board, || async { panic!("peer must never load") }).await.unwrap(),
            42_u64
        );
        assert_eq!(peer.status(&name).await.unwrap().unwrap().observed_at, Some(clock.now()));
        clock.advance(READ_HEARTBEAT_MAX_AGE);
        let error =
            peer.read::<u64, _, _>(&source, ForgeReadRequest::Board, || async { panic!("peer must never load") }).await.unwrap_err();
        assert_eq!(error, "forge observation heartbeat stale");
        peer.allow_stale = true;
        assert_eq!(
            peer.read::<u64, _, _>(&source, ForgeReadRequest::Board, || async { panic!("peer must never load") }).await.unwrap(),
            42_u64
        );
        let pulses = owner.backend.using::<ForgeReadHeartbeat>("flotilla");
        let record = pulses.get(&name).await.unwrap();
        let mut wrong = record.status.unwrap();
        wrong.attempted_at = clock.now();
        wrong.content_at = clock.now();
        pulses.update_status(&name, &record.metadata.resource_version, &wrong).await.unwrap();
        replicate::<ForgeReadHeartbeat>(&owner.backend, &peer.backend).await;
        assert_eq!(peer.status(&name).await.unwrap().unwrap().error.as_deref(), Some("forge observation heartbeat stale"));
    }

    // Cancellation must release local coalescing and leave no replicated claim
    // that prevents the very next caller from obtaining a completed result.
    #[tokio::test]
    async fn cancelled_read_does_not_delay_next_read() {
        let reads = ForgeReads::new(backend("owner"), "flotilla".into());
        let source = source();
        let (entered, mut started) = tokio::sync::oneshot::channel();
        let mut first = Box::pin(reads.read(&source, ForgeReadRequest::Board, || async {
            entered.send(()).unwrap();
            std::future::pending::<Result<u64, String>>().await
        }));
        tokio::select! {
            result = &mut first => panic!("unexpected completion: {result:?}"),
            _ = &mut started => {}
        }
        drop(first);
        assert_eq!(reads.read(&source, ForgeReadRequest::Board, || async { Ok(42_u64) }).await.unwrap(), 42);
    }

    // A concurrent demand touch may conflict with the final status write.
    // Retrying must preserve the loaded value and stamp its completion time.
    #[tokio::test]
    async fn demand_touch_during_load_keeps_completed_result() {
        let reads = ForgeReads::new(backend("owner"), "flotilla".into());
        let source = source();
        let name = forge_read_name(&source, &ForgeReadRequest::Board);
        let completed = Arc::new(Mutex::new(None));
        let result = reads
            .read(&source, ForgeReadRequest::Board, || async {
                let records = reads.backend.using::<ForgeRead>("flotilla");
                let record = records.get(&name).await.unwrap();
                let mut spec = record.spec;
                spec.demanded_at = Utc::now();
                records.update(&InputMeta::from(&record.metadata), &record.metadata.resource_version, &spec).await.unwrap();
                *completed.lock().await = Some(Utc::now());
                Ok(42_u64)
            })
            .await
            .unwrap();
        assert_eq!(result, 42);
        let status = reads.backend.using::<ForgeRead>("flotilla").get(&name).await.unwrap().status.unwrap();
        assert_eq!(status.value, Some(serde_json::json!(42)));
        assert!(status.observed_at.unwrap() >= completed.lock().await.unwrap());
        assert!(status.observed_at.unwrap() >= status.attempted_at);
    }

    // A former owner must not publish its in-flight result. After handoff the
    // new owner can read immediately, without inheriting a five-minute claim.
    #[tokio::test]
    async fn handoff_during_load_leaves_new_owner_free_to_read() {
        let first = backend("first");
        let second = backend("second");
        let source = source();
        let spec = ProjectSpec::builder()
            .display_name("Shared".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(source.clone()).alias("shared".into()).build()])
            .build();
        first.using::<Project>("flotilla").create(&meta("shared"), &spec).await.unwrap();
        replicate::<Project>(&first, &second).await;
        for backend in [&first, &second] {
            let hosts = backend.using::<Host>("flotilla");
            let host = hosts.create(&meta("host"), &HostSpec::default()).await.unwrap();
            hosts
                .update_status(
                    "host",
                    &host.metadata.resource_version,
                    &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
                )
                .await
                .unwrap();
        }
        replicate::<Host>(&second, &first).await;
        let reads = ForgeReads::new(first.clone(), "flotilla".into());
        let result = reads
            .read(&source, ForgeReadRequest::Board, || async {
                let hosts = first.using::<Host>("flotilla");
                let host = hosts.get("host").await.unwrap();
                hosts
                    .update_status(
                        "host",
                        &host.metadata.resource_version,
                        &HostStatus { ready: false, heartbeat_at: Some(Utc::now()), ..Default::default() },
                    )
                    .await
                    .unwrap();
                Ok(1_u64)
            })
            .await;
        assert_eq!(result.unwrap_err(), "forge observer changed during read");
        replicate::<Host>(&first, &second).await;
        replicate::<ForgeRead>(&first, &second).await;
        let new_reads = ForgeReads::new(second, "flotilla".into());
        assert_eq!(new_reads.read(&source, ForgeReadRequest::Board, || async { Ok(2_u64) }).await.unwrap(), 2);
        assert!(first
            .using::<ForgeRead>("flotilla")
            .get(&forge_read_name(&source, &ForgeReadRequest::Board))
            .await
            .unwrap()
            .status
            .is_none());
    }

    // Replication arrival order cannot change the selected status, including
    // exact timestamp ties. Generate distinct authorities and duplicate rows.
    #[hegel::test]
    fn status_selection_is_independent_of_arrival_order(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let index = tc.draw(gs::integers::<u32>().min_value(0).max_value(1000));
        let now = Utc::now();
        let status = |authority: String| ForgeReadStatus {
            authority,
            attempted_at: now,
            observed_at: Some(now),
            value: Some(serde_json::json!(index)),
            error: None,
            retry_at: None,
        };
        let lower = status(format!("a-{index}"));
        let higher = status(format!("b-{index}"));
        assert_eq!(latest_status([lower.clone(), higher.clone(), lower.clone()]), Some(higher.clone()));
        assert_eq!(latest_status([higher.clone(), lower.clone(), lower]), Some(higher));
    }

    // A preferred origin that never publishes a Host must eventually yield to
    // a ready replica. Exercise expiry through real replicated declarations.
    #[tokio::test]
    async fn never_reporting_owner_yields_after_startup_grace() {
        let missing = backend("missing");
        let ready = backend("ready");
        let source = source();
        let spec = ProjectSpec::builder()
            .display_name("Shared".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(source.clone()).alias("shared".into()).build()])
            .build();
        missing.using::<Project>("flotilla").create(&meta("shared"), &spec).await.unwrap();
        replicate::<Project>(&missing, &ready).await;
        let hosts = ready.using::<Host>("flotilla");
        let host = hosts.create(&meta("host"), &HostSpec::default()).await.unwrap();
        hosts
            .update_status(
                "host",
                &host.metadata.resource_version,
                &HostStatus { ready: true, heartbeat_at: Some(Utc::now()), ..Default::default() },
            )
            .await
            .unwrap();
        assert_eq!(source_owner(&ready, "flotilla", &source).await.unwrap(), Some(missing.local_root().unwrap()));
        // The replica boundary supplies aged declarations without wall-clock sleeps.
        let mut projects = missing.using::<Project>("flotilla").list().await.unwrap();
        projects.items[0].metadata.creation_timestamp -= UNKNOWN_OWNER_GRACE + Duration::seconds(1);
        ready.replica_writer::<Project>(missing.local_root().unwrap(), "flotilla").replace(&projects, Utc::now()).await.unwrap();
        assert_eq!(source_owner(&ready, "flotilla", &source).await.unwrap(), Some(ready.local_root().unwrap()));
        missing.using::<Host>("flotilla").create(&meta("silent"), &HostSpec::default()).await.unwrap();
        let mut silent = missing.using::<Host>("flotilla").list().await.unwrap();
        silent.items[0].metadata.creation_timestamp -= UNKNOWN_OWNER_GRACE + Duration::seconds(1);
        ready.replica_writer::<Host>(missing.local_root().unwrap(), "flotilla").replace(&silent, Utc::now()).await.unwrap();
        assert_eq!(source_owner(&ready, "flotilla", &source).await.unwrap(), Some(ready.local_root().unwrap()));
    }

    // Equal-heartbeat contradictions must not revive an explicitly unready
    // origin. Generate fresh heartbeat ages and both arrival orders.
    #[hegel::test]
    fn equal_heartbeat_preserves_positive_unready_evidence(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(179));
        let heartbeat = Some(Utc::now() - Duration::seconds(age));
        let ready = (heartbeat, ObserverHealth::Ready);
        let unready = (heartbeat, ObserverHealth::Unready);
        assert!(ready.max(unready).1 == ObserverHealth::Unready);
        assert!(unready.max(ready).1 == ObserverHealth::Unready);
    }

    // Unknown ownership lasts only through its startup grace, even if no first
    // heartbeat ever arrives. Generate ages on both sides and at the boundary.
    #[hegel::test]
    fn unknown_owner_grace_is_bounded(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let age = tc.draw(gs::integers::<i64>().min_value(0).max_value(360));
        let now = Utc::now();
        assert_eq!(unknown_health(now - Duration::seconds(age), now) == ObserverHealth::Unknown, age < UNKNOWN_OWNER_GRACE.num_seconds());
        assert!(unknown_health(now - UNKNOWN_OWNER_GRACE, now) == ObserverHealth::Unready);
    }
}
