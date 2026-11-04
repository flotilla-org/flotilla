//! Source-scoped forge ownership and per-host forge reads.
use std::{
    collections::{btree_map::Entry, BTreeMap},
    future::Future,
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use flotilla_change_request::{BoundObservations, ChangeRequestAdmission, ChangeRequestTracker, CrewGithubLoginsByRequest};
use flotilla_issue_tracker::IssueProvider;
use flotilla_protocol::{
    issue_query::{IssueQuery, IssueResultPage},
    ChangeRequest, Issue, IssueChangeset, IssueRef, IssueSource, NodeId,
};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{
    normalize_issue_source, resolve_project_issue_sources_from_inventory, Clock, Forge, ForgeSpec, Host, HostStatus, IssueSourceResolution,
    Project, ProjectSpec, Repository, ResourceError, ResourceObject, ResourceProvenance, SystemClock,
};
use flotilla_store::ResourceBackend;
use serde::{de::DeserializeOwned, Serialize};
use tokio::sync::Mutex;

pub const OWNER_LOCAL_INCREMENTAL_READ_ERROR: &str = "incremental forge reads are owner-local";

/// Stable diagnostic shared with the load harness when no observer qualifies.
pub const NO_READY_FORGE_OBSERVER: &str = "no ready forge observer";

const HEARTBEAT_MAX_AGE: Duration = Duration::seconds(180);
const UNKNOWN_OWNER_GRACE: Duration = Duration::seconds(180);
const READ_FRESHNESS: Duration = Duration::seconds(60);
pub const LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// One forge read, keyed with its source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum ForgeReadRequest {
    Board,
    Query { params: IssueQuery, page: u32, count: usize },
    Issue { id: String },
    Changes { since: String, count: usize },
    Mission { id: String },
    DispatchFacts { id: String },
    Branch { branch: String },
    ChangeRequests { limit: usize },
    ChangeRequest { id: String },
    MergedBranches { limit: usize },
}

type ReadLock = Arc<Mutex<Option<LastRead>>>;

/// This host's latest completed read of one request.
struct LastRead {
    attempted_at: DateTime<Utc>,
    /// The last successful result, kept through later failures.
    value: Option<serde_json::Value>,
    error: Option<ObservationError>,
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

// Admission readiness is independent of observation. Liveness is checked by
// the election heartbeat limit; only explicit forge-affecting faults withdraw it.
fn observer_ready(status: &HostStatus) -> bool {
    !status.conditions.iter().any(|condition| condition.blocks_forge_observation())
}

/// What declares a source's observer: the forges, each Project with its declaration (its earliest
/// creation and the home that made it), and the repositories the Projects name, by key.
pub struct Declarations<H> {
    pub forges: Vec<ForgeSpec>,
    pub projects: Vec<(ProjectSpec, (DateTime<Utc>, H))>,
    pub repositories: BTreeMap<String, Result<ResourceObject<Repository>, ResourceError>>,
}

impl<H: Ord + Clone> Declarations<H> {
    /// The earliest declaration of a Project bound to `source`, by an issue-source binding, a
    /// resolved binding, or a member repository's forge identity; its home is the preferred observer.
    pub fn preferred_observer(&self, source: &IssueSource) -> Option<(DateTime<Utc>, H)> {
        let mut source = canonical_source(source);
        if let Some(forge) = self.forges.iter().find(|forge| forge.forge_id == source.service) {
            source.service = forge.https_url.clone();
        }
        let source = normalize_issue_source(&source);
        let mut homes = Vec::new();
        for (project, declaration) in &self.projects {
            if project.issue_source_bindings.iter().any(|binding| !binding.exclude && canonical_source(&binding.source) == source) {
                homes.push(declaration.clone());
            }
            if let IssueSourceResolution::Available { bindings } = resolve_project_issue_sources_from_inventory(&self.repositories, project)
            {
                if bindings.iter().any(|binding| canonical_source(&binding.source) == source) {
                    homes.push(declaration.clone());
                }
            }
            // PR observation follows Repository forge identity even if its issue
            // source has been overridden or excluded.
            for member in &project.repositories {
                if let Some(Ok(repository)) = self.repositories.get(&member.repo.to_string()) {
                    if repository.spec.forge().is_some_and(|forge| {
                        normalize_issue_source(&IssueSource { service: forge.service_url.clone(), scope: forge.repository.clone() })
                            == source
                    }) {
                        homes.push(declaration.clone());
                    }
                }
            }
        }
        homes.into_iter().min()
    }
}

async fn declarations(backend: &ResourceBackend, namespace: &str) -> Result<Declarations<NodeId>, String> {
    let forges =
        backend.definitions::<Forge>(namespace).list().await.map_err(|e| e.to_string())?.into_iter().map(|forge| forge.spec).collect();
    // Recover every declaration home in one store listing, rather than listing
    // every Project's replica sources again for each binding and repository.
    let mut project_homes = BTreeMap::new();
    for record in backend.including_replicas::<Project>(namespace).list_replica_sources().await.map_err(|e| e.to_string())?.items {
        let candidate = (record.object.metadata.creation_timestamp, origin(backend, &record.provenance)?);
        let entry = project_homes.entry(record.object.metadata.name).or_insert_with(|| candidate.clone());
        *entry = candidate.min(entry.clone());
    }
    let resolver = backend.including_replicas::<Repository>(namespace);
    let mut projects = Vec::new();
    let mut repositories = BTreeMap::new();
    for project in backend.definitions::<Project>(namespace).list().await.map_err(|e| e.to_string())? {
        for member in &project.spec.repositories {
            let key = member.repo.to_string();
            if let Entry::Vacant(entry) = repositories.entry(key) {
                let repository = resolver.get(entry.key()).await.map(|record| record.object);
                entry.insert(repository);
            }
        }
        if let Some(home) = project_homes.get(&project.metadata.name) {
            projects.push((project.spec, home.clone()));
        }
    }
    Ok(Declarations { forges, projects, repositories })
}

/// Shared sources elect once, regardless of how many Projects bind them.
/// Unknown host health preserves the preferred owner during startup; only
/// observer-unready/stale evidence or expiry of the startup grace permits
/// fallback. Admission readiness never withdraws forge observation.
pub async fn source_owner(backend: &ResourceBackend, namespace: &str, source: &IssueSource) -> Result<Option<NodeId>, String> {
    let now = Utc::now();
    let preferred = declarations(backend, namespace).await?.preferred_observer(source);
    let mut ready = BTreeMap::new();
    for host in backend.including_replicas::<Host>(namespace).list_replica_sources().await.map_err(|e| e.to_string())?.items {
        if !matches!(host.object.spec.connection, flotilla_resources::HostConnection::Daemon) {
            continue;
        }
        let root = origin(backend, &host.provenance)?;
        let heartbeat = host.object.status.as_ref().and_then(|status| status.heartbeat_at);
        let health = match host.object.status.as_ref() {
            None => unknown_health(host.object.metadata.creation_timestamp, now),
            Some(status) if !observer_ready(status) => ObserverHealth::Unready,
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
        .ok_or_else(|| NO_READY_FORGE_OBSERVER.into())
}

pub(crate) async fn owns_source(backend: &ResourceBackend, namespace: &str, source: &IssueSource) -> Result<bool, String> {
    match source_owner(backend, namespace, source).await? {
        Some(owner) => Ok(owner == backend.local_root().map_err(|e| e.to_string())?),
        None => Ok(true),
    }
}

#[derive(Clone)]
pub struct ForgeReads {
    pub backend: ResourceBackend,
    pub namespace: String,
    pub allow_stale: bool,
    reads: Arc<Mutex<BTreeMap<String, ReadLock>>>,
    clock: Arc<dyn Clock>,
}
impl ForgeReads {
    pub fn new(backend: ResourceBackend, namespace: String) -> Self {
        Self { backend, namespace, allow_stale: false, reads: Default::default(), clock: Arc::new(SystemClock) }
    }
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
    pub async fn read<T, F, Fut>(&self, source: &IssueSource, request: ForgeReadRequest, load: F) -> Result<T, String>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, String>> + Send,
    {
        self.read_typed(source, request, || async { load().await.map_err(ObservationError::from) }).await.map_err(|error| error.to_string())
    }
    pub async fn read_typed<T, F, Fut>(&self, source: &IssueSource, request: ForgeReadRequest, load: F) -> Result<T, ObservationError>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T, ObservationError>> + Send,
    {
        let source = canonical_source(source);
        // Incremental cursors change every poll and belong to the owner's
        // materialization loop. Never cache one result per cursor.
        if matches!(request, ForgeReadRequest::Changes { .. }) {
            if !owns_source(&self.backend, &self.namespace, &source).await? {
                return Err(OWNER_LOCAL_INCREMENTAL_READ_ERROR.into());
            }
            let result = tokio::time::timeout(LOAD_TIMEOUT, load()).await.unwrap_or_else(|_| Err("forge observation timed out".into()));
            if !owns_source(&self.backend, &self.namespace, &source).await? {
                return Err("forge observer changed during read".into());
            }
            return result;
        }
        let lock = self.lock(&source, &request).await;
        let mut last = lock.lock().await;
        let now = self.clock.now();
        let fresh = last.as_ref().is_some_and(|last| {
            now.signed_duration_since(last.attempted_at) < READ_FRESHNESS
                || last.error.as_ref().and_then(ObservationError::retry_at).is_some_and(|at| at > now)
        });
        if !fresh {
            let result = tokio::time::timeout(LOAD_TIMEOUT, load())
                .await
                .unwrap_or_else(|_| Err("forge observation timed out".into()))
                .and_then(|value| serde_json::to_value(value).map_err(|error| ObservationError::from(error.to_string())));
            let previous = last.take().and_then(|last| last.value);
            let attempted_at = self.clock.now();
            *last = Some(match result {
                Ok(value) => LastRead { attempted_at, value: Some(value), error: None },
                Err(error) => LastRead { attempted_at, value: previous, error: Some(error) },
            });
        }
        let last = last.as_ref().expect("a read completed under this lock");
        if let Some(error) = last.error.as_ref().filter(|_| !self.allow_stale || last.value.is_none()) {
            return Err(error.clone());
        }
        let value = last.value.clone().expect("a read without an error holds a value");
        serde_json::from_value(value).map_err(|error| ObservationError::from(error.to_string()))
    }
    /// The error of this host's latest read of `request`, if it failed.
    pub async fn last_error(&self, source: &IssueSource, request: &ForgeReadRequest) -> Option<String> {
        let lock = self.lock(&canonical_source(source), request).await;
        let last = lock.lock().await;
        last.as_ref().and_then(|last| last.error.as_ref()).map(ToString::to_string)
    }
    async fn lock(&self, source: &IssueSource, request: &ForgeReadRequest) -> ReadLock {
        let key = serde_json::json!([source, request]).to_string();
        self.reads.lock().await.entry(key).or_default().clone()
    }
}

pub struct ObservedIssueProvider {
    pub inner: Arc<dyn IssueProvider>,
    pub reads: ForgeReads,
}
#[async_trait]
impl IssueProvider for ObservedIssueProvider {
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
        board.refresh_error = reads.last_error(source, &ForgeReadRequest::Board).await;
        Ok(board)
    }
    fn board_includes_mission_fields(&self) -> bool {
        self.inner.board_includes_mission_fields()
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

pub struct ObservedChangeRequestTracker {
    pub inner: Arc<dyn ChangeRequestTracker>,
    pub reads: ForgeReads,
    pub source: IssueSource,
}
#[async_trait]
impl ChangeRequestTracker for ObservedChangeRequestTracker {
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
            .read_typed(&self.source, ForgeReadRequest::Branch { branch: branch.into() }, || {
                self.inner.find_change_request_by_branch(branch)
            })
            .await
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use flotilla_resources::{HostSpec, HostStatus, InputMeta, IssueSourceBindingSpec, ProjectSpec, Resource};
    use flotilla_store_memory::InMemoryBackend;
    use flotilla_store_testkit::VirtualClock;

    use super::*;

    fn backend(root: &str) -> ResourceBackend {
        ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new(root))
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
    async fn declare_at(owner: &ResourceBackend) {
        let spec = ProjectSpec::builder()
            .display_name("Shared".into())
            .issue_source_bindings(vec![IssueSourceBindingSpec::builder().source(source()).alias("shared".into()).build()])
            .build();
        owner.using::<Project>("flotilla").create(&meta("shared"), &spec).await.expect("owner declaration");
    }
    fn clocked_reads(root: &str) -> (ForgeReads, Arc<VirtualClock>) {
        let clock = Arc::new(VirtualClock::new(Utc::now()));
        (ForgeReads::new(backend(root), "flotilla".into()).with_clock(clock.clone()), clock)
    }

    /// A forge that answers each call with its current response and counts the calls.
    struct CountingForge {
        calls: AtomicUsize,
        response: std::sync::Mutex<Result<u64, ObservationError>>,
    }
    impl CountingForge {
        fn new(response: Result<u64, ObservationError>) -> Self {
            Self { calls: AtomicUsize::new(0), response: std::sync::Mutex::new(response) }
        }
        fn respond(&self, response: Result<u64, ObservationError>) {
            *self.response.lock().unwrap() = response;
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
        async fn board(&self, reads: &ForgeReads) -> Result<u64, ObservationError> {
            reads
                .read_typed(&source(), ForgeReadRequest::Board, || async {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    tokio::task::yield_now().await;
                    self.response.lock().unwrap().clone()
                })
                .await
        }
    }

    #[tokio::test]
    async fn concurrent_reads_of_one_request_make_one_forge_call() {
        let (reads, _) = clocked_reads("host");
        let forge = CountingForge::new(Ok(7));
        let (first, second) = tokio::join!(forge.board(&reads), forge.board(&reads));
        assert_eq!((first.unwrap(), second.unwrap()), (7, 7));
        assert_eq!(forge.calls(), 1);
    }

    #[hegel::test]
    fn a_read_calls_the_forge_only_once_its_last_result_is_stale(tc: hegel::TestCase) {
        let elapsed = tc.draw(hegel::generators::integers::<i64>().min_value(0).max_value(3 * READ_FRESHNESS.num_seconds()));
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (reads, clock) = clocked_reads("host");
            let forge = CountingForge::new(Ok(1));
            assert_eq!(forge.board(&reads).await.unwrap(), 1);
            forge.respond(Ok(2));
            clock.advance(Duration::seconds(elapsed));
            let stale = Duration::seconds(elapsed) >= READ_FRESHNESS;
            assert_eq!(forge.board(&reads).await.unwrap(), if stale { 2 } else { 1 });
            assert_eq!(forge.calls(), 1 + usize::from(stale));
        });
    }

    #[hegel::test]
    fn a_failed_read_returns_the_last_good_value_only_when_stale_is_allowed(tc: hegel::TestCase) {
        let allow_stale = tc.draw(hegel::generators::booleans());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (mut reads, clock) = clocked_reads("host");
            reads.allow_stale = allow_stale;
            let forge = CountingForge::new(Ok(1));
            forge.board(&reads).await.unwrap();
            forge.respond(Err("outage".into()));
            clock.advance(READ_FRESHNESS);
            let result = forge.board(&reads).await;
            assert_eq!(forge.calls(), 2);
            if allow_stale {
                assert_eq!(result.unwrap(), 1);
            } else {
                assert_eq!(result.unwrap_err().to_string(), "outage");
            }
            assert_eq!(reads.last_error(&source(), &ForgeReadRequest::Board).await.as_deref(), Some("outage"));
        });
    }

    #[hegel::test]
    fn a_future_retry_time_suppresses_the_forge_call(tc: hegel::TestCase) {
        let wait = tc.draw(hegel::generators::integers::<i64>().min_value(READ_FRESHNESS.num_seconds()).max_value(3600));
        let elapsed = tc.draw(hegel::generators::integers::<i64>().min_value(0).max_value(wait + 60));
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (reads, clock) = clocked_reads("host");
            let retry_at = clock.now() + Duration::seconds(wait);
            let limited = ObservationError::RateLimited { budget: "forge".into(), retry_at: Some(retry_at), detail: "quota".into() };
            let forge = CountingForge::new(Err(limited.clone()));
            assert_eq!(forge.board(&reads).await.unwrap_err(), limited);
            forge.respond(Ok(1));
            clock.advance(Duration::seconds(elapsed));
            let suppressed = clock.now() < retry_at;
            let result = forge.board(&reads).await;
            assert_eq!(forge.calls(), 1 + usize::from(!suppressed));
            if suppressed {
                assert_eq!(result.unwrap_err(), limited);
            } else {
                assert_eq!(result.unwrap(), 1);
            }
        });
    }

    #[tokio::test]
    async fn a_host_that_does_not_own_the_source_reads_from_its_own_call() {
        let owner = backend("owner");
        let peer = backend("peer");
        declare_at(&owner).await;
        replicate::<Project>(&owner, &peer).await;
        assert_eq!(source_owner(&peer, "flotilla", &source()).await.unwrap(), Some(NodeId::new("owner")));
        let reads = ForgeReads::new(peer, "flotilla".into());
        let forge = CountingForge::new(Ok(42));
        assert_eq!(forge.board(&reads).await.unwrap(), 42);
        assert_eq!(forge.calls(), 1);
    }

    #[tokio::test]
    async fn cancelled_read_does_not_delay_next_read() {
        let reads = ForgeReads::new(backend("host"), "flotilla".into());
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

    #[hegel::test]
    fn owner_incremental_reads_call_the_forge_every_time(tc: hegel::TestCase) {
        let polls = tc.draw(hegel::generators::integers::<usize>().min_value(1).max_value(6));
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let reads = ForgeReads::new(backend("owner"), "flotilla".into());
            let calls = AtomicUsize::new(0);
            for _ in 0..polls {
                reads
                    .read(&source(), ForgeReadRequest::Changes { since: "cursor".into(), count: 50 }, || async {
                        Ok(calls.fetch_add(1, Ordering::SeqCst))
                    })
                    .await
                    .unwrap();
            }
            assert_eq!(calls.load(Ordering::SeqCst), polls);
        });
    }

    #[tokio::test]
    async fn nonowner_incremental_read_is_refused() {
        let owner = backend("owner");
        let peer = backend("peer");
        declare_at(&owner).await;
        replicate::<Project>(&owner, &peer).await;
        let reads = ForgeReads::new(peer, "flotilla".into());
        assert_eq!(
            reads
                .read::<u32, _, _>(&source(), ForgeReadRequest::Changes { since: "cursor".into(), count: 50 }, || async {
                    panic!("nonowner incremental loader")
                })
                .await
                .expect_err("nonowner refusal"),
            OWNER_LOCAL_INCREMENTAL_READ_ERROR
        );
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

    // Admission readiness and condition names never affect observer eligibility.
    // A false observation-blocking condition withdraws even an admission-ready host.
    #[hegel::test]
    fn observer_readiness_is_independent_of_admission(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let ready = tc.draw(gs::booleans());
        // Empty, duplicate, arbitrary names, both classification bits and
        // healthy/failed values; check after each appended condition.
        let count = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
        let mut status = HostStatus { ready, ..Default::default() };
        let mut withdrawn = false;
        assert!(observer_ready(&status));
        for _ in 0..count {
            let admission = tc.draw(gs::booleans());
            let observation = tc.draw(gs::booleans());
            let value = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            let value = [
                flotilla_resources::ConditionValue::False,
                flotilla_resources::ConditionValue::True,
                flotilla_resources::ConditionValue::Unknown,
            ][value];
            let name = tc.draw(gs::integers::<usize>().min_value(0).max_value(2));
            withdrawn |= observation && value == flotilla_resources::ConditionValue::False;
            status.conditions.push(
                flotilla_resources::HostCondition::builder()
                    .condition_type([flotilla_resources::PROJECTION_PARITY_CONDITION, "CapabilityRegression", "Unknown"][name])
                    .reason("arbitrary")
                    .message("test fault")
                    .observed_at(Utc::now())
                    .value(value)
                    .blocks_readiness(admission)
                    .blocks_forge_observation(observation)
                    .build(),
            );
            assert_eq!(observer_ready(&status), !withdrawn);
        }
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

#[cfg(test)]
mod forgejo_tests {
    use super::*;
    use flotilla_fake_forge::{recorded::ManualClock, FakeForge, ForgeSettings, InProcessClient};
    use flotilla_forge_forgejo::{ForgejoAuth, ForgejoIssueProviderConfig};
    use flotilla_forgejo_providers::change_request::ForgejoChangeRequestProvider;
    use flotilla_provider_runtime::{ChannelLabel, HttpClient};
    use flotilla_replay_testkit::testing::MockRunner;
    use flotilla_store_memory::InMemoryBackend;

    // HTTP boundary: the strict Forgejo fake validates the request before a quota response is injected.
    struct ThrottledForge(InProcessClient);
    #[async_trait]
    impl HttpClient for ThrottledForge {
        async fn execute(&self, request: reqwest::Request, label: &ChannelLabel) -> Result<http::Response<bytes::Bytes>, String> {
            let response = self.0.execute(request, label).await?;
            assert!(response.status().is_success(), "request must satisfy Forgejo's HTTP contract");
            Ok(http::Response::builder()
                .status(429)
                .header("x-ratelimit-reset", "1893456000")
                .body(bytes::Bytes::from_static(b"rate limit exceeded"))
                .expect("quota response"))
        }
    }

    // #3300: Forgejo's typed quota deadline is recorded just like any other provider's,
    // survives a cache hit, and suppresses further HTTP reads until that deadline.
    // One example suffices for this adapter-to-cache glue; cooldown boundaries have property coverage.
    #[tokio::test]
    async fn forgejo_rate_limit_records_retry_time_without_github_parsing() {
        let clock = Arc::new(ManualClock::new("2026-10-16T12:00:00Z".parse().expect("time")));
        let fake = Arc::new(FakeForge::new("https://forgejo.example", clock));
        fake.add_forge("lab", ForgeSettings { token: "test-token".into(), login: "bot".into() }).expect("forge");
        let source = IssueSource { service: fake.service_url("lab"), scope: "team/repo".into() };
        let provider = ForgejoChangeRequestProvider::new(
            Arc::new(ThrottledForge(InProcessClient::new(fake))),
            Arc::new(MockRunner::new(vec![])),
            ForgejoIssueProviderConfig::new(
                source.service.clone(),
                None,
                ForgejoAuth { token: "test-token".into(), token_path: "unused".into() },
            ),
            source.scope.clone(),
        );
        let backend = ResourceBackend::new(InMemoryBackend::default()).with_local_root(NodeId::new("owner"));
        let reads = ForgeReads::new(backend, "flotilla".into());
        let request = ForgeReadRequest::Branch { branch: "work".into() };
        let error = reads.read_typed(&source, request.clone(), || provider.find_change_request_by_branch("work")).await.expect_err("quota");
        let expected = DateTime::from_timestamp(1893456000, 0).expect("deadline");
        assert_eq!(error.retry_at(), Some(expected));
        assert_eq!(reads.last_error(&source, &request).await, Some(error.to_string()));
        let cached = reads
            .read_typed::<Option<(String, ChangeRequest)>, _, _>(&source, request, || async { panic!("quota must suppress HTTP") })
            .await
            .expect_err("cached quota");
        assert_eq!(cached, error);
        assert_eq!(cached.retry_at(), Some(expected));
    }
}
