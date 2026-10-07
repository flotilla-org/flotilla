//! Source-scoped forge ownership and replicated reads (ADR 0057).
use std::{collections::BTreeMap, future::Future, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    Issue, IssueChangeset, IssueRef, IssueSource, NodeId,
};
use flotilla_resources::{
    forge_read_name, normalize_issue_source, resolve_project_issue_sources, ForgeRead, ForgeReadRequest, ForgeReadSpec, ForgeReadStatus,
    Host, InputMeta, IssueSourceResolution, Project, Repository, ResourceBackend, ResourceError, ResourceProvenance,
};
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::Mutex;

use crate::providers::{
    change_request::{BoundObservations, ChangeRequestAdmission, ChangeRequestTracker, CrewGithubLoginsByRequest, ObservationError},
    github_api::{classified_rate_error, rate_limit_reset},
    issue_tracker::IssueProvider,
    types::ChangeRequest,
};

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
/// positive unready/stale evidence permits fallback to a known ready daemon.
pub async fn source_owner(backend: &ResourceBackend, namespace: &str, source: &IssueSource) -> Result<Option<NodeId>, String> {
    let mut source = canonical_source(source);
    for forge in backend.definitions::<flotilla_resources::Forge>(namespace).list().await.map_err(|e| e.to_string())? {
        if forge.spec.forge_id == source.service {
            source.service = forge.spec.https_url;
            break;
        }
    }
    let source = normalize_issue_source(&source);
    let mut homes = Vec::new();
    let repositories = backend.including_replicas::<Repository>(namespace);
    for project in backend.definitions::<Project>(namespace).list().await.map_err(|e| e.to_string())? {
        if project.spec.issue_source_bindings.iter().any(|binding| !binding.exclude && canonical_source(&binding.source) == source) {
            if let Some(home) = project_home(backend, namespace, &project.metadata.name).await? {
                homes.push((project.metadata.creation_timestamp, home));
            }
        }
        if let IssueSourceResolution::Available { bindings } = resolve_project_issue_sources(&repositories, &project.spec).await {
            if bindings.iter().any(|binding| canonical_source(&binding.source) == source) {
                if let Some(home) = project_home(backend, namespace, &project.metadata.name).await? {
                    homes.push((project.metadata.creation_timestamp, home));
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
                    if let Some(home) = project_home(backend, namespace, &project.metadata.name).await? {
                        homes.push((project.metadata.creation_timestamp, home));
                    }
                }
            }
        }
    }
    let preferred = homes.into_iter().min().map(|(_, root)| root);
    let mut ready = BTreeMap::new();
    for host in backend.including_replicas::<Host>(namespace).list_replica_sources().await.map_err(|e| e.to_string())?.items {
        if !matches!(host.object.spec.connection, flotilla_resources::HostConnection::Daemon) {
            continue;
        }
        let root = origin(backend, &host.provenance)?;
        let heartbeat = host.object.status.as_ref().and_then(|status| status.heartbeat_at);
        let health = match host.object.status.as_ref() {
            None => ObserverHealth::Unknown,
            Some(status) if !status.ready => ObserverHealth::Unready,
            Some(_) => match heartbeat {
                Some(at) if Utc::now().signed_duration_since(at) < chrono::Duration::seconds(180) => ObserverHealth::Ready,
                Some(_) => ObserverHealth::Unready,
                None => ObserverHealth::Unknown,
            },
        };
        let candidate = (heartbeat, health);
        let entry = ready.entry(root).or_insert(candidate);
        if candidate > *entry {
            *entry = candidate;
        }
    }
    if let Some(preferred) = preferred {
        if ready.get(&preferred).is_none_or(|(_, health)| *health != ObserverHealth::Unready) {
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
    locks: Arc<Mutex<BTreeMap<String, Arc<Mutex<()>>>>>,
}
impl ForgeReads {
    pub fn new(backend: ResourceBackend, namespace: String) -> Self {
        Self { backend, namespace, touch_demand: true, allow_stale: false, locks: Default::default() }
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
        let _guard = lock.lock().await;
        let records = self.backend.using::<ForgeRead>(&self.namespace);
        let now = Utc::now();
        let spec = ForgeReadSpec { source: source.clone(), request, demanded_at: now };
        let current = match records.get(&name).await {
            Ok(current) => {
                if self.touch_demand && now.signed_duration_since(current.spec.demanded_at).num_seconds() >= 60 {
                    records
                        .update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec)
                        .await
                        .map_err(|e| e.to_string())?
                } else {
                    current
                }
            }
            Err(ResourceError::NotFound { .. }) => {
                records.create(&InputMeta::builder().name(name.clone()).build(), &spec).await.map_err(|e| e.to_string())?
            }
            Err(e) => return Err(e.to_string()),
        };
        let statuses = self.backend.including_replicas::<ForgeRead>(&self.namespace).get_all(&name).await.map_err(|e| e.to_string())?;
        let previous = statuses
            .items
            .into_iter()
            .filter_map(|record| record.object.status)
            .max_by_key(|status| (status.attempted_at, status.authority.clone()));
        let owner = owns_source(&self.backend, &self.namespace, &source).await?;
        let fresh = previous.as_ref().is_some_and(|status| {
            now.signed_duration_since(status.attempted_at).num_seconds() < 60 || status.retry_at.is_some_and(|at| at > now)
        });
        if !owner || fresh {
            return previous
                .ok_or_else(|| "forge observation pending at source owner".to_string())
                .and_then(|status| decode(status, self.allow_stale));
        }
        let pending = ForgeReadStatus {
            authority: self.backend.local_root().map_err(|e| e.to_string())?.to_string(),
            attempted_at: now,
            observed_at: previous.as_ref().and_then(|status| status.observed_at),
            value: previous.as_ref().and_then(|status| status.value.clone()),
            error: Some("forge observation in progress".into()),
            retry_at: Some(now + chrono::Duration::seconds(300)),
        };
        records.update_status(&name, &current.metadata.resource_version, &pending).await.map_err(|e| e.to_string())?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(300), load())
            .await
            .unwrap_or_else(|_| Err("forge observation timed out".into()));
        // Handoff during a slow read cannot publish facts from the former owner.
        if !owns_source(&self.backend, &self.namespace, &source).await? {
            return Err("forge observer changed during read".into());
        }
        let (value, observed_at, error, retry_at) = match result {
            Ok(value) => (Some(serde_json::to_value(value).map_err(|e| e.to_string())?), Some(now), None, None),
            Err(error) => (
                previous.as_ref().and_then(|s| s.value.clone()),
                previous.as_ref().and_then(|s| s.observed_at),
                Some(error.clone()),
                rate_limit_reset(&error),
            ),
        };
        let status = ForgeReadStatus {
            authority: self.backend.local_root().map_err(|e| e.to_string())?.to_string(),
            attempted_at: Utc::now(),
            value,
            observed_at,
            error,
            retry_at,
        };
        let current = records.get(&name).await.map_err(|e| e.to_string())?;
        records.update_status(&name, &current.metadata.resource_version, &status).await.map_err(|e| e.to_string())?;
        decode(status, self.allow_stale)
    }
}
fn decode<T: DeserializeOwned>(status: ForgeReadStatus, allow_stale: bool) -> Result<T, String> {
    if !allow_stale {
        if let Some(error) = &status.error {
            return Err(error.clone());
        }
    }
    if let Some(value) = status.value {
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
        let statuses = self
            .reads
            .backend
            .including_replicas::<ForgeRead>(&self.reads.namespace)
            .get_all(&forge_read_name(&normalize_issue_source(source), &ForgeReadRequest::Board))
            .await
            .map_err(|e| e.to_string())?;
        if let Some(status) = statuses.items.into_iter().filter_map(|record| record.object.status).max_by_key(|status| status.attempted_at)
        {
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
        for record in backend.including_replicas::<ForgeRead>(&namespace).list().await.map_err(|e| e.to_string())?.items {
            if Utc::now().signed_duration_since(record.object.spec.demanded_at).num_seconds() < 180 {
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
            if Utc::now().signed_duration_since(record.spec.demanded_at).num_seconds() >= 3600 {
                local.delete(&record.metadata.name).await.map_err(|e| e.to_string())?;
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
