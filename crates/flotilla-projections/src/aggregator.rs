//! Resource-store Aggregator maintaining named-query result sets.
//! Convoy events update source keys; secondary joins use maintained key indexes.
//! See [workload and assumptions](../../../docs/implementation-choices/convoy-event-cost.md).

use flotilla_daemon_api::event_sink::EventSink;
use flotilla_observation::convoy_branch_refresh::ConvoyBranchRefresh;
#[cfg(test)]
use flotilla_store_memory::InMemoryBackend;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use crate::{
    aggregator_projection::AggregatorProjectionState,
    ports::{AgentSessionKey, AgentSessionUpdate},
    salience::{AttentionFact, RaisedFact, SalienceFacts},
    terminal_health,
};
use async_trait::async_trait;
use flotilla_charter::ops_entry::ENSURED_FROM_ANNOTATION;
#[cfg(test)]
use flotilla_daemon_api::event_sink::BroadcastEventSink;
use flotilla_protocol::{
    result_set::{
        CheckoutRow, ConvoyChangeRequest, ConvoyPhase, ConvoyRow, CrewMemberSummary, IndependentRow, ProjectRepositoriesRow,
        ProjectRepositoryMembership, QueryChanges, QueryId, QueryScope, ReadinessBlocker, ReadinessState, ResultDelta, SessionPhase,
        StandingRoleRow, SurfaceState, VesselRow, WorkPhase,
    },
    Change, ConfiguredResourceLimits, DaemonEvent, EnvironmentKind, HostName, LifecycleAuthority, RepoDelta, RepoIdentity, RepositoryKey,
    ResourceRef, UNKNOWN_REPOSITORY_LABEL,
};
use flotilla_resources::agent_activity::{self, AgentActivityView};
use flotilla_resources::attentions::{Attentions, OpenAttention, Sender, Subject as AttentionSubject, HOLD_ESCALATES_AFTER};
use flotilla_resources::discovered_checkouts::DiscoveredCheckouts;
use flotilla_resources::environment_records::EnvironmentChange;
use flotilla_resources::{
    api_version, convoy_subject_rows, repository_display_labels, subject_relationship_conflicts, Checkout, CheckoutSpec,
    Clone as CloneResource, Convoy, ConvoyEnsure, ConvoyPhase as ResourceConvoyPhase, ConvoyStatus, CrewSource, FulfilmentKind,
    FulfilmentRealisation, Project, ReadResourceObject, ReadWatchEvent, Repository, Resource, ResourceError, ResourceObject,
    ResourceProvenance, StallRung, StalledCondition, TerminalSession, TerminalSessionPhase, Vessel, VesselRequirement,
    WorkPhase as ResourceWorkPhase, WorkState, CONVOY_LABEL, CONVOY_UNREADABLE_ANNOTATION, REPO_KEY_LABEL, REPO_LABEL, ROLE_LABEL,
    VESSEL_LABEL,
};
#[cfg(test)]
use flotilla_resources::{ReadResourceList, ResourceList, WatchEvent, WatchStart, WatchStream};
#[cfg(test)]
use flotilla_store::LocalViewSource as AggregatorWatchSource;
use flotilla_store::ReplicaReadResolver;
use flotilla_store::{recover_view, ViewSource as AggregatorReplicaWatchSource};
use futures::{stream::BoxStream, FutureExt, StreamExt};
use tokio::{
    sync::{broadcast, mpsc},
    time::{Duration, Instant},
};
use tracing::debug;

use crate::issue_materializer::{IssueMaterializationResolver, IssueMaterializer, IssuePollingHealth};

mod convoy_inputs;
use convoy_inputs::ConvoyInputs;
mod readiness;
use flotilla_resources::checkout_records::latest_launches;
use readiness::{apply_readiness_event, next_checkout_change, readiness_resources, HeldCheckouts};

type RepositorySourceKey = (String, String, Option<flotilla_protocol::NodeId>);

type ConvoyKey = (String, String, Option<flotilla_protocol::NodeId>);
type EnsureKey = (String, String, Option<flotilla_protocol::NodeId>);
type SessionKey = (String, String, Option<flotilla_protocol::NodeId>);
const CONVOY_PROJECTION_COALESCE_INTERVAL: Duration = Duration::from_millis(25);
/// How soon a convoy whose engine read failed is projected again.
const ENGINE_READ_RETRY: Duration = Duration::from_secs(1);

const CHANGE_REQUEST_MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// A lookup that runs longer fails, so a hung resolver cannot hold a convoy's
/// refresh slot; matches the forge observation load timeout.
const CHANGE_REQUEST_LOOKUP_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CHANGE_REQUEST_MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30 * 60);

#[derive(bon::Builder)]
pub struct AggregatorResolvers {
    durable_convoys: ReplicaReadResolver<Convoy>,
    durable_convoy_ensures: ReplicaReadResolver<ConvoyEnsure>,
    durable_fulfilment_kinds: Option<ReplicaReadResolver<FulfilmentKind>>,
    durable_sessions: ReplicaReadResolver<TerminalSession>,
    durable_projects: ReplicaReadResolver<Project>,
    durable_fleet_designation: Option<ReplicaReadResolver<flotilla_resources::FleetDesignation>>,
    durable_repositories: ReplicaReadResolver<Repository>,
    durable_vessels: ReplicaReadResolver<Vessel>,
    durable_hosts: ReplicaReadResolver<flotilla_resources::Host>,
}

#[derive(bon::Builder)]
struct AggregatorSourceRefs<'a> {
    durable_convoys: &'a dyn AggregatorReplicaWatchSource<Convoy>,
    durable_convoy_ensures: &'a dyn AggregatorReplicaWatchSource<ConvoyEnsure>,
    durable_fulfilment_kinds: Option<&'a dyn AggregatorReplicaWatchSource<FulfilmentKind>>,
    durable_sessions: &'a dyn AggregatorReplicaWatchSource<TerminalSession>,
    durable_projects: &'a dyn AggregatorReplicaWatchSource<Project>,
    durable_fleet_designation: Option<&'a dyn AggregatorReplicaWatchSource<flotilla_resources::FleetDesignation>>,
    durable_repositories: &'a dyn AggregatorReplicaWatchSource<Repository>,
    durable_vessels: &'a dyn AggregatorReplicaWatchSource<Vessel>,
    durable_hosts: &'a dyn AggregatorReplicaWatchSource<flotilla_resources::Host>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LocalSource {
    Durable,
    Observed,
}

const LOCAL_SOURCE_PRECEDENCE: [LocalSource; 2] = [LocalSource::Durable, LocalSource::Observed];

#[async_trait]
pub trait AttachCapabilityResolver: Send + Sync {
    async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String>;

    async fn origin_host_names(&self, origins: &HashSet<flotilla_protocol::NodeId>) -> HashMap<flotilla_protocol::NodeId, HostName> {
        let _ = origins;
        HashMap::new()
    }
}

/// A lookup bounded by [`CHANGE_REQUEST_LOOKUP_TIMEOUT`]; on timeout every
/// repository reports the failure, so discovery fails and retries with backoff.
async fn bounded_refresh(
    resolver: &dyn ConvoyChangeRequestResolver,
    repositories: &[RepositoryKey],
    branch: &str,
    binding: Option<&flotilla_resources::BoundChangeRequest>,
) -> ConvoyBranchRefresh {
    match tokio::time::timeout(CHANGE_REQUEST_LOOKUP_TIMEOUT, resolver.refresh_branch(repositories, branch, binding)).await {
        Ok(refresh) => refresh,
        Err(_) => {
            let error = format!("change request lookup timed out after {CHANGE_REQUEST_LOOKUP_TIMEOUT:?}");
            ConvoyBranchRefresh {
                primary: Err(error.clone()),
                repositories: repositories.iter().map(|repository| (repository.clone(), Err(error.clone()))).collect(),
            }
        }
    }
}

#[async_trait]
pub trait ConvoyChangeRequestResolver: Send + Sync {
    async fn refresh_branch(
        &self,
        repositories: &[RepositoryKey],
        branch: &str,
        binding: Option<&flotilla_resources::BoundChangeRequest>,
    ) -> ConvoyBranchRefresh;

    async fn discover_branch_subjects(
        &self,
        _namespace: &str,
        _convoy: &str,
        _branch: &str,
        _resolution: ConvoyBranchRefresh,
    ) -> Result<(), String> {
        Ok(())
    }

    /// A tick whenever any home's change request records change: the forge observers' writes,
    /// including what their branch probes find (#3108). None when there are none to follow.
    async fn change_request_changes(&self) -> Option<futures::stream::BoxStream<'static, ()>> {
        None
    }
}

#[derive(Debug, Clone, Copy, Default)]
enum ConvoyProjectionDelivery {
    /// Delivers synchronously during bootstrap, before the runtime loop starts.
    #[default]
    Immediate,
    Coalesced {
        deadline: Option<Instant>,
    },
}

#[derive(bon::Builder)]
pub struct Aggregator {
    #[cfg(test)]
    #[builder(skip)]
    copied_convoys: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    #[builder(skip)]
    summarized_convoys: std::sync::atomic::AtomicUsize,
    state: AggregatorProjectionState,
    local_host: HostName,
    #[builder(skip)]
    convoys: HashMap<ConvoyKey, Arc<ReadResourceObject<Convoy>>>,
    #[builder(skip)]
    attentions: Vec<OpenAttention>,
    #[builder(skip)]
    attention_namespace: String,
    #[builder(skip)]
    attention_port: Option<Arc<dyn Attentions>>,
    #[builder(skip)]
    convoy_keys_by_name: HashMap<(String, String), HashSet<ConvoyKey>>,
    #[builder(skip)]
    sessions_by_source: HashMap<LocalSource, HashMap<SessionKey, ReadResourceObject<TerminalSession>>>,
    terminal_sessions: HashMap<SessionKey, ReadResourceObject<TerminalSession>>,
    attachable_sessions: HashSet<SessionKey>,
    origin_hosts: HashMap<flotilla_protocol::NodeId, HostName>,
    projects: HashMap<(String, String), ResourceObject<Project>>,
    #[builder(skip)]
    fleet_projects: BTreeMap<String, String>,
    #[builder(skip)]
    convoy_ensures: BTreeMap<EnsureKey, ResourceObject<ConvoyEnsure>>,
    /// Reads each declared role's state from its generations (#3108).
    #[builder(skip = Arc::new(crate::ports::NoStandingRoles))]
    standing_role_port: Arc<dyn crate::ports::StandingRoles>,
    /// The last reading of each declared role, by its ensure's namespace and name; `None` when
    /// it did not read.
    #[builder(skip)]
    standing_readings: HashMap<(String, String), Option<crate::ports::StandingRoleReading>>,
    repositories: HashMap<RepositoryKey, ResourceObject<Repository>>,
    repository_sources: BTreeMap<RepositorySourceKey, ReadResourceObject<Repository>>,
    /// The checkouts this host's refresh discovered, as Checkouts in `discovered_namespace`.
    #[builder(skip)]
    discovered_checkouts: DiscoveredCheckouts,
    #[builder(skip)]
    discovered_namespace: String,
    #[builder(skip)]
    readiness_vessels: BTreeMap<RepositorySourceKey, ReadResourceObject<Vessel>>,
    /// Declared Host authority by resource name, which maps a vessel's
    /// placement target to the daemon that acts on it (#4021).
    #[builder(skip)]
    hosts: BTreeMap<String, ResourceObject<flotilla_resources::Host>>,
    #[builder(skip)]
    fulfilment_kinds: BTreeMap<RepositorySourceKey, ReadResourceObject<FulfilmentKind>>,
    /// The engine's adopted checkouts at every home it follows: each name's latest launch that
    /// has not ended (#3108).
    #[builder(skip)]
    engine_checkouts: Vec<flotilla_resources::checkout_records::CheckoutRecord>,
    /// This home, as the engine's checkouts name it.
    #[builder(skip)]
    engine_checkout_home: Option<String>,
    bootstrapping: bool,
    /// The `[primitives]` Convoy path (#3587 layer 8), and the namespace its
    /// changed convoys are named in.
    convoy_engine: flotilla_resources::convoy::engine::EngineConvoyPath,
    engine_namespace: String,
    #[builder(skip)]
    projection_delivery: ConvoyProjectionDelivery,
    #[builder(skip)]
    projection_full: bool,
    #[builder(skip)]
    convoy_inputs: ConvoyInputs,
    #[builder(skip)]
    projection_keys: HashSet<ConvoyKey>,
    #[builder(skip)]
    known_origins: HashSet<flotilla_protocol::NodeId>,
    emitted_queries: HashSet<QueryId>,
    #[builder(skip)]
    attach_resolver: Option<Arc<dyn AttachCapabilityResolver>>,
    /// The engine's provisioning readings behind `[primitives]` (#3629 layer 4).
    #[builder(skip = Arc::new(crate::ports::NoLifecycleReads))]
    lifecycle_reads: Arc<dyn crate::ports::LifecycleReads>,
    #[builder(skip)]
    attach_refresh_tasks: tokio::task::JoinSet<AttachCapabilityResolution>,
    #[builder(skip)]
    attach_refresh_generations: HashMap<SessionKey, (uuid::Uuid, tokio::task::AbortHandle)>,
    #[builder(skip)]
    attach_refresh_inputs: HashMap<SessionKey, AttachCapabilityInput>,
    #[builder(skip)]
    change_request_resolver: Option<Arc<dyn ConvoyChangeRequestResolver>>,
    #[builder(skip)]
    convoy_change_requests: HashMap<ResourceRef, ConvoyChangeRequest>,
    #[builder(skip)]
    change_request_refresh_generations: HashMap<ResourceRef, uuid::Uuid>,
    #[builder(skip)]
    change_request_refresh_tasks: HashMap<ResourceRef, tokio::task::JoinHandle<()>>,
    #[builder(skip)]
    change_request_refresh_started: HashMap<ResourceRef, Instant>,
    #[builder(skip)]
    change_request_refresh_failures: HashMap<ResourceRef, u32>,
    /// Convoys whose running lookup was overtaken by a repository trigger; each
    /// looks again (at its next interval) once that lookup's result is applied.
    change_request_retriggers: HashSet<ResourceRef>,
    #[builder(skip)]
    change_request_refresh_queue: ChangeRequestRefreshQueue,
    #[builder(skip)]
    issue_materializer: Option<IssueMaterializer>,
    /// The decided store's AgentSessions (#3108), joined to crew members by record name.
    #[builder(skip = Arc::new(crate::ports::NoAgentSessions))]
    agent_session_port: Arc<dyn crate::ports::AgentSessions>,
    #[builder(skip)]
    agent_sessions: HashMap<AgentSessionKey, flotilla_protocol::AgentSessionReading>,
    /// The home this daemon's own sessions carry, once following has begun.
    #[builder(skip)]
    agent_session_home: Option<flotilla_protocol::NodeId>,
    /// When this home last heard from each other home: their readings are current until
    /// `HOME_HEARD_WITHIN` after (#3108).
    #[builder(skip)]
    agent_session_heard: HashMap<flotilla_protocol::NodeId, chrono::DateTime<chrono::Utc>>,
    /// The daemon's wall clock: readings' ages, a quiet home's staleness and escalation deadlines
    /// are judged against it, so a test drives them by setting it.
    #[builder(skip = Arc::new(flotilla_resources::SystemClock))]
    clock: Arc<dyn flotilla_resources::Clock>,
    event_sink: Arc<dyn EventSink>,
    event_rx: broadcast::Receiver<DaemonEvent>,
}

#[derive(Clone, PartialEq, Eq)]
struct AttachCapabilityInput {
    host: HostName,
    session_id: Option<String>,
    endpoint: Option<flotilla_protocol::result_set::CleatEndpoint>,
    environment: String,
    pool: String,
}

struct AttachCapabilityResolution {
    generation: uuid::Uuid,
    key: SessionKey,
    attachable: bool,
}

struct ChangeRequestResolution {
    reference: ResourceRef,
    generation: uuid::Uuid,
    branch: String,
    result: ConvoyBranchRefresh,
}

struct ChangeRequestRefreshGroup {
    binding: Option<flotilla_resources::BoundChangeRequest>,
    targets: Vec<(ResourceRef, uuid::Uuid)>,
}

struct ChangeRequestRefreshQueue {
    tx: mpsc::UnboundedSender<ChangeRequestResolution>,
    rx: mpsc::UnboundedReceiver<ChangeRequestResolution>,
}

impl Default for ChangeRequestRefreshQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx }
    }
}

impl Aggregator {
    #[cfg(test)]
    pub fn new(state: AggregatorProjectionState, local_host: HostName, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        let event_rx = event_tx.subscribe();
        Self::with_events(state, local_host, Arc::new(BroadcastEventSink::new(event_tx)), event_rx)
    }

    pub fn with_events(
        state: AggregatorProjectionState,
        local_host: HostName,
        event_sink: Arc<dyn EventSink>,
        event_rx: broadcast::Receiver<DaemonEvent>,
    ) -> Self {
        Self {
            #[cfg(test)]
            copied_convoys: Default::default(),
            #[cfg(test)]
            summarized_convoys: Default::default(),
            state,
            local_host,
            convoys: HashMap::new(),
            attentions: Vec::new(),
            attention_namespace: String::new(),
            attention_port: None,
            convoy_keys_by_name: HashMap::new(),
            sessions_by_source: HashMap::new(),
            terminal_sessions: HashMap::new(),
            attachable_sessions: HashSet::new(),
            origin_hosts: HashMap::new(),
            projects: HashMap::new(),
            fleet_projects: BTreeMap::new(),
            convoy_ensures: BTreeMap::new(),
            standing_role_port: Arc::new(crate::ports::NoStandingRoles),
            standing_readings: HashMap::new(),
            repositories: HashMap::new(),
            repository_sources: BTreeMap::new(),
            discovered_checkouts: DiscoveredCheckouts::default(),
            discovered_namespace: String::new(),
            readiness_vessels: BTreeMap::new(),
            fulfilment_kinds: BTreeMap::new(),
            hosts: BTreeMap::new(),
            engine_checkouts: Vec::new(),
            engine_checkout_home: None,
            bootstrapping: false,
            convoy_engine: Default::default(),
            engine_namespace: String::new(),
            projection_delivery: ConvoyProjectionDelivery::Immediate,
            projection_full: false,
            convoy_inputs: ConvoyInputs::default(),
            projection_keys: HashSet::new(),
            known_origins: HashSet::new(),
            emitted_queries: HashSet::new(),
            attach_resolver: None,
            lifecycle_reads: Arc::new(crate::ports::NoLifecycleReads),
            attach_refresh_tasks: tokio::task::JoinSet::new(),
            attach_refresh_generations: HashMap::new(),
            attach_refresh_inputs: HashMap::new(),
            change_request_resolver: None,
            convoy_change_requests: HashMap::new(),
            change_request_refresh_generations: HashMap::new(),
            change_request_refresh_tasks: HashMap::new(),
            change_request_refresh_started: HashMap::new(),
            change_request_refresh_failures: HashMap::new(),
            change_request_retriggers: HashSet::new(),
            change_request_refresh_queue: ChangeRequestRefreshQueue::default(),
            issue_materializer: None,
            agent_session_port: Arc::new(crate::ports::NoAgentSessions),
            agent_sessions: HashMap::new(),
            agent_session_home: None,
            agent_session_heard: HashMap::new(),
            clock: Arc::new(flotilla_resources::SystemClock),
            event_sink,
            event_rx,
        }
    }

    /// Judges times against `clock`, the daemon's, rather than the system's.
    pub fn with_clock(mut self, clock: Arc<dyn flotilla_resources::Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Crew members show their AgentSessions, read from the decided store through `port`.
    pub fn with_agent_sessions(mut self, port: Arc<dyn crate::ports::AgentSessions>) -> Self {
        self.agent_session_port = port;
        self
    }

    /// Standing role rows show each role's state as `port` derives it.
    pub fn with_standing_roles(mut self, port: Arc<dyn crate::ports::StandingRoles>) -> Self {
        self.standing_role_port = port;
        self
    }

    /// Checkout rows show the checkouts this host's refresh discovered, as Checkouts in `namespace`.
    pub fn with_discovered_checkouts(mut self, discovered: DiscoveredCheckouts, namespace: &str) -> Self {
        self.discovered_checkouts = discovered;
        self.discovered_namespace = namespace.to_owned();
        self
    }

    /// Read engine-held provisioning resources through `reads` (#3629 layer 4).
    pub fn with_lifecycle_reads(mut self, reads: Arc<dyn crate::ports::LifecycleReads>) -> Self {
        self.lifecycle_reads = reads;
        self
    }

    /// Rows read a convoy's promises from the engine when it holds the convoy.
    pub fn with_convoy_engine(mut self, engine: flotilla_resources::convoy::engine::EngineConvoyPath, namespace: &str) -> Self {
        self.convoy_engine = engine;
        self.engine_namespace = namespace.to_owned();
        self
    }

    /// Rows and salience read the open attentions of `namespace` through `port`.
    pub fn with_attentions(mut self, port: Arc<dyn Attentions>, namespace: &str) -> Self {
        self.attention_port = Some(port);
        self.attention_namespace = namespace.to_owned();
        self
    }

    pub fn with_attach_resolver_arc(mut self, resolver: Arc<dyn AttachCapabilityResolver>) -> Self {
        self.attach_resolver = Some(resolver);
        self
    }
    pub fn with_change_request_resolver_arc(mut self, resolver: Arc<dyn ConvoyChangeRequestResolver>) -> Self {
        self.change_request_resolver = Some(resolver);
        self
    }
    pub fn with_issue_resolver_arc(mut self, resolver: Arc<dyn IssueMaterializationResolver>) -> Self {
        self.issue_materializer = Some(IssueMaterializer::new(self.state.clone(), resolver, self.event_sink.clone()));
        self
    }
    #[cfg(test)]
    pub fn with_attach_resolver<R>(mut self, resolver: Arc<R>) -> Self
    where
        R: AttachCapabilityResolver + 'static,
    {
        self.attach_resolver = Some(resolver);
        self
    }

    #[cfg(test)]
    pub fn with_change_request_resolver<R>(mut self, resolver: Arc<R>) -> Self
    where
        R: ConvoyChangeRequestResolver + 'static,
    {
        self.change_request_resolver = Some(resolver);
        self
    }

    pub fn with_issue_polling_health(mut self, health: IssuePollingHealth) -> Self {
        let materializer = self.issue_materializer.take().expect("issue resolver must be configured before polling health");
        self.issue_materializer = Some(materializer.with_polling_health(health));
        self
    }

    pub async fn run(self, resolvers: AggregatorResolvers) -> Result<(), ResourceError> {
        let AggregatorResolvers {
            durable_convoys,
            durable_convoy_ensures,
            durable_fulfilment_kinds,
            durable_sessions,
            durable_projects,
            durable_fleet_designation,
            durable_repositories,
            durable_vessels,
            durable_hosts,
        } = resolvers;
        let sources = AggregatorSourceRefs::builder()
            .durable_convoys(&durable_convoys)
            .durable_convoy_ensures(&durable_convoy_ensures)
            .maybe_durable_fulfilment_kinds(
                durable_fulfilment_kinds.as_ref().map(|source| source as &dyn AggregatorReplicaWatchSource<FulfilmentKind>),
            )
            .durable_sessions(&durable_sessions)
            .durable_projects(&durable_projects)
            .maybe_durable_fleet_designation(
                durable_fleet_designation
                    .as_ref()
                    .map(|resolver| resolver as &dyn AggregatorReplicaWatchSource<flotilla_resources::FleetDesignation>),
            )
            .durable_repositories(&durable_repositories)
            .durable_vessels(&durable_vessels)
            .durable_hosts(&durable_hosts)
            .build();
        self.run_with_sources(sources).await
    }

    async fn run_with_sources(mut self, sources: AggregatorSourceRefs<'_>) -> Result<(), ResourceError> {
        let AggregatorSourceRefs {
            durable_convoys,
            durable_convoy_ensures,
            durable_fulfilment_kinds,
            durable_sessions,
            durable_projects,
            durable_fleet_designation,
            durable_repositories,
            durable_vessels,
            durable_hosts,
        } = sources;
        // Subscribe before any source bootstrap awaits so demand arriving
        // during recovery remains observable. Also consume the initial value
        // for demand that predates Aggregator startup; #747's source-backed
        // reconciler plugs into this same initial/change path.
        let mut demand_rx = self.state.subscribe_demand();
        let mut fetch_more_rx = self.state.subscribe_fetch_more();
        let mut daemon_event_rx = self.event_rx.resubscribe();
        let initial_demand = demand_rx.borrow_and_update().clone();
        if let Some(materializer) = &mut self.issue_materializer {
            materializer.reconcile(initial_demand);
        }
        self.bootstrapping = true;
        {
            let mut view = self.state.write().await;
            if !view.local_rows.is_empty() {
                view.local_rows.clear();
                view.seq = view.seq.saturating_add(1);
            }
        }
        self.state.replace_local_independent_rows(Vec::new()).await;
        let mut fulfilment_kind_stream = match durable_fulfilment_kinds {
            Some(source) => {
                let (items, watch) = Self::recover_replica_watch(source).await?;
                self.fulfilment_kinds = readiness_resources(items);
                watch
            }
            None => Box::pin(futures::stream::pending()) as BoxStream<'static, Result<ReadWatchEvent<FulfilmentKind>, ResourceError>>,
        };
        let mut durable_convoy_stream = self.recover_replica_convoy_watch(durable_convoys).await?;
        // The open attentions, read once here and again at each change the port signals.
        self.read_open_attentions().await;
        let mut durable_session_stream = self.recover_replica_session_watch(durable_sessions).await?;
        let mut durable_fleet_stream = match durable_fleet_designation {
            Some(source) => self.recover_fleet_designation_watch(source).await?,
            None => Box::pin(futures::stream::pending())
                as BoxStream<'static, Result<ReadWatchEvent<flotilla_resources::FleetDesignation>, ResourceError>>,
        };
        let mut durable_project_stream = self.recover_project_watch(durable_projects).await?;
        let mut durable_ensure_stream = self.recover_convoy_ensure_watch(durable_convoy_ensures).await?;
        let mut durable_repository_stream = self.recover_repository_watch(durable_repositories).await?;
        // The engine's checkout commits, at every home, wake the checkout rows and readiness
        // (#3108); subscribed before the first read, so a commit between the two is not missed.
        let mut checkout_changes = self.lifecycle_reads.checkouts().ok().map(|checkouts| checkouts.changes());
        self.read_engine_checkouts();
        self.refresh_origin_hosts().await;
        // Subscribed before the first read, so a refresh between the two is not missed.
        let mut discovered_changes = self.discovered_checkouts.changes();
        self.rebuild_checkout_rows().await?;
        let (items, mut vessel_stream) = Self::recover_replica_watch(durable_vessels).await?;
        self.readiness_vessels = readiness_resources(items);
        let (items, mut host_stream) = Self::recover_replica_watch(durable_hosts).await?;
        self.replace_hosts(items);
        // AgentSessions are the engine's, at every home: followed from the decided store (#3108).
        let mut agent_session_updates = match self.agent_session_port.follow().await {
            Ok(follow) => {
                self.agent_session_home = Some(follow.home);
                self.agent_sessions = follow.held.into_iter().collect();
                self.agent_session_heard = follow.heard.into_iter().collect();
                Some(follow.updates)
            }
            Err(error) => {
                debug!(%error, "crew members show no AgentSessions");
                None
            }
        };
        self.refresh_origin_hosts().await;
        self.rebuild_local_projection().await;
        self.bootstrapping = false;
        self.projection_delivery = ConvoyProjectionDelivery::Coalesced { deadline: None };
        self.emitted_queries.extend(QueryId::ALWAYS_MATERIALIZED.iter().cloned());
        // Deltas were withheld while bootstrapping: a subscriber that joined
        // meanwhile catches up from a full set of every family.
        for query in QueryId::ALWAYS_MATERIALIZED {
            if let Some(set) = self.state.result_set_for(query).await {
                self.event_sink.emit(DaemonEvent::ResultSet(Box::new(set)));
            }
        }
        // Publish/bootstrap scoped demand now, rather than charging the first
        // unrelated resource change for every existing project scope.
        self.emit_scoped_convoy_result_sets().await;
        self.emit_awareness_result_sets().await;

        // An engine edge writes no old record: follow the engine's changes to
        // re-project the convoy's row (#3587 layer 8).
        let mut engine_changes = self.convoy_engine.get().map(|engine| engine.changes());
        // Environments are the engine's: their changes come from its log (#3108).
        let mut environment_changes = self.lifecycle_reads.environments().ok().map(|environments| environments.changes());
        // So are clones: a checkout's readiness reads its clone launch.
        let mut clone_changes = self.lifecycle_reads.clones().ok().map(|clones| clones.changes());
        let mut attention_changes = self.attention_port.as_ref().and_then(|port| port.changes());
        // What the forge observers find, their branch probes' finds among it, is on the change
        // request records: a convoy with no change request looks again when they change (#3108).
        let mut change_request_changes = match &self.change_request_resolver {
            Some(resolver) => resolver.change_request_changes().await,
            None => None,
        };

        loop {
            let escalation_delay = self.next_escalation_delay(self.clock.now());
            let home_stale_delay = self.next_home_stale_delay(self.clock.now());
            let escalation = async move {
                match escalation_delay {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => futures::future::pending().await,
                }
            };
            tokio::pin!(escalation);
            // One wake, at the next moment a quiet home's readings stop being current.
            let home_stale = async move {
                match home_stale_delay {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => futures::future::pending().await,
                }
            };
            tokio::pin!(home_stale);
            let projection_deadline = match self.projection_delivery {
                ConvoyProjectionDelivery::Immediate => None,
                ConvoyProjectionDelivery::Coalesced { deadline } => deadline,
            };
            let projection_flush = async move {
                match projection_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => futures::future::pending().await,
                }
            };
            tokio::pin!(projection_flush);
            let attention_change = async {
                match attention_changes.as_mut() {
                    Some(changes) => changes.recv().await,
                    None => futures::future::pending().await,
                }
            };
            let engine_change = async {
                match engine_changes.as_mut() {
                    Some(changes) => changes.recv().await,
                    None => futures::future::pending().await,
                }
            };
            let environment_change = async {
                match environment_changes.as_mut() {
                    Some(changes) => changes.recv().await,
                    None => futures::future::pending().await,
                }
            };
            let clone_change = async {
                match clone_changes.as_mut() {
                    Some(changes) => changes.recv().await,
                    None => futures::future::pending().await,
                }
            };
            let change_request_change = async {
                match change_request_changes.as_mut() {
                    Some(changes) => changes.next().await,
                    None => futures::future::pending().await,
                }
            };
            let agent_session_update = async {
                match agent_session_updates.as_mut() {
                    Some(updates) => updates.next().await,
                    None => futures::future::pending().await,
                }
            };
            tokio::select! {
                update = agent_session_update => match update {
                    Some(update) => self.agent_sessions_changed(update).await,
                    None => {
                        tracing::warn!("the AgentSession follow ended; crew members keep the last readings");
                        agent_session_updates = None;
                    }
                },
                change = attention_change => match change {
                    Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => self.refresh_attentions().await,
                    Err(broadcast::error::RecvError::Closed) => attention_changes = None,
                },
                change = engine_change => match change {
                    Ok(name) => self.engine_convoy_changed(&name).await,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "aggregator lagged behind the engine's convoy changes");
                        self.rebuild_local_projection().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => engine_changes = None,
                },
                () = &mut projection_flush => {
                    self.projection_delivery = ConvoyProjectionDelivery::Coalesced { deadline: None };
                    self.flush_local_projection().await;
                    tokio::task::yield_now().await;
                }
                change = change_request_change => match change {
                    Some(()) => self.recheck_unproduced_change_requests(),
                    None => {
                        tracing::warn!("the change request records' follow ended; convoys without a change request are not looked up again");
                        change_request_changes = None;
                    }
                },
                () = &mut escalation => {
                    if self.rebuild_salience_projection().await {
                        self.emit_awareness_result_sets().await;
                    }
                }
                () = &mut home_stale => {
                    self.rebuild_local_projection().await;
                }
                demand = demand_rx.changed() => {
                    if demand.is_err() {
                        return Err(ResourceError::other("aggregator demand registry closed"));
                    }
                    let demanded = demand_rx.borrow_and_update().clone();
                    if let Some(materializer) = &mut self.issue_materializer {
                        materializer.reconcile(demanded);
                    }
                }
                intent = fetch_more_rx.recv() => match intent {
                    Ok((query, generation)) => {
                        if let Some(materializer) = &self.issue_materializer {
                            materializer.fetch_more(&query, generation);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "aggregator lagged behind fetch-more intents");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(ResourceError::other("aggregator fetch-more intent channel closed"));
                    }
                },
                event = daemon_event_rx.recv() => match event {
                    Ok(DaemonEvent::RepoRefreshCompleted { .. }) => {}
                    Ok(DaemonEvent::RepoDelta(delta)) => {
                        if self.repo_delta_changed_change_requests(&delta) {
                            self.refresh_repository_change_requests(&delta.repo_identity).await;
                        }
                    }
                    Ok(event @ (DaemonEvent::PeerStatusChanged { .. } | DaemonEvent::HostSnapshot(_) | DaemonEvent::HostRemoved { .. })) => {
                        // Snapshot/status churn does not itself change a route.
                        // Explicit topology transitions invalidate route preflight.
                        if !matches!(event, DaemonEvent::HostSnapshot(_)) {
                            self.attach_refresh_inputs.clear();
                        }
                        self.rebuild_independents_projection().await;
                        self.rebuild_local_projection().await;
                        self.rebuild_checkout_rows().await?;
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "aggregator lagged behind daemon refresh events");
                        self.refresh_all_change_requests().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(ResourceError::other("daemon event channel closed"));
                    }
                },
                resolution = self.attach_refresh_tasks.join_next(), if !self.attach_refresh_tasks.is_empty() => {
                    match resolution {
                        Some(Ok(resolution)) => self.apply_attach_capability_resolution(resolution).await?,
                        Some(Err(error)) if !error.is_cancelled() => {
                            tracing::warn!(%error, "terminal capability refresh failed");
                        }
                        _ => {}
                    }
                },
                resolution = self.change_request_refresh_queue.rx.recv() => {
                    if let Some(resolution) = resolution {
                        self.apply_change_request_resolution(resolution).await;
                    }
                },
                change = clone_change => match change {
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => self.rebuild_local_projection().await,
                    Err(broadcast::error::RecvError::Closed) => clone_changes = None,
                },
                event = fulfilment_kind_stream.next() => {
                    match event {
                        Some(Ok(event)) => apply_readiness_event(&mut self.fulfilment_kinds, event),
                        Some(Err(ResourceError::WatchExpired { .. })) => {
                            if let Some(source) = durable_fulfilment_kinds {
                                let (items, watch) = Self::recover_replica_watch(source).await?;
                                self.fulfilment_kinds = readiness_resources(items);
                                fulfilment_kind_stream = watch;
                            }
                        }
                        Some(Err(error)) => return Err(error),
                        None => return Err(ResourceError::other("aggregator fulfilment kind watch ended")),
                    }
                    self.rebuild_local_projection().await;
                }
                event = vessel_stream.next() => {
                    match event {
                        Some(Ok(event)) => apply_readiness_event(&mut self.readiness_vessels, event),
                        Some(Err(ResourceError::WatchExpired { .. })) => {
                            let (items, watch) = Self::recover_replica_watch(durable_vessels).await?;
                            self.readiness_vessels = readiness_resources(items);
                            vessel_stream = watch;
                        }
                        Some(Err(error)) => return Err(error),
                        None => return Err(ResourceError::other("aggregator vessel readiness watch ended")),
                    }
                    self.refresh_origin_hosts().await;
                    self.rebuild_local_projection().await;
                }
                event = host_stream.next() => {
                    let changed = match event {
                        Some(Ok(event)) => self.apply_host_event(event),
                        Some(Err(ResourceError::WatchExpired { .. })) => {
                            let (items, watch) = Self::recover_replica_watch(durable_hosts).await?;
                            host_stream = watch;
                            self.replace_hosts(items)
                        }
                        Some(Err(error)) => return Err(error),
                        None => return Err(ResourceError::other("aggregator host watch ended")),
                    };
                    if changed {
                        self.rebuild_local_projection().await;
                        if self.rebuild_salience_projection().await {
                            self.emit_awareness_result_sets().await;
                        }
                    }
                }
                change = next_checkout_change(&mut checkout_changes) => {
                    if change {
                        self.read_engine_checkouts();
                        self.refresh_origin_hosts().await;
                        self.rebuild_checkout_rows().await?;
                        self.rebuild_local_projection().await;
                    }
                }
                event = durable_convoy_stream.next() => match event {
                    Some(Ok(event)) => self.apply_replica_convoy_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_convoy_stream = self.recover_replica_convoy_watch(durable_convoys).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable convoy watch ended")),
                },
                change = environment_change => match change {
                    Ok(change) => self.environment_changed(Some(change)).await,
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, "aggregator lagged behind the environments' changes");
                        self.environment_changed(None).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => environment_changes = None,
                },

                event = durable_session_stream.next() => match event {
                    Some(Ok(event)) => self.apply_replica_session_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_session_stream = self.recover_replica_session_watch(durable_sessions).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable terminal session watch ended")),
                },
                event = durable_fleet_stream.next() => match event {
                    Some(Ok(event)) => {
                        if matches!(event, ReadWatchEvent::DeletedByName { .. }) {
                            if let Some(source) = durable_fleet_designation {
                                durable_fleet_stream = self.recover_fleet_designation_watch(source).await?;
                            }
                            continue;
                        }
                        let (deleted, read) = match event {
                            ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) => (false, read),
                            ReadWatchEvent::Deleted(read) => (true, read),
                            ReadWatchEvent::DeletedByName { .. } => unreachable!("handled above"),
                        };
                        if deleted {
                            self.fleet_projects.remove(&read.object.metadata.namespace);
                        } else {
                            self.fleet_projects.insert(read.object.metadata.namespace, read.object.spec.project);
                        }
                        self.rebuild_store_catalog().await;
                    }
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        if let Some(source) = durable_fleet_designation {
                            durable_fleet_stream = self.recover_fleet_designation_watch(source).await?;
                        }
                    }
                    Some(Err(error)) => return Err(error),
                    None => return Err(ResourceError::other("aggregator fleet designation watch ended")),
                },
                event = durable_project_stream.next() => match event {
                    Some(Ok(event)) => self.apply_project_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_project_stream = self.recover_project_watch(durable_projects).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable project watch ended")),
                },
                event = durable_ensure_stream.next() => match event {
                    Some(Ok(event)) => self.apply_convoy_ensure_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_ensure_stream = self.recover_convoy_ensure_watch(durable_convoy_ensures).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable convoy ensure watch ended")),
                },
                event = durable_repository_stream.next() => match event {
                    Some(Ok(event)) => self.apply_repository_read_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_repository_stream = self.recover_repository_watch(durable_repositories).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable repository watch ended")),
                },
                change = discovered_changes.recv() => match change {
                    // A lagged receiver missed some changes; the rows read them all anyway.
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => self.discovered_checkouts_changed().await?,
                    Err(broadcast::error::RecvError::Closed) => return Err(ResourceError::other("aggregator discovered checkouts ended")),
                },

            }
        }
    }

    async fn recover_replica_convoy_watch(
        &mut self,
        resolver: &dyn AggregatorReplicaWatchSource<Convoy>,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<Convoy>, ResourceError>>, ResourceError> {
        let (items, watch) = Self::recover_replica_watch(resolver).await?;
        self.replace_replica_convoys(items).await;
        Ok(watch)
    }

    async fn recover_replica_session_watch(
        &mut self,
        resolver: &dyn AggregatorReplicaWatchSource<TerminalSession>,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<TerminalSession>, ResourceError>>, ResourceError> {
        let (items, watch) = Self::recover_replica_watch(resolver).await?;
        self.replace_replica_sessions(items).await;
        Ok(watch)
    }

    async fn recover_replica_watch<T: Resource>(
        resolver: &dyn AggregatorReplicaWatchSource<T>,
    ) -> Result<(Vec<ReadResourceObject<T>>, BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>), ResourceError> {
        let (listed, watch) = recover_view(resolver).await?;
        Ok((listed.items, watch))
    }

    async fn recover_fleet_designation_watch(
        &mut self,
        resolver: &dyn AggregatorReplicaWatchSource<flotilla_resources::FleetDesignation>,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<flotilla_resources::FleetDesignation>, ResourceError>>, ResourceError> {
        let (items, watch) = Self::recover_replica_watch(resolver).await?;
        self.fleet_projects = items.into_iter().map(|read| (read.object.metadata.namespace, read.object.spec.project)).collect();
        self.rebuild_store_catalog().await;
        Ok(watch)
    }

    async fn recover_project_watch(
        &mut self,
        resolver: &dyn AggregatorReplicaWatchSource<Project>,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<Project>, ResourceError>>, ResourceError> {
        let (items, watch) = Self::recover_replica_watch(resolver).await?;
        self.projects = items
            .into_iter()
            .map(|project| {
                let object = project.object;
                ((object.metadata.namespace.clone(), object.metadata.name.clone()), object)
            })
            .collect();
        self.rebuild_store_catalog().await;
        Ok(watch)
    }

    async fn recover_convoy_ensure_watch(
        &mut self,
        resolver: &dyn AggregatorReplicaWatchSource<ConvoyEnsure>,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<ConvoyEnsure>, ResourceError>>, ResourceError> {
        let (items, watch) = Self::recover_replica_watch(resolver).await?;
        self.convoy_ensures = items
            .into_iter()
            .map(|ensure| {
                (repository_source_key(&ensure.object.metadata.namespace, &ensure.object.metadata.name, &ensure.provenance), ensure.object)
            })
            .collect();
        self.rebuild_standing_roles().await;
        Ok(watch)
    }

    async fn recover_repository_watch(
        &mut self,
        resolver: &dyn AggregatorReplicaWatchSource<Repository>,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<Repository>, ResourceError>>, ResourceError> {
        let (items, watch) = Self::recover_replica_watch(resolver).await?;
        self.repository_sources = items
            .into_iter()
            .map(|source| {
                (repository_source_key(&source.object.metadata.namespace, &source.object.metadata.name, &source.provenance), source)
            })
            .collect();
        self.rebuild_repository_catalog().await;
        Ok(watch)
    }

    /// Rereads the engine's adopted checkouts at every home it follows. A failed read keeps the
    /// checkouts held, and the next change reads again.
    fn read_engine_checkouts(&mut self) {
        let Ok(checkouts) = self.lifecycle_reads.checkouts() else { return };
        let records = match checkouts.every() {
            Ok(records) => records,
            Err(error) => {
                tracing::warn!(%error, "aggregator could not read the engine's checkouts");
                return;
            }
        };
        self.engine_checkout_home = Some(checkouts.home().to_owned());
        let mut by_home = BTreeMap::<String, Vec<_>>::new();
        for record in records {
            by_home.entry(record.home.clone()).or_default().push(record);
        }
        self.engine_checkouts = by_home
            .into_values()
            .flat_map(|records| flotilla_resources::checkout_records::latest_launches(records).into_values())
            .filter(|record| matches!(record.spec, CheckoutSpec::Observed(_)) && !record.ended())
            .collect();
    }

    /// The host `home` names: this host for this home, else the host its origin resolves to.
    fn engine_home_host(&self, home: &str) -> Option<HostName> {
        if self.engine_checkout_home.as_deref() == Some(home) {
            Some(self.local_host.clone())
        } else {
            self.origin_hosts.get(&flotilla_protocol::NodeId::new(home)).cloned()
        }
    }

    /// Reads the open attentions from the attention port, the engine's, and recomputes salience
    /// when they changed.
    async fn read_open_attentions(&mut self) {
        if self.read_attentions().await && self.rebuild_salience_projection().await && !self.bootstrapping {
            self.emit_awareness_result_sets().await;
        }
    }

    /// Reads the open attentions; whether they changed.
    async fn read_attentions(&mut self) -> bool {
        let Some(port) = &self.attention_port else { return false };
        let mut open = match port.open(&self.attention_namespace, None, None).await {
            Ok(open) => open,
            Err(error) => {
                tracing::warn!(%error, "the open attentions did not read; rows keep the last read");
                return false;
            }
        };
        open.sort_by(|left, right| (left.sender, &left.subject, &left.reason).cmp(&(right.sender, &right.subject, &right.reason)));
        if open == self.attentions {
            return false;
        }
        self.attentions = open;
        true
    }

    async fn refresh_attentions(&mut self) {
        if self.read_attentions().await {
            self.rebuild_local_projection().await;
            // A role's hold is read from its attention.
            self.rebuild_standing_roles().await;
        }
    }

    async fn replace_replica_convoys(&mut self, convoys: Vec<ReadResourceObject<Convoy>>) {
        let previous = self.effective_convoys();
        self.convoys = convoys.into_iter().map(|convoy| (convoy_key(&convoy), Arc::new(convoy))).collect();
        self.convoy_keys_by_name.clear();
        for key in self.convoys.keys() {
            self.convoy_keys_by_name.entry((key.0.clone(), key.1.clone())).or_default().insert(key.clone());
        }
        self.refresh_origin_hosts().await;
        let current = self.effective_convoys();
        let references = previous.keys().chain(current.keys()).cloned().collect::<HashSet<_>>();
        self.convoy_change_requests.retain(|reference, _| current.contains_key(reference));
        for reference in references {
            self.handle_convoy_transition(&reference, previous.get(&reference), current.get(&reference));
        }
        self.change_request_refresh_generations.retain(|reference, _| current.contains_key(reference));
        self.change_request_refresh_started.retain(|reference, _| current.contains_key(reference));
        self.change_request_refresh_failures.retain(|reference, _| current.contains_key(reference));
        self.rebuild_local_projection().await;
        if let Err(error) = self.rebuild_checkout_rows().await {
            debug!(%error, "could not refresh checkout orphan attention after convoy relist");
        }
        self.rebuild_standing_roles().await;
    }

    /// The role convoy `convoy` is a generation of, as its ensure's namespace and name.
    fn generation_of(convoy: &ResourceObject<Convoy>) -> Option<(String, String)> {
        convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION).map(|ensure| (convoy.metadata.namespace.clone(), ensure.clone()))
    }

    async fn apply_replica_convoy_event(&mut self, event: ReadWatchEvent<Convoy>) {
        let key = match &event {
            ReadWatchEvent::Added(convoy) | ReadWatchEvent::Modified(convoy) | ReadWatchEvent::Deleted(convoy) => convoy_key(convoy),
            ReadWatchEvent::DeletedByName { tombstone, provenance } => tombstone_key(tombstone, provenance),
        };
        // Resolve a newly seen origin once. Unknown hosts remain buffered until
        // host discovery changes; ordinary replica events do no inventory walk.
        let new_origin = key.2.as_ref().is_some_and(|origin| !self.known_origins.contains(origin));
        let previous_all = new_origin.then(|| self.effective_convoys());
        let previous = self.effective_convoy_key(&key).filter(|item| item.object.metadata.deletion_timestamp.is_none()).cloned();
        let previous_ref = self.convoy_key_ref(&key);
        let role = match &event {
            ReadWatchEvent::Added(convoy) | ReadWatchEvent::Modified(convoy) | ReadWatchEvent::Deleted(convoy) => {
                Self::generation_of(&convoy.object)
            }
            ReadWatchEvent::DeletedByName { .. } => None,
        }
        .or_else(|| previous.as_ref().and_then(|item| Self::generation_of(&item.object)));
        match event {
            ReadWatchEvent::Added(convoy) | ReadWatchEvent::Modified(convoy) => {
                self.convoys.insert(key.clone(), Arc::new(convoy));
            }
            ReadWatchEvent::Deleted(_) | ReadWatchEvent::DeletedByName { .. } => {
                self.convoys.remove(&key);
            }
        }
        let identity = (key.0.clone(), key.1.clone());
        if self.effective_convoy_key(&key).is_some() {
            self.convoy_keys_by_name.entry(identity).or_default().insert(key.clone());
        } else if let Some(keys) = self.convoy_keys_by_name.get_mut(&identity) {
            keys.remove(&key);
            if keys.is_empty() {
                self.convoy_keys_by_name.remove(&identity);
            }
        }
        if new_origin {
            let old_hosts = self.origin_hosts.clone();
            self.refresh_origin_hosts().await;
            if self.origin_hosts != old_hosts {
                let previous = previous_all.expect("new origin saved the prior view");
                let current = self.effective_convoys();
                for reference in previous.keys().chain(current.keys()).collect::<HashSet<_>>() {
                    self.handle_convoy_transition(reference, previous.get(reference), current.get(reference));
                }
                self.rebuild_local_projection().await;
                if let Err(error) = self.rebuild_checkout_rows().await {
                    debug!(%error, "could not refresh checkout orphan attention after origin change");
                }
                if let Some((namespace, ensure)) = role {
                    self.refresh_standing_role(&namespace, &ensure).await;
                }
                return;
            }
        }
        let current = self.effective_convoy_key(&key).filter(|item| item.object.metadata.deletion_timestamp.is_none()).cloned();
        if let Some(reference) = self.convoy_key_ref(&key).or(previous_ref) {
            self.handle_convoy_transition(
                &reference,
                previous.as_ref().map(|item| &item.object),
                current.as_ref().map(|item| &item.object),
            );
            if current.is_none() {
                self.invalidate_change_request(&reference);
                self.change_request_refresh_started.remove(&reference);
                self.change_request_refresh_generations.remove(&reference);
            }
        }
        self.projection_keys.insert(key);
        self.schedule_local_projection().await;
        // Orphan status changes only when parent membership changes, not on
        // progress/status churn. Checkouts contain compact references only.
        if previous.is_some() != current.is_some() {
            if let Err(error) = self.rebuild_checkout_rows().await {
                debug!(%error, "could not refresh checkout orphan attention after convoy event");
            }
        }
        // A role's state is read from its generations.
        if let Some((namespace, ensure)) = role {
            self.refresh_standing_role(&namespace, &ensure).await;
        }
    }

    /// This home's convoy `name` changed on the engine: re-project its row.
    async fn engine_convoy_changed(&mut self, name: &str) {
        let keys: Vec<ConvoyKey> = self
            .convoy_keys_by_name
            .get(&(self.engine_namespace.clone(), name.to_owned()))
            .into_iter()
            .flat_map(|keys| keys.iter().filter(|key| key.2.is_none()).cloned())
            .collect();
        if keys.is_empty() {
            return;
        }
        let role = keys.iter().filter_map(|key| self.convoys.get(key)).find_map(|convoy| Self::generation_of(&convoy.object));
        self.projection_keys.extend(keys);
        self.schedule_local_projection().await;
        if let Some((namespace, ensure)) = role {
            self.refresh_standing_role(&namespace, &ensure).await;
        }
    }

    fn has_convoy(&self, namespace: &str, name: &str) -> bool {
        self.convoy_keys_by_name
            .get(&(namespace.to_string(), name.to_string()))
            .is_some_and(|keys| keys.iter().any(|key| self.convoy_key_ref(key).is_some()))
    }

    fn effective_convoy_key(&self, key: &ConvoyKey) -> Option<&Arc<ReadResourceObject<Convoy>>> {
        self.convoys.get(key)
    }

    fn convoy_key_ref(&self, key: &ConvoyKey) -> Option<ResourceRef> {
        let host = match &key.2 {
            Some(origin) => self.origin_hosts.get(origin)?.clone(),
            None => self.local_host.clone(),
        };
        Some(ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, &key.0, &key.1).on_host(host))
    }

    fn handle_convoy_transition(
        &mut self,
        reference: &ResourceRef,
        previous: Option<&ResourceObject<Convoy>>,
        current: Option<&ResourceObject<Convoy>>,
    ) {
        let association_changed = Self::convoy_association_changed(previous, current);
        let phase_changed = Self::convoy_phase_changed(previous, current);
        if association_changed {
            self.invalidate_change_request(reference);
        }
        if current.is_some_and(|convoy| {
            convoy.status.iter().flat_map(|status| &status.subjects).any(|entry| {
                entry.subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
                    && entry.relationship == flotilla_protocol::Relationship::Produces
            })
        }) {
            self.change_request_refresh_failures.remove(reference);
            if let Some(task) = self.change_request_refresh_tasks.remove(reference) {
                task.abort();
            }
        }
        // A lookup already in flight for an unchanged association stays valid
        // across a live phase change. Replacing it would defer the convoy's first
        // branch scan by a whole refresh interval, as admission's Pending→Active
        // step otherwise does (#3206).
        // A lookup is pending until its resolution is applied, which removes
        // its task; a finished task's resolution may still be queued.
        let lookup_in_flight = self.change_request_refresh_tasks.contains_key(reference);
        let ended = current.and_then(|convoy| convoy.status.as_ref()).is_some_and(|status| convoy_phase_is_terminal(status.phase));
        if association_changed || (phase_changed && (ended || !lookup_in_flight)) {
            if let Some(current) = current {
                self.schedule_change_request_refresh(current);
            }
        }
    }

    fn convoy_association_changed(previous: Option<&ResourceObject<Convoy>>, current: Option<&ResourceObject<Convoy>>) -> bool {
        previous.map(|convoy| (&convoy.spec.r#ref, &convoy.spec.repositories, &convoy.spec.change_request))
            != current.map(|convoy| (&convoy.spec.r#ref, &convoy.spec.repositories, &convoy.spec.change_request))
    }

    fn convoy_phase_changed(previous: Option<&ResourceObject<Convoy>>, current: Option<&ResourceObject<Convoy>>) -> bool {
        previous.and_then(|convoy| convoy.status.as_ref().map(|status| status.phase))
            != current.and_then(|convoy| convoy.status.as_ref().map(|status| status.phase))
    }

    fn invalidate_change_request(&mut self, reference: &ResourceRef) {
        self.change_request_refresh_generations.insert(reference.clone(), uuid::Uuid::new_v4());
        if let Some(task) = self.change_request_refresh_tasks.remove(reference) {
            task.abort();
        }
        self.convoy_change_requests.remove(reference);
        self.change_request_refresh_failures.remove(reference);
        self.change_request_retriggers.remove(reference);
    }

    fn is_home_convoy(&self, reference: &ResourceRef) -> bool {
        reference.host.as_ref() == Some(&self.local_host)
            && self.convoys.contains_key(&(reference.namespace.clone(), reference.name.clone(), None))
    }

    /// When convoy `reference`'s next lookup may run. A lookup by branch reads only the forge
    /// observers' records, so it runs at once. Only a lookup of a bound change request can still
    /// reach the forge, when its record is missing (#3108), so only that one keeps the minimum
    /// interval and the failure backoff.
    fn next_change_request_refresh(&self, reference: &ResourceRef, bound: bool, now: Instant) -> Instant {
        if !bound {
            return now;
        }
        let failures = self.change_request_refresh_failures.get(reference).copied().unwrap_or(0).min(3);
        let interval = std::cmp::min(CHANGE_REQUEST_MIN_REFRESH_INTERVAL * (1 << failures), CHANGE_REQUEST_MAX_RETRY_INTERVAL);
        match self.change_request_refresh_started.get(reference).copied() {
            Some(started) if started > now => started,
            Some(started) => std::cmp::max(now, started + interval),
            None => now,
        }
    }

    fn schedule_change_request_refresh(&mut self, convoy: &ResourceObject<Convoy>) {
        let reference = self.convoy_ref(&convoy.metadata.namespace, &convoy.metadata.name);
        if !self.is_home_convoy(&reference) {
            return;
        }
        if convoy.status.as_ref().is_some_and(|status| convoy_phase_is_terminal(status.phase)) {
            self.change_request_refresh_generations.insert(reference.clone(), uuid::Uuid::new_v4());
            if let Some(task) = self.change_request_refresh_tasks.remove(&reference) {
                task.abort();
            }
            return;
        }
        let generation = uuid::Uuid::new_v4();
        self.change_request_refresh_generations.insert(reference.clone(), generation);
        if let Some(task) = self.change_request_refresh_tasks.remove(&reference) {
            task.abort();
        }
        let Some(branch) = convoy.spec.r#ref.clone() else {
            self.convoy_change_requests.remove(&reference);
            return;
        };
        let repositories = convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>();
        let binding = convoy.spec.change_request.clone();
        if repositories.is_empty() && binding.is_none() {
            self.convoy_change_requests.remove(&reference);
            return;
        }
        let Some(resolver) = self.change_request_resolver.clone() else {
            return;
        };
        let refresh_at = self.next_change_request_refresh(&reference, binding.is_some(), Instant::now());
        self.change_request_refresh_started.insert(reference.clone(), refresh_at);
        let refresh_tx = self.change_request_refresh_queue.tx.clone();
        let task_reference = reference.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep_until(refresh_at).await;
            let result = bounded_refresh(&*resolver, &repositories, &branch, binding.as_ref()).await;
            let _ = refresh_tx.send(ChangeRequestResolution { reference: task_reference, generation, branch, result });
        });
        self.change_request_refresh_tasks.insert(reference, task);
    }

    async fn apply_change_request_resolution(&mut self, resolution: ChangeRequestResolution) {
        let ChangeRequestResolution { reference, generation, branch, result } = resolution;
        if self.change_request_refresh_generations.get(&reference) != Some(&generation) {
            return;
        }
        self.change_request_refresh_tasks.remove(&reference);
        let retrigger = self.change_request_retriggers.remove(&reference);
        let discovery_resolution = result.clone();
        let mut failed = false;
        match result.primary {
            Ok(Some(change_request)) => {
                self.convoy_change_requests.insert(reference.clone(), change_request);
                self.change_request_refresh_failures.remove(&reference);
            }
            Ok(None) => {
                self.convoy_change_requests.remove(&reference);
                self.change_request_refresh_failures.remove(&reference);
            }
            Err(error) => {
                tracing::warn!(convoy = %reference.name, %branch, %error, "failed to refresh convoy change request");
                failed = true;
            }
        }
        if self.is_home_convoy(&reference) {
            if let Some(resolver) = &self.change_request_resolver {
                if let Err(error) =
                    resolver.discover_branch_subjects(&reference.namespace, &reference.name, &branch, discovery_resolution).await
                {
                    tracing::warn!(convoy = %reference.name, %branch, %error, "failed to discover convoy branch subjects");
                    failed = true;
                }
            }
        }
        if failed {
            *self.change_request_refresh_failures.entry(reference.clone()).or_default() += 1;
        }
        if let Some(convoy) = self.effective_convoys().get(&reference) {
            // A failed lookup by branch read only records: the next change to them looks again,
            // rather than a retry. A bound lookup retries under its backoff.
            if retrigger || (failed && convoy.spec.change_request.is_some()) {
                self.schedule_change_request_refresh(convoy);
            }
        }
        self.rebuild_local_projection().await;
    }

    /// Looks up again the change request of each active home convoy that produces none yet, as
    /// the change request records change: what a forge observer's branch probe found is read then.
    fn recheck_unproduced_change_requests(&mut self) {
        let convoys = self.effective_convoys().into_iter().filter(|(reference, convoy)| {
            self.is_home_convoy(reference)
                && convoy.status.as_ref().is_some_and(|status| status.phase == ResourceConvoyPhase::Active)
                && !convoy.status.iter().flat_map(|status| &status.subjects).any(|entry| {
                    entry.subject.kind == flotilla_protocol::SubjectKind::ChangeRequest
                        && entry.relationship == flotilla_protocol::Relationship::Produces
                })
                && !self.change_request_refresh_tasks.contains_key(reference)
        });
        self.schedule_change_request_refresh_pass(convoys.collect::<Vec<_>>());
    }

    async fn refresh_all_change_requests(&mut self) {
        let effective_convoys = self.effective_convoys();
        self.convoy_change_requests.retain(|reference, _| effective_convoys.contains_key(reference));
        self.change_request_refresh_generations.retain(|reference, _| effective_convoys.contains_key(reference));
        self.change_request_refresh_started.retain(|reference, _| effective_convoys.contains_key(reference));
        self.change_request_refresh_failures.retain(|reference, _| effective_convoys.contains_key(reference));
        self.schedule_change_request_refresh_pass(effective_convoys);
        self.rebuild_local_projection().await;
    }

    async fn refresh_repository_change_requests(&mut self, repo_identity: &RepoIdentity) {
        let effective_convoys =
            self.effective_convoys().into_iter().filter(|(_, convoy)| convoy_references_repo(convoy, repo_identity)).collect::<Vec<_>>();
        self.schedule_change_request_refresh_pass(effective_convoys);
        self.rebuild_local_projection().await;
    }

    fn schedule_change_request_refresh_pass(&mut self, convoys: impl IntoIterator<Item = (ResourceRef, ResourceObject<Convoy>)>) {
        // Repository order is resolver precedence, so only identical ordered
        // repository lists may safely share one lookup.
        let mut lookups = HashMap::<(Vec<RepositoryKey>, String, Option<(RepositoryKey, String)>), ChangeRequestRefreshGroup>::new();
        for (reference, convoy) in convoys {
            if !self.is_home_convoy(&reference) {
                continue;
            }
            if convoy.status.as_ref().is_some_and(|status| convoy_phase_is_terminal(status.phase)) {
                continue;
            }
            // Keep a pending lookup: replacing it would restart its wait from its
            // last start, so a busy repository could defer a convoy's first branch
            // scan indefinitely (#3206). One not yet started will read the
            // triggering change; one already running may not, so it looks again
            // once its result is applied. The kept lookup captured the convoy's
            // branch, repositories and binding: this pass serves rechecks and
            // repository triggers only; spec changes go through
            // `schedule_change_request_refresh`, which replaces the task.
            if self.change_request_refresh_tasks.contains_key(&reference) {
                let started = self.change_request_refresh_started.get(&reference).is_some_and(|at| *at <= Instant::now());
                if started {
                    self.change_request_retriggers.insert(reference);
                }
                continue;
            }
            let generation = uuid::Uuid::new_v4();
            self.change_request_refresh_generations.insert(reference.clone(), generation);
            let Some(branch) = convoy.spec.r#ref.clone() else {
                self.convoy_change_requests.remove(&reference);
                continue;
            };
            let repositories = convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>();
            let binding = convoy.spec.change_request.as_ref().map(|binding| (binding.repository_ref.clone(), binding.id.clone()));
            if repositories.is_empty() && binding.is_none() {
                self.convoy_change_requests.remove(&reference);
                continue;
            }
            lookups
                .entry((repositories, branch, binding))
                .or_insert_with(|| ChangeRequestRefreshGroup { binding: convoy.spec.change_request.clone(), targets: Vec::new() })
                .targets
                .push((reference, generation));
        }

        let Some(resolver) = self.change_request_resolver.clone() else {
            return;
        };
        for ((repositories, branch, _), ChangeRequestRefreshGroup { binding, targets }) in lookups {
            let now = Instant::now();
            let refresh_at = targets
                .iter()
                .map(|(reference, _)| self.next_change_request_refresh(reference, binding.is_some(), now))
                .max()
                .unwrap_or(now);
            for (reference, _) in &targets {
                self.change_request_refresh_started.insert(reference.clone(), refresh_at);
            }
            let resolver = Arc::clone(&resolver);
            let lookup_branch = branch.clone();
            let lookup = async move {
                tokio::time::sleep_until(refresh_at).await;
                bounded_refresh(&*resolver, &repositories, &lookup_branch, binding.as_ref()).await
            }
            .boxed()
            .shared();
            for (reference, generation) in targets {
                let lookup = lookup.clone();
                let refresh_tx = self.change_request_refresh_queue.tx.clone();
                let task_reference = reference.clone();
                let task_branch = branch.clone();
                let task = tokio::spawn(async move {
                    let result = lookup.await;
                    let _ = refresh_tx.send(ChangeRequestResolution { reference: task_reference, generation, branch: task_branch, result });
                });
                self.change_request_refresh_tasks.insert(reference, task);
            }
        }
    }

    fn repo_delta_changed_change_requests(&self, delta: &RepoDelta) -> bool {
        delta.changes.iter().any(|change| matches!(change, Change::ChangeRequest { .. }))
    }

    fn effective_convoys(&self) -> HashMap<ResourceRef, ResourceObject<Convoy>> {
        self.effective_convoy_reads().into_iter().map(|(reference, convoy)| (reference, convoy.object)).collect()
    }

    fn effective_convoy_reads(&self) -> HashMap<ResourceRef, ReadResourceObject<Convoy>> {
        let mut effective_convoys = self.effective_convoy_reads_including_deleted();
        effective_convoys.retain(|_, convoy| convoy.object.metadata.deletion_timestamp.is_none());
        effective_convoys
    }

    fn effective_convoy_reads_including_deleted(&self) -> HashMap<ResourceRef, ReadResourceObject<Convoy>> {
        #[cfg(test)]
        self.copied_convoys.fetch_add(self.convoys.len(), std::sync::atomic::Ordering::Relaxed);
        self.convoys
            .values()
            .filter_map(|convoy| self.read_convoy_ref(convoy).map(|reference| (reference, convoy.as_ref().clone())))
            .collect()
    }

    async fn rebuild_local_projection(&mut self) {
        self.projection_full = true;
        self.schedule_local_projection().await;
    }

    async fn schedule_local_projection(&mut self) {
        if let ConvoyProjectionDelivery::Coalesced { deadline } = &mut self.projection_delivery {
            // Keep the first deadline: a continuous stream cannot postpone delivery.
            deadline.get_or_insert_with(|| Instant::now() + CONVOY_PROJECTION_COALESCE_INTERVAL);
            return;
        }
        self.flush_local_projection().await;
    }

    async fn flush_local_projection(&mut self) {
        let full = std::mem::take(&mut self.projection_full);
        if full {
            self.refresh_origin_hosts().await;
            self.rebuild_convoy_inputs();
        }
        let keys = std::mem::take(&mut self.projection_keys);
        // A deletion timestamp begins finalizer-driven reaping; it does not
        // mean the Convoy or its child resources have left the store yet.
        // Keep the parent row projected until the watch emits Deleted so
        // vessels, checkouts, and sessions never appear parentless during
        // teardown.
        let effective_convoys = if full { self.effective_convoy_reads_including_deleted() } else { HashMap::new() };
        let salience_changed = self.rebuild_salience_projection().await;
        let mut local_replacement = HashMap::new();
        let mut replica_replacements: HashMap<HostName, HashMap<ResourceRef, ConvoyRow>> = HashMap::new();
        // A convoy whose engine read failed keeps its row until a retried projection reads it.
        let mut unread: Vec<(ResourceRef, ResourceProvenance)> = Vec::new();
        for (reference, convoy) in effective_convoys {
            let viewed = match self.viewed(&convoy) {
                Ok(viewed) => viewed,
                Err(error) => {
                    tracing::warn!(convoy = %reference.name, %error, "the engine convoy did not read; its row stays until a retry");
                    unread.push((reference, convoy.provenance.clone()));
                    continue;
                }
            };
            let row = self.summarize(&reference, &viewed);
            match convoy.provenance {
                ResourceProvenance::Local => {
                    local_replacement.insert(row.resource.clone(), row);
                }
                ResourceProvenance::Replica { .. } => {
                    let host = reference.host.clone().expect("replica convoy reference is host-qualified");
                    replica_replacements.entry(host).or_default().insert(row.resource.clone(), row);
                }
            }
        }

        let mut retry = Vec::new();
        let updates = if full {
            Vec::new()
        } else {
            keys.iter()
                .filter_map(|key| {
                    let reference = self.convoy_key_ref(key)?;
                    let row = match self.effective_convoy_key(key) {
                        Some(item) => match self.viewed(item) {
                            Ok(viewed) => Some(self.summarize(&reference, &viewed)),
                            Err(error) => {
                                // Keep the row as it is, and project it again.
                                tracing::warn!(convoy = %reference.name, %error, "the engine convoy did not read; its row stays until a retry");
                                retry.push(key.clone());
                                return None;
                            }
                        },
                        None => None,
                    };
                    Some((reference, key.2.is_some(), row))
                })
                .collect::<Vec<_>>()
        };
        if !retry.is_empty() || !unread.is_empty() {
            // Project the unread convoys again shortly; their rows stay meanwhile.
            self.projection_keys.extend(retry);
            self.projection_full |= !unread.is_empty();
            if let ConvoyProjectionDelivery::Coalesced { deadline } = &mut self.projection_delivery {
                deadline.get_or_insert_with(|| Instant::now() + ENGINE_READ_RETRY);
            }
        }
        let (changed, removed, result_set) = {
            let mut view = self.state.write().await;
            let mut incremental_changed = Vec::new();
            let mut incremental_removed = Vec::new();
            if !full {
                for (reference, replica, row) in updates {
                    let rows = if replica {
                        view.replica_rows.entry(reference.host.clone().expect("replica host")).or_default()
                    } else {
                        &mut view.local_rows
                    };
                    match row {
                        Some(row) if rows.get(&reference) != Some(&row) => {
                            rows.insert(reference, row.clone());
                            incremental_changed.push(row);
                        }
                        None if rows.remove(&reference).is_some() => incremental_removed.push(reference),
                        _ => {}
                    }
                }
            }
            // An unread convoy keeps its prior row: neither changed nor removed.
            for (reference, provenance) in &unread {
                match provenance {
                    ResourceProvenance::Local => {
                        if let Some(row) = view.local_rows.get(reference) {
                            local_replacement.insert(reference.clone(), row.clone());
                        }
                    }
                    ResourceProvenance::Replica { .. } => {
                        let Some(host) = reference.host.clone() else { continue };
                        if let Some(row) = view.replica_rows.get(&host).and_then(|rows| rows.get(reference)) {
                            replica_replacements.entry(host).or_default().insert(reference.clone(), row.clone());
                        }
                    }
                }
            }
            let changed = if !full {
                incremental_changed
            } else {
                local_replacement
                    .iter()
                    .filter(|(reference, row)| view.local_rows.get(*reference) != Some(*row))
                    .map(|(_, row)| row.clone())
                    .chain(replica_replacements.iter().flat_map(|(host, rows)| {
                        let prior = view.replica_rows.get(host);
                        rows.iter()
                            .filter(move |(reference, row)| prior.and_then(|prior| prior.get(*reference)) != Some(*row))
                            .map(|(_, row)| row.clone())
                    }))
                    .collect::<Vec<_>>()
            };
            let removed = if !full {
                incremental_removed
            } else {
                view.local_rows
                    .keys()
                    .filter(|reference| !local_replacement.contains_key(*reference))
                    .cloned()
                    .chain(view.replica_rows.iter().flat_map(|(host, rows)| {
                        let replacement = replica_replacements.get(host);
                        rows.keys()
                            .filter(move |reference| replacement.is_none_or(|replacement| !replacement.contains_key(*reference)))
                            .cloned()
                    }))
                    .collect::<Vec<_>>()
            };
            if changed.is_empty() && removed.is_empty() {
                drop(view);
                if salience_changed && !self.bootstrapping {
                    self.emit_awareness_result_sets().await;
                }
                return;
            }
            if full {
                view.local_rows = local_replacement;
                view.replica_rows = replica_replacements;
            }
            view.seq = view.seq.saturating_add(1);
            (changed, removed, (!self.emitted_queries.contains(&QueryId::Convoys { scope: None })).then(|| view.result_set()))
        };

        if self.bootstrapping {
            return;
        }
        if self.emitted_queries.contains(&QueryId::Convoys { scope: None }) {
            self.emit_delta(changed, removed).await;
        } else {
            self.emitted_queries.insert(QueryId::Convoys { scope: None });
            self.event_sink.emit(DaemonEvent::ResultSet(Box::new(result_set.expect("initial projection snapshot"))));
        }
        self.emit_scoped_convoy_result_sets().await;
        let represented = self.state.represented_issue_refs().await;
        for delta in self.state.suppress_issues(&represented) {
            self.event_sink.emit(DaemonEvent::ResultDelta(Box::new(delta)));
        }
        if let Some(materializer) = &self.issue_materializer {
            // The direct delta hides represented rows immediately; refiltering
            // republishes loaded windows so rows can reappear when representation
            // changes again.
            materializer.refilter_active_queries();
        }
        if salience_changed {
            self.emit_awareness_result_sets().await;
        }
    }

    #[cfg(test)]
    async fn replace_session_source(&mut self, source: LocalSource, sessions: Vec<ResourceObject<TerminalSession>>) {
        self.replace_session_read_source(
            source,
            sessions.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect(),
        )
        .await;
    }

    async fn replace_replica_sessions(&mut self, sessions: Vec<ReadResourceObject<TerminalSession>>) {
        self.replace_session_read_source(LocalSource::Durable, sessions).await;
    }

    async fn replace_session_read_source(&mut self, source: LocalSource, sessions: Vec<ReadResourceObject<TerminalSession>>) {
        let replacement = sessions.into_iter().map(|session| (session_key(&session), session)).collect();
        self.sessions_by_source.insert(source, replacement);
        self.rebuild_independents_projection().await;
        self.rebuild_local_projection().await;
    }

    #[cfg(test)]
    async fn apply_convoy_event(&mut self, event: WatchEvent<Convoy>) {
        self.apply_replica_convoy_event(local_read_event(event)).await;
    }

    #[cfg(test)]
    async fn apply_session_event_from(&mut self, source: LocalSource, event: WatchEvent<TerminalSession>) {
        self.apply_session_read_event_from(source, local_read_event(event)).await;
    }

    async fn apply_replica_session_event(&mut self, event: ReadWatchEvent<TerminalSession>) {
        self.apply_session_read_event_from(LocalSource::Durable, event).await;
    }

    async fn apply_session_read_event_from(&mut self, source: LocalSource, event: ReadWatchEvent<TerminalSession>) {
        match event {
            ReadWatchEvent::Added(session) | ReadWatchEvent::Modified(session) => {
                self.sessions_by_source.entry(source).or_default().insert(session_key(&session), session);
            }
            ReadWatchEvent::Deleted(session) => {
                self.sessions_by_source.entry(source).or_default().remove(&session_key(&session));
            }
            ReadWatchEvent::DeletedByName { tombstone, provenance } => {
                self.sessions_by_source.entry(source).or_default().remove(&tombstone_key(&tombstone, &provenance));
            }
        }
        self.rebuild_independents_projection().await;
        self.rebuild_local_projection().await;
    }

    /// AgentSessions changed in the decided store: the convoys whose crew run in them are
    /// projected again; a home reread projects every convoy.
    async fn agent_sessions_changed(&mut self, update: AgentSessionUpdate) {
        match update {
            AgentSessionUpdate::Changed(sessions) => {
                let mut changed = Vec::new();
                for (key, reading) in sessions {
                    let differs = match reading {
                        Some(reading) => self.agent_sessions.insert(key.clone(), reading.clone()).as_ref() != Some(&reading),
                        None => self.agent_sessions.remove(&key).is_some(),
                    };
                    if differs {
                        changed.push(key);
                    }
                }
                let keys: Vec<ConvoyKey> = changed.iter().flat_map(|key| self.convoys_running_in(key)).collect();
                if !keys.is_empty() {
                    self.projection_keys.extend(keys);
                    self.schedule_local_projection().await;
                }
            }
            AgentSessionUpdate::Heard { home, at } => {
                let now = self.clock.now();
                let was = agent_activity::home_current(self.agent_session_heard.get(&home).copied(), now);
                self.agent_session_heard.insert(home, at);
                // A keepalive from a home already current changes no row.
                if was != agent_activity::home_current(Some(at), now) {
                    self.rebuild_local_projection().await;
                }
            }
            AgentSessionUpdate::Home { home, sessions } => {
                let held: HashMap<_, _> =
                    sessions.into_iter().map(|(name, reading)| (AgentSessionKey { home: home.clone(), name }, reading)).collect();
                let previous: HashMap<_, _> = self.agent_sessions.extract_if(|key, _| key.home == home).collect();
                let changed = previous != held;
                self.agent_sessions.extend(held);
                if changed {
                    self.rebuild_local_projection().await;
                }
            }
        }
    }

    /// The convoys with a crew member running in the launch that `key` names.
    fn convoys_running_in(&self, key: &AgentSessionKey) -> Vec<ConvoyKey> {
        self.terminal_sessions
            .values()
            .filter(|session| {
                flotilla_resources::terminal_records::launch_of(&session.object) == Some(key.name.as_str())
                    && self.agent_session_home_of(session).as_ref() == Some(&key.home)
            })
            .filter_map(|session| {
                Some((session.object.metadata.namespace.clone(), session.object.metadata.labels.get(CONVOY_LABEL)?.clone()))
            })
            .flat_map(|convoy| self.convoy_keys_by_name.get(&convoy).into_iter().flatten().cloned())
            .collect()
    }

    /// The home whose AgentSession a crew in `session` records on: the session's own home.
    fn agent_session_home_of(&self, session: &ReadResourceObject<TerminalSession>) -> Option<flotilla_protocol::NodeId> {
        match &session.provenance {
            ResourceProvenance::Local => self.agent_session_home.clone(),
            ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
        }
    }

    /// The AgentSession of the crew member running in `session`, the one its home holds under the
    /// launch the session's view shows, as this home reads it at `now`: this home's own are always
    /// current, another home's while that home is heard from.
    fn agent_view(&self, session: &ReadResourceObject<TerminalSession>, now: chrono::DateTime<chrono::Utc>) -> Option<AgentActivityView> {
        let home = self.agent_session_home_of(session)?;
        let current = Some(&home) == self.agent_session_home.as_ref()
            || agent_activity::home_current(self.agent_session_heard.get(&home).copied(), now);
        let name = flotilla_resources::terminal_records::launch_of(&session.object)?.to_owned();
        let reading = self.agent_sessions.get(&AgentSessionKey { home, name })?.clone();
        Some(AgentActivityView { reading, current, progress: None })
    }

    /// An environment's engine record changed, or any of them may have (`None` or a rescan):
    /// sessions in it are attachable afresh.
    async fn environment_changed(&mut self, change: Option<EnvironmentChange>) {
        match change {
            Some(EnvironmentChange::Changed(name)) => self.attach_refresh_inputs.retain(|_, input| input.environment != name),
            Some(EnvironmentChange::Rescanned) | None => self.attach_refresh_inputs.clear(),
        }
        self.rebuild_independents_projection().await;
        self.rebuild_local_projection().await;
    }

    async fn apply_project_event(&mut self, event: ReadWatchEvent<Project>) {
        match event {
            ReadWatchEvent::Added(project) | ReadWatchEvent::Modified(project) => {
                let project = project.object;
                self.projects.insert((project.metadata.namespace.clone(), project.metadata.name.clone()), project);
            }
            ReadWatchEvent::Deleted(project) => {
                let project = project.object;
                self.projects.remove(&(project.metadata.namespace, project.metadata.name));
            }
            ReadWatchEvent::DeletedByName { tombstone, .. } => {
                self.projects.remove(&(tombstone.namespace, tombstone.name));
            }
        }
        self.rebuild_store_catalog().await;
    }

    async fn apply_convoy_ensure_event(&mut self, event: ReadWatchEvent<ConvoyEnsure>) {
        let key = match event {
            ReadWatchEvent::Added(ensure) | ReadWatchEvent::Modified(ensure) => {
                let key = repository_source_key(&ensure.object.metadata.namespace, &ensure.object.metadata.name, &ensure.provenance);
                self.convoy_ensures.insert(key.clone(), ensure.object);
                key
            }
            ReadWatchEvent::Deleted(ensure) => {
                let key = repository_source_key(&ensure.object.metadata.namespace, &ensure.object.metadata.name, &ensure.provenance);
                self.convoy_ensures.remove(&key);
                key
            }
            ReadWatchEvent::DeletedByName { tombstone, provenance } => {
                let key = tombstone_key(&tombstone, &provenance);
                self.convoy_ensures.remove(&key);
                key
            }
        };
        self.refresh_standing_role(&key.0, &key.1).await;
    }

    /// One declaration per role. Several roots may hold a copy of the same definition; any copy
    /// serves, since nothing of the role's state is stored on it (#3108). This host's comes first.
    fn standing_role_declarations(&self) -> BTreeMap<(String, String), ResourceObject<ConvoyEnsure>> {
        let mut declared = BTreeMap::new();
        for ((namespace, name, _), ensure) in &self.convoy_ensures {
            declared.entry((namespace.clone(), name.clone())).or_insert_with(|| ensure.clone());
        }
        declared
    }

    /// Re-reads every declared role and republishes their rows.
    async fn rebuild_standing_roles(&mut self) {
        let declared = self.standing_role_declarations();
        for (key, ensure) in &declared {
            let reading = self.standing_role_port.reading(&key.0, &ensure.spec.project_ref, &ensure.spec.role).await;
            self.standing_readings.insert(key.clone(), reading);
        }
        self.publish_standing_roles(&declared).await;
    }

    /// Re-reads the role that ensure `name` in `namespace` declares, after a change to it or to
    /// one of its generations.
    async fn refresh_standing_role(&mut self, namespace: &str, name: &str) {
        let declared = self.standing_role_declarations();
        let key = (namespace.to_owned(), name.to_owned());
        if let Some(ensure) = declared.get(&key) {
            let reading = self.standing_role_port.reading(namespace, &ensure.spec.project_ref, &ensure.spec.role).await;
            self.standing_readings.insert(key, reading);
        }
        self.publish_standing_roles(&declared).await;
    }

    async fn publish_standing_roles(&mut self, declared: &BTreeMap<(String, String), ResourceObject<ConvoyEnsure>>) {
        self.standing_readings.retain(|key, _| declared.contains_key(key));
        let rows =
            declared.iter().map(|(key, ensure)| standing_role_row(ensure, self.standing_readings.get(key).cloned().flatten())).collect();
        let deltas = self.state.replace_standing_role_rows(rows).await;
        self.emit_store_deltas(deltas).await;
    }

    #[cfg(test)]
    async fn apply_repository_event(&mut self, event: WatchEvent<Repository>) {
        self.apply_repository_read_event(local_read_event(event)).await;
    }

    async fn apply_repository_read_event(&mut self, event: ReadWatchEvent<Repository>) {
        match event {
            ReadWatchEvent::Added(source) | ReadWatchEvent::Modified(source) => {
                let key = repository_source_key(&source.object.metadata.namespace, &source.object.metadata.name, &source.provenance);
                self.repository_sources.insert(key, source);
            }
            ReadWatchEvent::Deleted(source) => {
                self.repository_sources.remove(&repository_source_key(
                    &source.object.metadata.namespace,
                    &source.object.metadata.name,
                    &source.provenance,
                ));
            }
            ReadWatchEvent::DeletedByName { tombstone, provenance } => {
                self.repository_sources.remove(&repository_source_key(&tombstone.namespace, &tombstone.name, &provenance));
            }
        }
        self.rebuild_repository_catalog().await;
    }

    async fn rebuild_repository_catalog(&mut self) {
        self.repositories.clear();
        // Repository identity is convergent. Retain every source so deleting
        // one root's observation cannot erase another root's catalogue entry.
        // Ordered origins make the representative stable, with local first.
        for source in self.repository_sources.values() {
            self.repositories.entry(source.object.spec.key()).or_insert_with(|| source.object.clone());
        }
        self.rebuild_store_catalog().await;
        if let Err(error) = self.rebuild_checkout_rows().await {
            debug!(%error, "could not refresh checkout rows after repository update");
        }
    }

    async fn discovered_checkouts_changed(&mut self) -> Result<(), ResourceError> {
        self.rebuild_checkout_rows().await?;
        if self.rebuild_salience_projection().await {
            self.emit_awareness_result_sets().await;
        }
        Ok(())
    }

    fn repository_labels(&self) -> HashMap<RepositoryKey, String> {
        repository_display_labels(self.repositories.iter().map(|(key, repository)| (key, &repository.spec))).into_iter().collect()
    }

    async fn rebuild_store_catalog(&self) {
        let repositories = self.repository_labels();
        let projects = self
            .projects
            .values()
            .map(|project| {
                let scope = QueryScope::new(project.metadata.namespace.clone(), project.metadata.name.clone());
                let repositories = project.spec.repositories.iter().map(|repository| repository.repo.clone()).collect();
                (scope, repositories)
            })
            .collect();
        let mut deltas = self.state.replace_store_catalog(repositories, projects).await;
        let namespaces = self.projects.keys().map(|(namespace, _)| namespace).collect::<BTreeSet<_>>();
        let hierarchies = namespaces
            .into_iter()
            .filter_map(|namespace| {
                let declared = self
                    .projects
                    .iter()
                    .filter(|((ns, _), _)| ns == namespace)
                    .map(|((_, name), project)| (name.clone(), project.spec.parent.clone()))
                    .collect();
                match flotilla_resources::ProjectHierarchy::new(declared, self.fleet_projects.get(namespace).cloned()) {
                    Ok(hierarchy) => Some((namespace, hierarchy)),
                    Err(error) => {
                        tracing::warn!(%namespace, %error, "Project hierarchy unavailable during federation");
                        None
                    }
                }
            })
            .collect::<BTreeMap<_, _>>();
        let rows = self
            .projects
            .values()
            .map(|project| ProjectRepositoriesRow {
                parent: hierarchies
                    .get(&project.metadata.namespace)
                    .and_then(|hierarchy| hierarchy.parent(&project.metadata.name).ok().flatten())
                    .map(|name| ResourceRef::new("flotilla.work/v1", "Project", &project.metadata.namespace, name)),
                resource: ResourceRef::new(
                    api_version(Project::API_PATHS),
                    Project::API_PATHS.kind,
                    &project.metadata.namespace,
                    &project.metadata.name,
                ),
                display_name: project.spec.display_name.clone(),
                repositories: project
                    .spec
                    .repositories
                    .iter()
                    .map(|membership| ProjectRepositoryMembership {
                        key: membership.repo.clone(),
                        slug: self.repositories.get(&membership.repo).map(|repo| repo.spec.catalog_slug()),
                        subpath: membership.subpath.clone(),
                    })
                    .collect(),
            })
            .collect();
        deltas.extend(self.state.replace_project_repository_rows(rows).await);
        let rows = self
            .projects
            .values()
            .filter(|project| project.status.as_ref().is_none_or(|status| status.dispatch_queue_error.is_none()))
            .flat_map(|project| {
                let attention = project.status.as_ref().is_some_and(|status| status.dispatch_queue_attention.is_some());
                project.status.as_ref().into_iter().flat_map(|status| status.dispatch_queue.iter()).map(move |entry| {
                    flotilla_protocol::DispatchQueueRow::builder()
                        .namespace(project.metadata.namespace.clone())
                        .project(project.metadata.name.clone())
                        .issue(entry.issue.clone())
                        .title(entry.title.clone())
                        .ready_observed_at(entry.ready_observed_at)
                        .age_seconds(self.clock.now().signed_duration_since(entry.ready_observed_at).num_seconds().max(0) as u64)
                        .attention(attention)
                        .maybe_score(entry.score.clone())
                        .provenance(entry.provenance.clone())
                        .build()
                })
            })
            .collect();
        let errors =
            self.projects
                .values()
                .filter_map(|project| {
                    project.status.as_ref()?.dispatch_queue_error.as_ref().map(|error| {
                        (flotilla_protocol::QueryScope::new(&project.metadata.namespace, &project.metadata.name), error.clone())
                    })
                })
                .collect();
        deltas.extend(self.state.replace_dispatch_ready_rows(rows, errors).await);
        self.emit_store_deltas(deltas).await;
    }

    async fn rebuild_checkout_rows(&self) -> Result<(), ResourceError> {
        let mut replicas = HashMap::<HostName, Vec<CheckoutRow>>::new();
        let mut local = Vec::new();
        // This host's observations, and the checkouts the engine adopted at every home it follows.
        let adopted = self
            .engine_checkouts
            .iter()
            .filter_map(|record| {
                let host = self.engine_home_host(&record.home)?;
                let mut view = record.read_view();
                view.metadata.namespace = self.engine_namespace.clone();
                Some((view, host))
            })
            .collect::<Vec<_>>();
        let discovered = self.discovered_checkouts.views(&self.discovered_namespace);
        let sources = discovered
            .iter()
            .map(|checkout| (checkout, self.local_host.clone()))
            .chain(adopted.iter().map(|(checkout, host)| (checkout, host.clone())));
        for (checkout, host) in sources {
            let CheckoutSpec::Observed(spec) = &checkout.spec else { continue };
            let authority = checkout.metadata.lifecycle_authority()?.unwrap_or(LifecycleAuthority::Observed);
            let for_convoy = checkout.metadata.labels.get(CONVOY_LABEL).cloned();
            let attention_reason =
                for_convoy.as_ref().filter(|convoy| !self.has_convoy(&checkout.metadata.namespace, convoy)).map(|convoy| {
                    format!(
                        "checkout {} at {} references missing convoy {}/{}",
                        checkout.metadata.name, spec.path, checkout.metadata.namespace, convoy
                    )
                });
            let row = CheckoutRow::builder()
                .resource(self.checkout_ref(&checkout.metadata.namespace, &checkout.metadata.name).on_host(host.clone()))
                .repo(spec.repo_ref.clone())
                .repo_label(
                    self.repositories
                        .get(&spec.repo_ref)
                        .map(|repository| repository.spec.qualified_label())
                        .unwrap_or_else(|| UNKNOWN_REPOSITORY_LABEL.to_string()),
                )
                .maybe_repo_fact(
                    self.repositories.get(&spec.repo_ref).map(|repository| flotilla_protocol::RepoKey(repository.spec.repo_fact_value())),
                )
                .path(spec.path.clone())
                .branch(spec.r#ref.clone())
                .host(host.clone())
                .authority(authority)
                .maybe_for_convoy(for_convoy)
                .needs_attention(attention_reason.is_some())
                .maybe_attention_reason(attention_reason)
                .build();
            if host == self.local_host {
                local.push(row);
            } else {
                replicas.entry(host).or_default().push(row);
            }
        }
        let mut deltas = self.state.replace_local_checkout_rows(local).await;
        deltas.extend(self.state.replace_checkout_replica_rows(replicas).await);
        self.emit_store_deltas(deltas).await;
        Ok(())
    }

    async fn emit_store_deltas(&self, deltas: Vec<ResultDelta>) {
        if self.bootstrapping {
            return;
        }
        let changed = !deltas.is_empty();
        for delta in deltas {
            self.event_sink.emit(DaemonEvent::ResultDelta(Box::new(delta)));
        }
        if changed {
            self.emit_awareness_result_sets().await;
        }
    }

    async fn rebuild_independents_projection(&mut self) {
        let mut effective_sessions = HashMap::new();
        for source in LOCAL_SOURCE_PRECEDENCE {
            let Some(sessions) = self.sessions_by_source.get(&source) else { continue };
            effective_sessions.extend(sessions.iter().map(|(key, session)| (key.clone(), session.clone())));
        }
        self.terminal_sessions = effective_sessions;
        self.refresh_origin_hosts().await;
        let salience_changed = self.rebuild_salience_projection().await;

        self.refresh_attach_capabilities();
        self.publish_independents_projection(salience_changed).await;
    }

    fn refresh_attach_capabilities(&mut self) {
        // Running status is a liveness gate, not part of the provider target.
        // Keep the last verified capability during rechecks; only a current
        // negative result or removal from the running set withdraws it.
        let inputs = self
            .terminal_sessions
            .iter()
            .filter_map(|(key, session)| {
                let status = session.object.status.as_ref()?;
                if status.phase != TerminalSessionPhase::Running {
                    return None;
                }
                Some((
                    key.clone(),
                    AttachCapabilityInput {
                        host: self.read_host(&session.provenance)?,
                        session_id: status.session_id.clone(),
                        endpoint: status.cleat_endpoint.clone(),
                        environment: session.object.spec.env_ref.clone(),
                        pool: session.object.spec.pool.clone(),
                    },
                ))
            })
            .collect::<HashMap<_, _>>();
        self.attachable_sessions.retain(|key| inputs.contains_key(key));
        self.attach_refresh_generations.retain(|key, (_, task)| {
            let unchanged = inputs.get(key).is_some_and(|input| self.attach_refresh_inputs.get(key) == Some(input));
            if !unchanged {
                task.abort();
            }
            unchanged
        });
        let Some(resolver) = self.attach_resolver.clone() else {
            self.attach_refresh_inputs = inputs;
            self.attachable_sessions.clear();
            return;
        };
        for (key, input) in &inputs {
            if self.attach_refresh_generations.contains_key(key) {
                continue;
            }
            let generation = uuid::Uuid::new_v4();
            let key = key.clone();
            let name = self.terminal_sessions[&key].object.metadata.name.clone();
            let host = input.host.clone();
            let resolver = resolver.clone();
            let result_key = key.clone();
            let task = self.attach_refresh_tasks.spawn(async move {
                let targets = [(name, host)];
                let mut delay = Duration::from_millis(250);
                let mut warned = false;
                loop {
                    let error = match resolver.resolvable_attach_targets(&targets).await {
                        Ok(resolved) if resolved.len() == 1 => {
                            return AttachCapabilityResolution { generation, key: result_key, attachable: resolved[0] };
                        }
                        Ok(_) => "terminal capability resolver returned an invalid result count".to_string(),
                        Err(error) => error,
                    };
                    if warned {
                        tracing::debug!(%error, "terminal capability check still failing; retrying");
                    } else {
                        tracing::warn!(%error, "terminal capability check failed; retrying");
                        warned = true;
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            });
            self.attach_refresh_generations.insert(key, (generation, task));
        }
        self.attach_refresh_inputs = inputs;
    }

    async fn apply_attach_capability_resolution(&mut self, resolution: AttachCapabilityResolution) -> Result<(), ResourceError> {
        if !self.attach_refresh_generations.get(&resolution.key).is_some_and(|(generation, _)| *generation == resolution.generation) {
            return Ok(());
        }
        if resolution.attachable {
            self.attachable_sessions.insert(resolution.key);
        } else {
            self.attachable_sessions.remove(&resolution.key);
        }
        self.publish_independents_projection(false).await;
        self.rebuild_local_projection().await;
        self.rebuild_checkout_rows().await
    }

    async fn publish_independents_projection(&self, salience_changed: bool) {
        let mut local_replacement = Vec::new();
        let mut replica_replacements: HashMap<HostName, Vec<IndependentRow>> = HashMap::new();
        for session in self.terminal_sessions.values() {
            let Some(row) = self.summarize_independent(session, &self.attachable_sessions) else { continue };
            match &session.provenance {
                ResourceProvenance::Local => local_replacement.push(row),
                ResourceProvenance::Replica { .. } => {
                    replica_replacements.entry(row.host.clone()).or_default().push(row);
                }
            }
        }
        let mut deltas = self.state.replace_local_independent_rows(local_replacement).await;
        deltas.extend(self.state.replace_independent_replica_rows(replica_replacements).await);
        let store_changed = !deltas.is_empty();
        self.emit_store_deltas(deltas).await;
        if salience_changed && !store_changed && !self.bootstrapping {
            self.emit_awareness_result_sets().await;
        }
    }

    async fn emit_delta(&self, changed: Vec<ConvoyRow>, removed: Vec<ResourceRef>) {
        let seq = self.state.seq().await;
        let changes = QueryChanges::Convoys { scope: None, changed, removed };
        self.event_sink.emit(DaemonEvent::ResultDelta(Box::new(ResultDelta { seq, changes, state: None })));
        self.emit_awareness_result_sets().await;
    }

    async fn emit_scoped_convoy_result_sets(&self) {
        let subscribed_scopes =
            self.state.subscribed_queries().into_iter().filter(|query| matches!(query, QueryId::Convoys { scope: Some(_) })).count();
        let result_sets = self.state.changed_scoped_convoy_result_sets().await;
        let scoped_row_counts = result_sets.iter().map(|result_set| (result_set.query(), result_set.rows.len())).collect::<Vec<_>>();
        debug!(
            subscribed_scopes,
            changed_scopes = result_sets.len(),
            scoped_row_counts = ?scoped_row_counts,
            "projected scoped convoy snapshots"
        );
        for result_set in result_sets {
            self.event_sink.emit(DaemonEvent::ResultSet(Box::new(result_set)));
        }
    }

    async fn rebuild_salience_projection(&self) -> bool {
        self.state.replace_salience_facts(self.salience_facts()).await
    }

    fn salience_facts(&self) -> SalienceFacts {
        self.salience_facts_at(self.clock.now())
    }

    fn salience_facts_at(&self, now: chrono::DateTime<chrono::Utc>) -> SalienceFacts {
        let raised = self
            .attentions
            .iter()
            .map(|open| RaisedFact {
                target: self.attention_target(&open.subject),
                escalated: open.escalated_at(now),
                as_of: open.raised_at,
            })
            .collect();
        let mut sessions = self.terminal_sessions.values().collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            (&left.object.metadata.namespace, &left.object.metadata.name)
                .cmp(&(&right.object.metadata.namespace, &right.object.metadata.name))
        });
        let attention = sessions
            .into_iter()
            .flat_map(|source| {
                let session = source;
                let Some(session_host) = self.read_host(&session.provenance) else { return Vec::new() };
                let session = &session.object;
                let Some(convoy) = session.metadata.labels.get(CONVOY_LABEL) else { return Vec::new() };
                let Some(vessel) = session.metadata.labels.get(VESSEL_LABEL) else { return Vec::new() };
                let Some(status) = session.status.as_ref() else { return Vec::new() };
                if status.phase != TerminalSessionPhase::Running {
                    return Vec::new();
                }
                let Some(agent) = self.agent_view(source, now) else { return Vec::new() };
                let (Some(activity), Some(since)) = (agent.live_activity(), agent.reading.activity_since) else { return Vec::new() };
                self.convoy_keys_by_name
                    .get(&(session.metadata.namespace.clone(), convoy.clone()))
                    .into_iter()
                    .flatten()
                    .filter_map(|key| {
                        let item = self.effective_convoy_key(key)?;
                        let reference = self.convoy_key_ref(key)?;
                        let work = item.object.status.as_ref().and_then(|status| status.work.get(vessel));
                        (self.vessel_host(&reference, work) == session_host).then(|| AttentionFact {
                            target: reference.subresource(format!("vessels/{vessel}")),
                            activity,
                            as_of: since,
                        })
                    })
                    .collect()
            })
            .collect();
        SalienceFacts { raised, attention }
    }

    /// How long until the next home with sessions here stops being current: when its readings
    /// show as unobservable. A home heard from again before then moves the wake along.
    fn next_home_stale_delay(&self, now: chrono::DateTime<chrono::Utc>) -> Option<std::time::Duration> {
        let homes: HashSet<&flotilla_protocol::NodeId> =
            self.agent_sessions.keys().map(|key| &key.home).filter(|home| Some(*home) != self.agent_session_home.as_ref()).collect();
        homes
            .into_iter()
            .filter_map(|home| self.agent_session_heard.get(home))
            .map(|heard| agent_activity::home_stale_at(*heard))
            .filter(|stale| *stale > now)
            .min()
            .map(|stale| (stale - now).to_std().unwrap_or_default())
    }

    /// How long until the next open hold escalates.
    fn next_escalation_delay(&self, now: chrono::DateTime<chrono::Utc>) -> Option<std::time::Duration> {
        self.attentions
            .iter()
            .filter(|open| open.is_hold())
            .map(|open| open.raised_at + HOLD_ESCALATES_AFTER)
            .filter(|at| *at > now)
            .min()
            .map(|at| (at - now).to_std().unwrap_or_default())
    }

    /// The record salience joins an attention onto: its convoy or vessel, else its project.
    fn attention_target(&self, subject: &AttentionSubject) -> ResourceRef {
        match subject {
            AttentionSubject::Convoy { convoy } | AttentionSubject::ConvoyRole { convoy, .. } => {
                self.convoy_ref(&self.attention_namespace, convoy)
            }
            AttentionSubject::Vessel { convoy, vessel } => {
                self.convoy_ref(&self.attention_namespace, convoy).subresource(format!("vessels/{vessel}"))
            }
            AttentionSubject::StandingRole { project, .. } => {
                ResourceRef::new(api_version(Project::API_PATHS), Project::API_PATHS.kind, &self.attention_namespace, project)
            }
        }
    }

    async fn emit_awareness_result_sets(&self) {
        for query in self.state.subscribed_queries() {
            if !matches!(query, QueryId::Awareness { .. }) {
                continue;
            }
            if let Some(result_set) = self.state.result_set_for(&query).await {
                self.event_sink.emit(DaemonEvent::ResultSet(Box::new(result_set)));
            }
        }
    }

    fn convoy_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, name).on_host(self.local_host.clone())
    }

    fn read_convoy_ref(&self, convoy: &ReadResourceObject<Convoy>) -> Option<ResourceRef> {
        Some(
            ResourceRef::new(
                api_version(Convoy::API_PATHS),
                Convoy::API_PATHS.kind,
                &convoy.object.metadata.namespace,
                &convoy.object.metadata.name,
            )
            .on_host(self.read_host(&convoy.provenance)?),
        )
    }

    fn read_host(&self, provenance: &ResourceProvenance) -> Option<HostName> {
        match provenance {
            ResourceProvenance::Local => Some(self.local_host.clone()),
            ResourceProvenance::Replica { origin_root, .. } => self.origin_hosts.get(origin_root).cloned(),
        }
    }

    async fn refresh_origin_hosts(&mut self) {
        let origins = self
            .convoys
            .values()
            .map(|convoy| &convoy.provenance)
            .chain(self.sessions_by_source.values().flat_map(|sessions| sessions.values()).map(|session| &session.provenance))
            .chain(self.readiness_vessels.values().map(|source| &source.provenance))
            .filter_map(|provenance| match provenance {
                ResourceProvenance::Local => None,
                ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
            })
            .chain(
                self.engine_checkouts
                    .iter()
                    .filter(|checkout| self.engine_checkout_home.as_deref() != Some(checkout.home.as_str()))
                    .map(|checkout| flotilla_protocol::NodeId::new(checkout.home.as_str())),
            )
            .collect::<HashSet<_>>();
        self.known_origins = origins.clone();
        self.origin_hosts = match &self.attach_resolver {
            Some(resolver) => resolver.origin_host_names(&origins).await,
            None => HashMap::new(),
        };
    }

    fn terminal_session_ref(&self, session: &ReadResourceObject<TerminalSession>) -> Option<ResourceRef> {
        Some(
            ResourceRef::new(
                api_version(TerminalSession::API_PATHS),
                TerminalSession::API_PATHS.kind,
                &session.object.metadata.namespace,
                &session.object.metadata.name,
            )
            .on_host(self.read_host(&session.provenance)?),
        )
    }

    fn checkout_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(Checkout::API_PATHS), Checkout::API_PATHS.kind, namespace, name).on_host(self.local_host.clone())
    }

    fn summarize_independent(
        &self,
        session: &ReadResourceObject<TerminalSession>,
        attachable_sessions: &HashSet<SessionKey>,
    ) -> Option<IndependentRow> {
        let resource = self.terminal_session_ref(session)?;
        let host = resource.host.clone().expect("terminal session read reference is host-qualified");
        let key = session_key(session);
        let session = &session.object;
        if !is_independent_session(session) {
            return None;
        }
        let name = &session.metadata.name;
        let attach = attachable_sessions.contains(&key).then(|| name.clone());
        let repository_key = session.metadata.labels.get(REPO_KEY_LABEL).cloned().map(RepositoryKey);
        let repository = repository_key.as_ref().and_then(|key| self.repositories.get(key));
        let repository_label = repository_key.as_ref().map_or_else(
            || session.metadata.labels.get(REPO_LABEL).cloned(),
            |key| {
                repository
                    .map(|repository| repository.spec.qualified_label())
                    .or_else(|| session.metadata.labels.get(REPO_LABEL).filter(|label| *label != &key.0).cloned())
                    .or_else(|| Some(UNKNOWN_REPOSITORY_LABEL.to_string()))
            },
        );
        Some(
            IndependentRow::builder()
                .resource(resource)
                .name(name)
                .maybe_repo(repository_label.map(flotilla_protocol::RepoKey))
                .maybe_repo_fact(repository.map(|repository| flotilla_protocol::RepoKey(repository.spec.repo_fact_value())))
                .maybe_repository_key(repository_key)
                .host(host)
                .maybe_attach(attach)
                .maybe_cleat_endpoint(session.status.as_ref().and_then(|status| status.cleat_endpoint.clone()))
                .phase(SessionPhase::Running)
                .build(),
        )
    }

    /// The daemon that acts on a vessel. Joins, attach and completion use it.
    fn vessel_host(&self, convoy_ref: &ResourceRef, state: Option<&WorkState>) -> HostName {
        self.vessel_hosts(convoy_ref, state).0
    }

    /// The daemon that acts on a vessel, and the machine the vessel runs on
    /// (#4021). The acting daemon comes from the placement target's declared
    /// Host authority, never from who wrote a record. Without that Host record
    /// it is the target itself, which is right for every daemon-carrying host.
    fn vessel_hosts(&self, convoy_ref: &ResourceRef, state: Option<&WorkState>) -> (HostName, Option<HostName>) {
        let placement = state.and_then(|state| state.placement.as_ref());
        let target =
            placement.and_then(|placement| placement.fields.get("placement_decision")).and_then(|decision| decision.get("target_host"));
        let (reference, carried_by) = match target {
            Some(target) => (
                target.get("ref").and_then(serde_json::Value::as_str),
                target.get("display_name").and_then(serde_json::Value::as_str).map(HostName::new),
            ),
            None => {
                let host = placement.and_then(|placement| placement.fields.get("host")).and_then(serde_json::Value::as_str);
                (host, host.map(HostName::new))
            }
        };
        // Until the target's Host record replicates, an SSH host vessel
        // briefly reads as its own acting daemon (no cross-host direct reason);
        // the Host event rebuilds the row.
        let acting = reference
            .and_then(|reference| self.acting_daemon(reference))
            .or_else(|| carried_by.clone())
            .or_else(|| convoy_ref.host.clone())
            .unwrap_or_else(|| self.local_host.clone());
        (acting, carried_by)
    }

    /// The display name of the daemon acting on the Host `reference` names.
    fn acting_daemon(&self, reference: &str) -> Option<HostName> {
        let resolve = |reference: &str| {
            flotilla_resources::canonical_host_id(self.hosts.values(), reference).ok().flatten().and_then(|id| self.hosts.get(id.as_str()))
        };
        let acting = flotilla_resources::acting_daemon_ref(resolve(reference)?);
        Some(HostName::new(match resolve(acting.as_str()) {
            Some(host) if !host.spec.display_name.is_empty() => host.spec.display_name.clone(),
            Some(host) => host.metadata.name.clone(),
            // The owning daemon's Host has not replicated yet: show its ref
            // until that Host event arrives and rebuilds the rows.
            None => acting.as_str().to_owned(),
        }))
    }

    /// Replace the Host authority; reports whether any vessel mapping input changed.
    fn replace_hosts(&mut self, items: Vec<ReadResourceObject<flotilla_resources::Host>>) -> bool {
        let hosts = items.into_iter().map(|read| (read.object.metadata.name.clone(), read.object)).collect::<BTreeMap<_, _>>();
        let changed = hosts.len() != self.hosts.len()
            || hosts.iter().any(|(name, host)| self.hosts.get(name).is_none_or(|known| !same_host_authority(known, host)));
        self.hosts = hosts;
        changed
    }

    /// Apply one Host event; heartbeats that change no authority report false.
    fn apply_host_event(&mut self, event: ReadWatchEvent<flotilla_resources::Host>) -> bool {
        match event {
            ReadWatchEvent::Added(read) | ReadWatchEvent::Modified(read) => {
                let changed = self.hosts.get(&read.object.metadata.name).is_none_or(|known| !same_host_authority(known, &read.object));
                self.hosts.insert(read.object.metadata.name.clone(), read.object);
                changed
            }
            ReadWatchEvent::Deleted(read) => self.hosts.remove(&read.object.metadata.name).is_some(),
            ReadWatchEvent::DeletedByName { tombstone, .. } => self.hosts.remove(&tombstone.name).is_some(),
        }
    }

    fn session_running(&self, session: &ReadResourceObject<TerminalSession>) -> bool {
        session.object.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running)
    }

    fn vessel_materialize(&self, namespace: &str, convoy: &str, vessel: &str, vessel_host: &HostName) -> Option<String> {
        self.convoy_sessions(namespace, convoy)
            .filter(|session| {
                session.object.metadata.namespace == namespace
                    && session.object.metadata.labels.get(CONVOY_LABEL).is_some_and(|value| value == convoy)
                    && session.object.metadata.labels.get(VESSEL_LABEL).is_some_and(|value| value == vessel)
                    && self.read_host(&session.provenance).as_ref() == Some(vessel_host)
                    && self.attachable_sessions.contains(&session_key(session))
                    // Last: behind `[primitives]` it is an engine read.
                    && self.session_running(session)
            })
            .min_by_key(|session| &session.object.metadata.name)
            .map(|session| session.object.metadata.name.clone())
    }

    fn vessel_cleat_endpoint(
        &self,
        namespace: &str,
        convoy: &str,
        vessel: &str,
        vessel_host: &HostName,
    ) -> Option<flotilla_protocol::result_set::CleatEndpoint> {
        let name = self.vessel_materialize(namespace, convoy, vessel, vessel_host)?;
        self.convoy_sessions(namespace, convoy)
            .find(|session| {
                session.object.metadata.namespace == namespace
                    && session.object.metadata.name == name
                    && self.read_host(&session.provenance).as_ref() == Some(vessel_host)
            })?
            .object
            .status
            .as_ref()?
            .cleat_endpoint
            .clone()
    }

    /// `read` as its row summarizes it: with the engine's promises, except for a launch the
    /// convoy views could not read, which has nothing more to read.
    fn viewed(&self, read: &ReadResourceObject<Convoy>) -> Result<ResourceObject<Convoy>, String> {
        if unreadable_reason(&read.object).is_some() {
            return Ok(read.object.clone());
        }
        flotilla_resources::convoy::engine::viewed_read(&self.convoy_engine, read)
    }

    fn summarize(&self, resource: &ResourceRef, convoy: &ResourceObject<Convoy>) -> ConvoyRow {
        #[cfg(test)]
        self.summarized_convoys.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(reason) = unreadable_reason(convoy) {
            return ConvoyRow::builder()
                .resource(resource.clone())
                .name(&convoy.metadata.name)
                .unreadable(reason.clone())
                .generation(convoy.spec.generation)
                .workflow_ref(&convoy.spec.workflow_ref)
                .maybe_project_ref(convoy.spec.project_ref.clone())
                .build();
        }
        let name = if convoy.spec.role.is_empty() { &convoy.metadata.name } else { &convoy.spec.role };
        let status = convoy.status.as_ref();
        let subject_conflicts = subject_relationship_conflicts(convoy);
        let project =
            convoy.spec.project_ref.as_ref().and_then(|name| self.projects.get(&(convoy.metadata.namespace.clone(), name.clone())));
        let reference_context = flotilla_resources::convoy_reference_context(
            &convoy.spec.repositories,
            convoy.spec.project_ref.as_deref(),
            project.map(|project| &project.spec),
            &[],
            |key| self.repositories.get(key).map(|record| &record.spec),
        );
        let mut subjects = convoy_subject_rows(convoy, &reference_context);
        for entry in &mut subjects {
            entry.repository_key = self.convoy_subject_repository_key(convoy, &entry.subject);
        }
        let phase = status.map(|status| status.phase).unwrap_or_default();
        let checkouts = self.held_checkouts(convoy);
        let mut vessels: Vec<VesselRow> = status
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .vessels
                    .iter()
                    .map(|definition| {
                        self.summarize_vessel(
                            resource,
                            definition,
                            status.and_then(|status| status.work.get(&definition.name)),
                            status.and_then(|status| status.stalled.as_ref()),
                            status.and_then(|status| status.crew_work.get(&definition.name)),
                            &checkouts,
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Terminal convoys owe no further crew work, including before teardown
        // removes their last session observations from this projection.
        if phase.is_terminal() {
            for vessel in &mut vessels {
                vessel.surface_state = SurfaceState::Available;
            }
        }
        // #3455: a held message batch for one of this convoy's crews names
        // itself, why it is held and the command that clears it.
        let attentions: Vec<&OpenAttention> = self.convoy_attentions(&resource.namespace, &resource.name).collect();
        // #3455: a held message batch for one of this convoy's crews names
        // itself, why it is held and the command that clears it. Several held
        // crews show one, chosen lexicographically so the row is stable.
        let delivery_hold = attentions
            .iter()
            .filter(|open| open.sender == Sender::TurnRules)
            .map(|open| open.reason.clone())
            .min()
            .filter(|_| !phase.is_terminal());
        // A standing role's drift is about the role, told on its running convoy.
        let drift = convoy
            .spec
            .project_ref
            .as_ref()
            .filter(|_| convoy.metadata.annotations.contains_key(ENSURED_FROM_ANNOTATION) && !phase.is_terminal())
            .and_then(|project| {
                let role = AttentionSubject::StandingRole { project: project.clone(), role: convoy.spec.role.clone() };
                self.attentions.iter().find(|open| open.sender == Sender::Standing && open.subject == role)
            });
        let attention_demand = attentions
            .iter()
            .copied()
            .filter(|open| open.sender != Sender::TurnRules)
            .chain(drift)
            .min_by_key(|open| (open.sender != Sender::Teardown, open.sender, &open.subject));
        // Terminal records retain delivery history for explain, but delivery can no
        // longer require a turn. Filter at projection so old stored holds need no repair.
        let delivery_status = status.filter(|_| !phase.is_terminal());
        let holds = delivery_status.map(|status| status.turn_delivery_holds()).unwrap_or_default();
        let convoy_attention = status
            .and_then(|status| {
                status.attention.as_ref().filter(|attention| !phase.is_terminal() || !status.is_turn_delivery_attention(attention))
            })
            .or_else(|| holds.first());
        let pending_supervisor_turn_count = delivery_status
            .map_or(0, |status| status.turn_deliveries.values().filter(|delivery| delivery.pending_supervisor_turn.is_some()).count());
        // Preserve unrelated convoy attention and demands even after settlement;
        // only the crew's working surface is suppressed for terminal convoys.
        let surface_state = if !subject_conflicts.is_empty()
            || delivery_hold.is_some()
            || attention_demand.is_some()
            || convoy_attention.is_some()
            || !holds.is_empty()
            || pending_supervisor_turn_count > 0
            || vessels.iter().any(|vessel| vessel.surface_state.needs_attention())
        {
            SurfaceState::NeedsYou
        } else if phase.is_terminal() {
            SurfaceState::Available
        } else if let Some(stalled) = status.and_then(|status| status.stalled.as_ref()) {
            stalled_surface_state(stalled)
        } else if vessels.iter().all(|vessel| vessel.surface_state == SurfaceState::Available) && !vessels.is_empty() {
            SurfaceState::Available
        } else {
            SurfaceState::Working
        };
        let mut promise_counts = BTreeMap::<String, BTreeMap<String, usize>>::new();
        for promise in status.into_iter().flat_map(|s| s.promises.values()).flat_map(BTreeMap::values).flatten() {
            let state = serde_json::to_value(promise.state).expect("promise state").as_str().expect("state string").to_string();
            *promise_counts.entry(format!("{}/{}", promise.vessel, promise.role)).or_default().entry(state).or_default() += 1;
        }
        ConvoyRow::builder()
            .promise_counts(promise_counts)
            .admission_blockers(
                status
                    .and_then(|status| status.stalled.as_ref())
                    .filter(|stalled| matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Controller { .. })))
                    .map(|stalled| {
                        vec![ReadinessBlocker { resource: resource.clone(), phase: "Admission".into(), reason: stalled.evidence.clone() }]
                    })
                    .unwrap_or_default(),
            )
            .resource(resource.clone())
            .maybe_address_role(convoy.metadata.labels.get(ROLE_LABEL).cloned())
            .maybe_ensured_from(convoy.metadata.annotations.get(ENSURED_FROM_ANNOTATION).cloned())
            .name(name)
            .generation(convoy.spec.generation)
            .workflow_ref(&convoy.spec.workflow_ref)
            .dispatching_principal_ref(convoy.spec.dispatching_principal_ref.clone())
            .phase(convoy_phase(phase))
            .maybe_landing_reason(
                status
                    .filter(|status| matches!(status.phase, ResourceConvoyPhase::Landing | ResourceConvoyPhase::Landed))
                    .and_then(|status| status.landing_entry.as_ref())
                    .map(|entry| format!("{}: {}", convoy_phase(phase), entry.reason())),
            )
            .maybe_placement_decision(status.and_then(|status| status.placement_decision.clone()))
            .initializing(convoy_is_initializing(status))
            .maybe_message(
                delivery_hold
                    .or_else(|| attention_demand.map(|open| open.reason.clone()))
                    .or_else(|| {
                        (!subject_conflicts.is_empty()).then(|| {
                            let details =
                                subject_conflicts.iter().map(|subject| subject.short(&reference_context)).collect::<Vec<_>>().join(", ");
                            format!("conflicting relationships for {details}")
                        })
                    })
                    .or_else(|| {
                        let pending = match pending_supervisor_turn_count {
                            0 => None,
                            1 => Some("Supervisor turn awaits terminal delivery".to_string()),
                            count => Some(format!("{count} supervisor turns await terminal delivery")),
                        };
                        match (convoy_attention, pending) {
                            (Some(attention), Some(pending)) => Some(format!("{}; {pending}", attention.reason)),
                            (Some(attention), None) => Some(attention.reason.clone()),
                            (None, pending) => pending,
                        }
                    })
                    .or_else(|| status.and_then(|status| status.message.clone())),
            )
            .maybe_disposition(status.and_then(|status| status.disposition.clone()))
            .maybe_repo(self.convoy_repo_fact(convoy).map(flotilla_protocol::RepoKey))
            .maybe_started_at(status.and_then(|status| status.started_at))
            .maybe_finished_at(status.and_then(|status| status.finished_at))
            .maybe_observed_workflow_ref(status.and_then(|status| status.observed_workflow_ref.clone()))
            .maybe_project_ref(convoy.spec.project_ref.clone())
            .issues(
                convoy
                    .spec
                    .issues
                    .iter()
                    .map(|issue| flotilla_protocol::result_set::ConvoyIssueRow {
                        reference: issue.reference.clone(),
                        title: issue.snapshot.title.clone(),
                        state: issue.snapshot.state,
                    })
                    .collect(),
            )
            .subjects(subjects)
            .vessels(vessels)
            .surface_state(surface_state)
            .build()
    }

    fn convoy_repo_fact(&self, convoy: &ResourceObject<Convoy>) -> Option<String> {
        convoy
            .spec
            .sole_repository()
            .and_then(|snapshot| self.repositories.get(&snapshot.repo_ref))
            .map(|repository| repository.spec.repo_fact_value())
    }

    fn convoy_subject_repository_key(
        &self,
        convoy: &ResourceObject<Convoy>,
        subject: &flotilla_protocol::Subject,
    ) -> Option<RepositoryKey> {
        convoy.spec.repositories.iter().find_map(|repository| {
            let context = flotilla_resources::convoy_reference_context(
                std::slice::from_ref(repository),
                convoy.spec.project_ref.as_deref(),
                None,
                &[],
                |key| self.repositories.get(key).map(|record| &record.spec),
            );
            let matches = context.repositories.iter().any(|alias| alias.source == subject.source);
            matches.then(|| repository.repo_ref.clone())
        })
    }

    /// Resolve the running vessel's execution kind from its observed fulfilment kind.
    /// The spec reference is best-effort when no observed reference is recorded.
    /// Without a recorded version, kind edits since launch cannot be detected.
    fn vessel_environment_kind(&self, convoy: &ResourceRef, name: &str, host: &HostName) -> Option<EnvironmentKind> {
        let vessel = self.convoy_vessels(&convoy.namespace, &convoy.name).find(|source| {
            source.object.metadata.namespace == convoy.namespace
                && source.object.spec.convoy_ref == convoy.name
                && source.object.spec.vessel_name == name
                && self.read_host(&source.provenance).as_ref() == Some(host)
        })?;
        let kind_ref = vessel
            .object
            .status
            .as_ref()
            .and_then(|status| status.observed_kind_ref.as_ref())
            .unwrap_or(&vessel.object.spec.fulfilment_kind_ref);
        let placed = placement_record(&self.fulfilment_kinds, &convoy.namespace, kind_ref, &vessel.provenance)?;
        let version = &placed.object.metadata.resource_version;
        let kind = match placed.object.spec.realisation {
            FulfilmentRealisation::HostDirect => EnvironmentKind::Direct,
            _ => EnvironmentKind::Provisioned,
        };
        if vessel
            .object
            .status
            .as_ref()
            .and_then(|status| status.observed_kind_version.as_ref())
            .is_some_and(|observed| observed != version)
        {
            return None;
        }
        Some(kind)
    }

    fn summarize_vessel(
        &self,
        convoy_ref: &ResourceRef,
        definition: &VesselRequirement,
        state: Option<&WorkState>,
        stalled: Option<&StalledCondition>,
        crew_work: Option<&BTreeMap<String, flotilla_resources::CrewWorkState>>,
        checkouts: &HeldCheckouts,
    ) -> VesselRow {
        let placement = state.and_then(|state| state.placement.as_ref());
        let requested_stance = placement
            .and_then(|placement| placement.fields.get("requested_stance"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let placement_decision = placement
            .and_then(|placement| placement.fields.get("placement_decision"))
            .and_then(|value| serde_json::from_value(value.clone()).ok());
        let (vessel_host, carried_by) = self.vessel_hosts(convoy_ref, state);
        let now = self.clock.now();
        let vessel_resource =
            placement.and_then(|placement| placement.fields.get("vessel_ref")).and_then(serde_json::Value::as_str).map(|name| {
                ResourceRef::new(api_version(Vessel::API_PATHS), Vessel::API_PATHS.kind, &convoy_ref.namespace, name)
                    .on_host(vessel_host.clone())
            });
        let effective_stance = placement
            .and_then(|placement| placement.fields.get("effective_stance"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let image_ref =
            placement.and_then(|placement| placement.fields.get("image_ref")).and_then(serde_json::Value::as_str).map(str::to_string);
        let local_image_id =
            placement.and_then(|placement| placement.fields.get("local_image_id")).and_then(serde_json::Value::as_str).map(str::to_string);
        let crew = definition
            .crew
            .iter()
            .map(|process| {
                let command_preview = match &process.source {
                    CrewSource::Tool { command } => command.clone(),
                    CrewSource::Agent { selector, prompt, .. } => prompt.clone().unwrap_or_else(|| selector.capability.clone()),
                };
                let session = self.convoy_sessions(&convoy_ref.namespace, &convoy_ref.name).find(|session| {
                    session.object.metadata.namespace == convoy_ref.namespace
                        && session.object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy_ref.name)
                        && session.object.metadata.labels.get(VESSEL_LABEL) == Some(&definition.name)
                        && session.object.spec.role == process.role
                        && self.read_host(&session.provenance).as_ref() == Some(&vessel_host)
                });
                let observed_crew = session.and_then(|session| session.object.status.as_ref()?.crew.as_ref());
                CrewMemberSummary {
                    session: session.map(|session| format!("{}/{}/{}", vessel_host, convoy_ref.namespace, session.object.metadata.name)),
                    adapter: observed_crew.map(|crew| crew.adapter.clone()),
                    model: observed_crew.and_then(|crew| crew.model.clone()),
                    role: process.role.clone(),
                    command_preview,
                    requested_stance: requested_stance.clone(),
                    effective_stance: effective_stance.clone(),
                    agent_session: session.and_then(|session| self.agent_view(session, now)).map(|agent| agent.row_reading()),
                }
            })
            .collect();
        let selected_sessions = self.convoy_sessions(&convoy_ref.namespace, &convoy_ref.name).cloned().collect::<Vec<_>>();
        let matching_sessions = || {
            selected_sessions.iter().filter(|session| {
                session.object.metadata.namespace == convoy_ref.namespace
                    && session.object.metadata.labels.get(CONVOY_LABEL) == Some(&convoy_ref.name)
                    && session.object.metadata.labels.get(VESSEL_LABEL) == Some(&definition.name)
                    && self.read_host(&session.provenance).as_ref() == Some(&vessel_host)
            })
        };
        let placement_limits = placement.and_then(|placement| placement.fields.get("configured_limits")).and_then(|value| {
            match serde_json::from_value::<ConfiguredResourceLimits>(value.clone()) {
                Ok(limits) => Some(limits),
                Err(error) => {
                    debug!(convoy = %convoy_ref.name, vessel = %definition.name, %error, "could not decode configured placement limits");
                    None
                }
            }
        });
        let configured_limits = match matching_sessions().find_map(|session| session.object.status.as_ref()?.configured_limits.clone()) {
            Some(limits) => Some(placement_limits.unwrap_or_default().with_overrides(limits)),
            None => placement_limits,
        };
        let vessel_subject = AttentionSubject::Vessel { convoy: convoy_ref.name.clone(), vessel: definition.name.clone() };
        let credential_attention = self
            .convoy_attentions(&convoy_ref.namespace, &convoy_ref.name)
            .find(|open| open.sender == Sender::Capabilities && open.subject == vessel_subject);
        let terminal_conditions = matching_sessions()
            .filter_map(|session| {
                let condition = session.object.status.as_ref().and_then(terminal_health::condition)?;
                if matches!(condition, flotilla_protocol::ExplainedTerminalCondition::ProcessExited { .. })
                    && state.is_some_and(|state| state.phase.is_terminal())
                {
                    return None;
                }
                Some(format!("{}: {condition}", session.object.spec.role))
            })
            .collect::<BTreeSet<_>>();
        let terminal_condition = (!terminal_conditions.is_empty()).then(|| terminal_conditions.into_iter().collect::<Vec<_>>().join("; "));
        // #3460: an idle agent that cannot take a turn needs an operator, and says why.
        let obstruction = matching_sessions()
            .filter_map(|session| {
                let status = session.object.status.as_ref().filter(|status| status.phase == TerminalSessionPhase::Running)?;
                let name = status.session_id.as_deref().unwrap_or(&session.object.metadata.name);
                Some(format!("{}: {}", session.object.spec.role, status.attention_obstruction()?.describe(name)))
            })
            // Several blocked sessions on one vessel show one, chosen
            // lexicographically (role first) so the row is stable.
            .min();
        let surface_state = if credential_attention.is_some() || obstruction.is_some() {
            SurfaceState::NeedsYou
        } else if let Some(stalled) = stalled.filter(|stalled| {
            matches!(&stalled.maker, Some(flotilla_resources::LeafMaker::Actor { vessel, .. }) if vessel == &definition.name)
                || stalled
                    .leaves
                    .iter()
                    .any(|leaf| matches!(&leaf.address, flotilla_protocol::LeafAddress::Work { work, .. } if work == &definition.name))
        }) {
            stalled_surface_state(stalled)
        } else {
            matching_sessions()
                .filter(|session| session.object.status.as_ref().is_some_and(|status| status.phase == TerminalSessionPhase::Running))
                .filter_map(|session| self.agent_view(session, now)?.live_activity())
                .map(agent_surface_state)
                .max_by_key(|state| match state {
                    SurfaceState::NeedsYou => 2,
                    SurfaceState::Working => 1,
                    SurfaceState::Available | SurfaceState::StalledHandled { .. } => 0,
                })
                .unwrap_or_default()
        };
        let (mut readiness, loss_reason) = self.vessel_readiness(convoy_ref, definition, state, crew_work, &vessel_host, checkouts);
        if readiness.state == ReadinessState::Provisioning {
            if let Some(stalled) = stalled.filter(|stalled| matches!(stalled.maker, Some(flotilla_resources::LeafMaker::Controller { .. })))
            {
                readiness.state = ReadinessState::Blocked;
                readiness.blockers.push(ReadinessBlocker {
                    resource: convoy_ref.clone(),
                    phase: "Admission".into(),
                    reason: stalled.evidence.clone(),
                });
            }
        }
        // Frozen backing loss takes precedence over credential refresh: credentials
        // cannot make this vessel usable until rehydration exists.
        let surface_state = if loss_reason.is_some() { SurfaceState::NeedsYou } else { surface_state };
        VesselRow::builder()
            .maybe_environment_kind(self.vessel_environment_kind(convoy_ref, &definition.name, &vessel_host))
            .readiness(readiness)
            .resource(convoy_ref.subresource(format!("vessels/{}", definition.name)))
            .maybe_vessel_resource(vessel_resource)
            .name(&definition.name)
            .phase(work_phase(state.map(|state| state.phase).unwrap_or(ResourceWorkPhase::Pending)))
            .maybe_placement_decision(placement_decision)
            .maybe_environment_ref(
                placement
                    .and_then(|placement| placement.fields.get("environment_ref"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            )
            .maybe_configured_limits(configured_limits)
            .crew(crew)
            .maybe_ready_at(state.and_then(|state| state.ready_at))
            .maybe_started_at(state.and_then(|state| state.started_at))
            .maybe_finished_at(state.and_then(|state| state.finished_at))
            .maybe_message(loss_reason.or_else(|| {
                credential_attention.map(|open| open.reason.clone()).or_else(|| {
                    state
                        .filter(|state| state.phase == ResourceWorkPhase::Interrupted)
                        .and_then(|state| state.message.clone())
                        .or_else(|| obstruction.clone())
                        .or_else(|| terminal_condition.clone())
                        .or_else(|| state.and_then(|state| state.message.clone()))
                })
            }))
            .maybe_requested_stance(requested_stance)
            .maybe_effective_stance(effective_stance)
            .maybe_image_ref(image_ref)
            .maybe_local_image_id(local_image_id)
            .maybe_registry_digest(
                placement
                    .and_then(|placement| placement.fields.get("registry_digest"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
            )
            .depends_on(definition.depends_on.clone())
            .host(vessel_host.clone())
            .maybe_carried_by(carried_by)
            .maybe_materialize(self.vessel_materialize(&convoy_ref.namespace, &convoy_ref.name, &definition.name, &vessel_host))
            .maybe_cleat_endpoint(self.vessel_cleat_endpoint(&convoy_ref.namespace, &convoy_ref.name, &definition.name, &vessel_host))
            .complete_work(state.is_some_and(|state| !state.phase.is_terminal()))
            .surface_state(surface_state)
            .build()
    }
}

impl Drop for Aggregator {
    fn drop(&mut self) {
        for (_, task) in self.change_request_refresh_tasks.drain() {
            task.abort();
        }
    }
}

/// `name`'s record at the vessel's origin; else, since admission can author the frozen placement
/// on a different home from the vessel, the only record of that name.
fn placement_record<'a, T: flotilla_resources::Resource>(
    records: &'a BTreeMap<RepositorySourceKey, ReadResourceObject<T>>,
    namespace: &str,
    name: &str,
    provenance: &ResourceProvenance,
) -> Option<&'a ReadResourceObject<T>> {
    records.get(&repository_source_key(namespace, name, provenance)).or_else(|| {
        let mut candidates = records
            .range((namespace.to_owned(), name.to_owned(), None)..)
            .take_while(|((held_namespace, held_name, _), _)| held_namespace == namespace && held_name == name)
            .map(|(_, record)| record);
        let candidate = candidates.next()?;
        candidates.next().is_none().then_some(candidate)
    })
}

fn repository_source_key(namespace: &str, name: &str, provenance: &ResourceProvenance) -> RepositorySourceKey {
    let origin = match provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
    };
    (namespace.to_string(), name.to_string(), origin)
}

/// Whether two Host records map placements to the same acting daemon. Covers
/// every Host field `acting_daemon` reads; extend it with any field read later.
fn same_host_authority(left: &ResourceObject<flotilla_resources::Host>, right: &ResourceObject<flotilla_resources::Host>) -> bool {
    left.spec.display_name == right.spec.display_name && left.spec.connection == right.spec.connection
}

/// `ensure`'s row, its state from `reading`; with none the state is unknown.
fn standing_role_row(ensure: &ResourceObject<ConvoyEnsure>, reading: Option<crate::ports::StandingRoleReading>) -> StandingRoleRow {
    let row = StandingRoleRow::builder()
        .resource(ResourceRef::new(
            api_version(ConvoyEnsure::API_PATHS),
            ConvoyEnsure::API_PATHS.kind,
            &ensure.metadata.namespace,
            &ensure.metadata.name,
        ))
        .project_ref(&ensure.spec.project_ref)
        .role(&ensure.spec.role)
        .maybe_presents_as(ensure.spec.presents_as.clone());
    match reading {
        Some(reading) => row
            .maybe_convoy_ref(reading.convoy_ref)
            .maybe_hold(reading.hold)
            .strikes(reading.strikes)
            .maybe_next_attempt(reading.next_attempt)
            .maybe_last_failure(reading.last_failure)
            .build(),
        None => row.state_unknown(true).build(),
    }
}

#[cfg(test)]
fn local_read_event<T: Resource>(event: WatchEvent<T>) -> ReadWatchEvent<T> {
    let local = |object| ReadResourceObject { object, provenance: ResourceProvenance::Local };
    match event {
        WatchEvent::Added(object) => ReadWatchEvent::Added(local(object)),
        WatchEvent::Modified(object) => ReadWatchEvent::Modified(local(object)),
        WatchEvent::Deleted(object) => ReadWatchEvent::Deleted(local(object)),
        WatchEvent::DeletedByName(tombstone) => ReadWatchEvent::DeletedByName { tombstone, provenance: ResourceProvenance::Local },
    }
}

fn convoy_key(convoy: &ReadResourceObject<Convoy>) -> ConvoyKey {
    let origin = match &convoy.provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
    };
    (convoy.object.metadata.namespace.clone(), convoy.object.metadata.name.clone(), origin)
}

fn tombstone_key(
    tombstone: &flotilla_resources::ResourceTombstone,
    provenance: &ResourceProvenance,
) -> (String, String, Option<flotilla_protocol::NodeId>) {
    let origin = match provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
    };
    (tombstone.namespace.clone(), tombstone.name.clone(), origin)
}

fn session_key(session: &ReadResourceObject<TerminalSession>) -> SessionKey {
    let origin = match &session.provenance {
        ResourceProvenance::Local => None,
        ResourceProvenance::Replica { origin_root, .. } => Some(origin_root.clone()),
    };
    (session.object.metadata.namespace.clone(), session.object.metadata.name.clone(), origin)
}

fn agent_surface_state(activity: flotilla_protocol::AgentActivity) -> SurfaceState {
    use flotilla_protocol::AgentActivity;
    match activity {
        AgentActivity::NeedsInput => SurfaceState::NeedsYou,
        AgentActivity::Idle | AgentActivity::Interrupted => SurfaceState::Available,
        AgentActivity::Working => SurfaceState::Working,
    }
}

fn stalled_surface_state(stalled: &StalledCondition) -> SurfaceState {
    if stalled.maker.is_none() {
        return SurfaceState::NeedsYou;
    }
    match stalled.rung {
        StallRung::Nudge => SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Nudge },
        StallRung::Supervisor => SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Supervisor },
        StallRung::Bosun => SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Bosun },
        StallRung::Governor => SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Governor },
        StallRung::Operator => SurfaceState::NeedsYou,
    }
}

fn convoy_references_repo(convoy: &ResourceObject<Convoy>, repo_identity: &RepoIdentity) -> bool {
    convoy
        .spec
        .repositories
        .iter()
        .filter_map(|repository| RepoIdentity::from_remote_url(&repository.url))
        .any(|identity| identity == *repo_identity)
}

fn is_independent_session(session: &ResourceObject<TerminalSession>) -> bool {
    !session.metadata.labels.contains_key(CONVOY_LABEL)
        && session.status.as_ref().map(|status| status.phase) == Some(TerminalSessionPhase::Running)
}

/// Why the convoy views could not read `convoy`'s launch. They list such a launch with no status
/// and the reason; a launch they read always has a status, and never carries the reason.
fn unreadable_reason(convoy: &ResourceObject<Convoy>) -> Option<&String> {
    convoy.metadata.annotations.get(CONVOY_UNREADABLE_ANNOTATION).filter(|_| convoy.status.is_none())
}

fn convoy_phase(phase: ResourceConvoyPhase) -> ConvoyPhase {
    match phase {
        ResourceConvoyPhase::Pending => ConvoyPhase::Pending,
        ResourceConvoyPhase::Active => ConvoyPhase::Active,
        ResourceConvoyPhase::Interrupted => ConvoyPhase::Interrupted,
        ResourceConvoyPhase::Landing => ConvoyPhase::Landing,
        ResourceConvoyPhase::Abandoned => ConvoyPhase::Abandoned,
        ResourceConvoyPhase::Landed => ConvoyPhase::Landed,
        ResourceConvoyPhase::Failed => ConvoyPhase::Failed,
    }
}

fn convoy_phase_is_terminal(phase: ResourceConvoyPhase) -> bool {
    matches!(phase, ResourceConvoyPhase::Landed | ResourceConvoyPhase::Failed | ResourceConvoyPhase::Abandoned)
}

fn convoy_is_initializing(status: Option<&ConvoyStatus>) -> bool {
    status.is_none_or(|status| status.workflow_snapshot.is_none() && !convoy_phase_is_terminal(status.phase))
}

fn work_phase(phase: ResourceWorkPhase) -> WorkPhase {
    match phase {
        ResourceWorkPhase::Pending => WorkPhase::Pending,
        ResourceWorkPhase::Ready => WorkPhase::Ready,
        ResourceWorkPhase::Launching => WorkPhase::Launching,
        ResourceWorkPhase::Running => WorkPhase::Running,
        ResourceWorkPhase::Stalled => WorkPhase::Stalled,
        ResourceWorkPhase::Interrupted => WorkPhase::Interrupted,
        ResourceWorkPhase::Complete => WorkPhase::Complete,
        ResourceWorkPhase::Failed => WorkPhase::Failed,
        ResourceWorkPhase::Cancelled => WorkPhase::Cancelled,
        ResourceWorkPhase::Abandoned => WorkPhase::Abandoned,
    }
}

#[cfg(test)]
mod tests {
    use flotilla_resources::VesselPhase;
    use std::{
        collections::{BTreeMap, VecDeque},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use chrono::Utc;
    use flotilla_protocol::{result_set::Rows, PrincipalRef, StandingRoleHold, Subject};
    use flotilla_resources::{
        discovered_checkouts::DiscoveredCheckout, BoundChangeRequest, ConvoyRepositorySpec, ConvoySpec, CrewSpec, DeclaredSubject,
        InputMeta, ObjectMeta, ObservedCheckoutSpec, PlacementStatus, ProjectSpec, RepositorySpec, TerminalAttention,
        TerminalAttentionSource, TerminalAttentionState, TerminalSessionSource, TerminalSessionSpec, TerminalSessionStatus,
        VesselRequirement, WorkflowSnapshot,
    };
    use flotilla_store::ResourceBackend;
    use futures::stream;
    use tokio::{sync::Mutex, time::timeout};
    use uuid::Uuid;

    use super::*;

    /// This aggregator's own home, once a test has an agent report on it.
    const OWN_HOME: &str = "local-node-id";

    /// The launch a test session's view shows: its first. Its AgentSession is named by it.
    fn test_launch(session: &str) -> String {
        format!("{session}#1")
    }

    /// The crew in TerminalSession `session` reports `activity` on its AgentSession, named by the
    /// session's [`test_launch`], at `home` (`None`: this home), as the decided store's follow
    /// delivers it.
    async fn agent_reports(
        aggregator: &mut Aggregator,
        home: Option<&str>,
        session: &str,
        activity: flotilla_protocol::AgentActivity,
        since: chrono::DateTime<Utc>,
    ) {
        let own = aggregator.agent_session_home.get_or_insert_with(|| flotilla_protocol::NodeId::new(OWN_HOME)).clone();
        let home = home.map(flotilla_protocol::NodeId::new).unwrap_or(own);
        let reading = flotilla_protocol::AgentSessionReading {
            ended: None,
            activity: Some(activity),
            activity_since: Some(since),
            activity_cause: Some("Stop".into()),
            stale: false,
        };
        aggregator
            .agent_sessions_changed(AgentSessionUpdate::Changed(vec![(
                AgentSessionKey { home, name: test_launch(session) },
                Some(reading),
            )]))
            .await;
    }

    /// This home hears from `home` at `at`, as a keepalive or entry on its change stream.
    async fn home_heard(aggregator: &mut Aggregator, home: &str, at: chrono::DateTime<Utc>) {
        aggregator.agent_sessions_changed(AgentSessionUpdate::Heard { home: flotilla_protocol::NodeId::new(home), at }).await;
    }

    #[test]
    fn agent_activity_distinguishes_available_from_needs_you() {
        use flotilla_protocol::AgentActivity;
        assert!(agent_surface_state(AgentActivity::NeedsInput).needs_attention());
        assert!(agent_surface_state(AgentActivity::Idle) == SurfaceState::Available);
        assert!(agent_surface_state(AgentActivity::Interrupted) == SurfaceState::Available);
        assert!(!agent_surface_state(AgentActivity::Idle).needs_attention());
        assert!(!agent_surface_state(AgentActivity::Working).needs_attention());
    }

    #[tokio::test]
    async fn pending_supervisor_turn_is_visible_on_the_governor_convoy() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let mut governor = convoy_with_vessel("governor").await;
        let status = governor.status.as_mut().expect("status");
        status.attention = Some(flotilla_resources::ConvoyAttention {
            source: "existing-attention".to_string(),
            reason: "Existing concern".to_string(),
            raised_at: Utc::now(),
        });
        status.turn_deliveries.insert(
            "supervision-1".to_string(),
            flotilla_resources::TurnDeliveryStatus {
                pending_supervisor_turn: Some(flotilla_resources::PendingSupervisorTurn {
                    queued_order: 0,
                    vessel: "govern".to_string(),
                    role: "governor".to_string(),
                    message: flotilla_resources::TerminalCrewMessage {
                        id: "supervision-1".to_string(),
                        text: "Supervise the stalled crew".to_string(),
                        sender: flotilla_resources::CrewMessageSender::FlotillaEscalation { from: "coder@work".to_string() },
                        delivery: flotilla_resources::CrewMessageDelivery::Queued,
                        acknowledged: Default::default(),
                        following: Vec::new(),
                    },
                }),
                ..Default::default()
            },
        );
        let mut no_prior_attention = governor.clone();
        no_prior_attention.metadata.name = "governor-no-prior-attention".to_string();
        no_prior_attention.status.as_mut().expect("status").attention = None;
        aggregator.apply_convoy_event(WatchEvent::Added(governor)).await;
        aggregator.apply_convoy_event(WatchEvent::Added(no_prior_attention)).await;
        let result = state.result_set().await;
        let row = result.rows.as_convoys().expect("convoys").iter().find(|row| row.name == "governor").expect("governor row");
        assert_eq!(row.surface_state, SurfaceState::NeedsYou);
        assert!(row.message.as_deref().is_some_and(|message| message.contains("Existing concern")));
        assert!(row.message.as_deref().is_some_and(|message| message.contains("Supervisor turn awaits terminal delivery")));
        assert!(!row.message.as_deref().is_some_and(|message| message.contains("Supervise the stalled crew")));
        let row = result
            .rows
            .as_convoys()
            .expect("convoys")
            .iter()
            .find(|row| row.name == "governor-no-prior-attention")
            .expect("governor row without attention");
        assert_eq!(row.surface_state, SurfaceState::NeedsYou);
        assert_eq!(row.message.as_deref(), Some("Supervisor turn awaits terminal delivery"));
    }

    #[tokio::test]
    async fn fleet_surface_states_keep_available_and_handled_stalls_out_of_attention() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        for name in ["governor-a", "governor-b", "governor-c", "crew", "orphan"] {
            let mut convoy = convoy_with_vessel(name).await;
            if name == "crew" || name == "orphan" {
                convoy.status.as_mut().expect("status").stalled = Some(StalledCondition {
                    leaves: Vec::new(),
                    maker: (name == "crew")
                        .then(|| flotilla_resources::LeafMaker::Actor { vessel: "implement".into(), role: "coder".into() }),
                    evidence: "idle at turn end".into(),
                    source: flotilla_resources::StallEvidenceSource::Hook,
                    cause: None,
                    began_at: Utc::now(),
                    rung: if name == "crew" { StallRung::Nudge } else { StallRung::Operator },
                    supervisor: None,
                    supervision_message: None,
                    supervision_index: None,
                    supervision_exhausted: false,
                    reason: None,
                    proposed_disposition: None,
                    nudge_history: Vec::new(),
                });
            }
            aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
            if name.starts_with("governor") {
                let mut session = session_object(&format!("terminal-{name}")).await;
                session.metadata.labels =
                    BTreeMap::from([(CONVOY_LABEL.to_string(), name.to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
                agent_reports(&mut aggregator, None, &format!("terminal-{name}"), flotilla_protocol::AgentActivity::Idle, Utc::now()).await;
                aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session)).await;
            }
        }
        let result = state.result_set().await;
        let rows = result.rows.as_convoys().expect("convoys");
        assert_eq!(rows.iter().filter(|row| row.surface_state == SurfaceState::Available).count(), 3);
        assert_eq!(
            rows.iter().find(|row| row.name == "crew").expect("crew").surface_state,
            SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Nudge }
        );
        assert_eq!(
            rows.iter().find(|row| row.name == "crew").expect("crew").vessels[0].surface_state,
            SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Nudge }
        );
        assert_eq!(rows.iter().filter(|row| row.surface_state.needs_attention()).count(), 1);
        assert_eq!(rows.iter().find(|row| row.name == "orphan").expect("orphan").surface_state, SurfaceState::NeedsYou);

        let supervised = StalledCondition {
            leaves: Vec::new(),
            maker: Some(flotilla_resources::LeafMaker::Actor { vessel: "implement".into(), role: "bosun".into() }),
            evidence: "awaiting supervisor".into(),
            source: flotilla_resources::StallEvidenceSource::Hook,
            cause: None,
            began_at: Utc::now(),
            rung: StallRung::Supervisor,
            supervisor: None,
            supervision_message: None,
            supervision_index: None,
            supervision_exhausted: false,
            reason: None,
            proposed_disposition: None,
            nudge_history: Vec::new(),
        };
        let supervisor_state = stalled_surface_state(&supervised);
        assert_eq!(supervisor_state, SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Supervisor });
        assert_eq!(supervisor_state.label(), "stalled (rung 1: supervisor)");
        assert!(!supervisor_state.needs_attention());
        let governor = StalledCondition { rung: StallRung::Governor, ..supervised.clone() };
        assert_eq!(stalled_surface_state(&governor).label(), "stalled (rung 1: governor)");
        let no_maker = StalledCondition { maker: None, rung: StallRung::Nudge, ..supervised };
        assert_eq!(stalled_surface_state(&no_maker), SurfaceState::NeedsYou);
    }

    #[test]
    fn an_open_hold_schedules_its_escalation() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state, HostName::new("local"), event_tx);
        let now = Utc::now();
        let open = |sender, subject, minutes| OpenAttention {
            sender,
            subject,
            reason: "held".into(),
            raised_at: now - chrono::Duration::minutes(minutes),
        };
        aggregator.attentions = vec![
            open(Sender::Standing, AttentionSubject::convoy("held"), 5),
            open(Sender::Standing, AttentionSubject::convoy("escalated"), 20),
            open(Sender::Teardown, AttentionSubject::convoy("refused"), 1),
            open(Sender::Standing, AttentionSubject::StandingRole { project: "p".into(), role: "r".into() }, 1),
        ];
        assert_eq!(aggregator.next_escalation_delay(now), Some(Duration::from_secs(10 * 60)));
        assert_eq!(aggregator.next_escalation_delay(now + chrono::Duration::minutes(10)), None);
    }

    #[tokio::test]
    async fn credential_refresh_attention_marks_convoy_and_vessel_until_cleared() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let attentions = Arc::new(SetAttentions::default());
        let mut aggregator =
            Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attentions(attentions.clone(), "flotilla");
        let query = QueryId::Convoys { scope: None };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query, since: None }]);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_vessel("convoy-a").await)).await;
        let credential = open_attention(
            Sender::Capabilities,
            AttentionSubject::Vessel { convoy: "convoy-a".into(), vessel: "implement".into() },
            "github-app: credential `github-app` failed; expires in 120 seconds",
        );
        attentions.set(vec![credential.clone()]);
        aggregator.refresh_attentions().await;
        let result_set = state.result_set().await;
        let convoy = &result_set.rows.as_convoys().expect("convoy rows")[0];
        assert!(convoy.surface_state.needs_attention());
        assert!(convoy.vessels[0].surface_state.needs_attention());
        assert!(convoy.vessels[0].message.as_deref().is_some_and(|message| message.contains("github-app")));

        let reclaim = open_attention(Sender::Teardown, AttentionSubject::convoy("convoy-a"), "reclaim failed");
        attentions.set(vec![credential, reclaim]);
        aggregator.refresh_attentions().await;
        let result_set = state.result_set().await;
        assert_eq!(result_set.rows.as_convoys().expect("convoy rows")[0].message.as_deref(), Some("reclaim failed"));

        attentions.set(Vec::new());
        aggregator.refresh_attentions().await;
        let result_set = state.result_set().await;
        let convoy = &result_set.rows.as_convoys().expect("convoy rows")[0];
        assert!(!convoy.surface_state.needs_attention());
        assert!(!convoy.vessels[0].surface_state.needs_attention());
    }

    // #3460: an idle crew whose composer holds a stray reply is not "working":
    // the vessel and convoy need an operator, and the vessel names the
    // condition and the exact recovery command.
    #[tokio::test]
    async fn idle_crew_with_composer_text_names_itself_in_attention() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        state
            .replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: QueryId::Convoys { scope: None }, since: None }]);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_vessel("convoy-a").await)).await;
        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        let status = session.status.as_mut().expect("running status");
        status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Unobservable,
            as_of: Utc::now(),
            source: TerminalAttentionSource::Screen,
        });
        status.screen_obstruction = Some(flotilla_resources::ScreenObstruction::ComposerText { reply: Some("6;24;9t".into()), chars: 7 });
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session.clone())).await;
        let result_set = state.result_set().await;
        let convoy = &result_set.rows.as_convoys().expect("convoy rows")[0];
        assert_eq!(convoy.surface_state, SurfaceState::NeedsYou);
        assert_eq!(convoy.vessels[0].surface_state, SurfaceState::NeedsYou);
        let message = convoy.vessels[0].message.as_deref().expect("vessel names the condition");
        assert!(message.starts_with("observer: idle, composer holds unsent text `6;24;9t`"), "{message}");
        assert!(message.contains("`cleat send-keys terminal-convoy-a-implement C-u`"), "{message}");

        // #3724: hook review also names itself, under NeedsInput rather than Unobservable.
        let status = session.status.as_mut().expect("status");
        status.attention.as_mut().expect("attention").state = TerminalAttentionState::NeedsInput;
        status.screen_obstruction = Some(flotilla_resources::ScreenObstruction::HookReview);
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Modified(session.clone())).await;
        let result_set = state.result_set().await;
        let convoy = &result_set.rows.as_convoys().expect("convoy rows")[0];
        assert_eq!(convoy.surface_state, SurfaceState::NeedsYou);
        assert_eq!(convoy.vessels[0].surface_state, SurfaceState::NeedsYou);
        assert!(convoy.vessels[0].message.as_deref().unwrap().contains("Codex hook trust review"));
        session.status.as_mut().expect("status").attention.as_mut().expect("attention").state = TerminalAttentionState::Unobservable;

        // Once the composer is clear the crew reads as working again.
        session.status.as_mut().expect("status").screen_obstruction = None;
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Modified(session)).await;
        let result_set = state.result_set().await;
        assert_eq!(result_set.rows.as_convoys().expect("convoy rows")[0].vessels[0].surface_state, SurfaceState::Working);
    }

    // #3455: an open attention about a crew's role names itself at the top of
    // the convoy's attention instead of a generic "needs you", and clears with it.
    #[tokio::test]
    async fn an_open_crew_attention_names_itself_in_convoy_attention() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let attentions = Arc::new(SetAttentions::default());
        let mut aggregator =
            Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attentions(attentions.clone(), "flotilla");
        state
            .replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: QueryId::Convoys { scope: None }, since: None }]);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_vessel("convoy-a").await)).await;
        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session.clone())).await;
        let mut other = session_object("terminal-convoy-b-implement").await;
        other.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-b".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        let description = "a turn rule for flotilla/convoy-a/implement/coder needs a person to rule on it";
        let held = |convoy: &str| {
            open_attention(Sender::TurnRules, AttentionSubject::ConvoyRole { convoy: convoy.into(), role: "coder".into() }, description)
        };
        // Another convoy's held crew does not reach this convoy.
        attentions.set(vec![held("convoy-b")]);
        aggregator.refresh_attentions().await;
        let result_set = state.result_set().await;
        assert!(!result_set.rows.as_convoys().expect("convoy rows")[0].surface_state.needs_attention());

        attentions.set(vec![held("convoy-b"), held("convoy-a")]);
        aggregator.refresh_attentions().await;
        let result_set = state.result_set().await;
        let convoy = &result_set.rows.as_convoys().expect("convoy rows")[0];
        assert_eq!(convoy.surface_state, SurfaceState::NeedsYou);
        assert_eq!(convoy.message.as_deref(), Some(description));

        attentions.set(vec![held("convoy-b")]);
        aggregator.refresh_attentions().await;
        let result_set = state.result_set().await;
        let convoy = &result_set.rows.as_convoys().expect("convoy rows")[0];
        assert!(!convoy.surface_state.needs_attention());
        assert_ne!(convoy.message.as_deref(), Some(description));
    }

    #[tokio::test]
    async fn attention_projection_is_namespace_scoped_and_tracks_idle_unsettled_sessions() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_vessel("convoy-a").await)).await;
        let query = QueryId::Convoys { scope: None };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: query.clone(), since: None }]);

        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        agent_reports(&mut aggregator, None, "terminal-convoy-a-implement", flotilla_protocol::AgentActivity::Idle, Utc::now()).await;

        let mut foreign_session = session.clone();
        foreign_session.metadata.namespace = "other".to_string();
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(foreign_session)).await;
        let result_set = state.result_set().await;
        assert!(!result_set.rows.as_convoys().expect("convoy rows")[0].surface_state.needs_attention());

        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session)).await;

        let result_set = state.result_set().await;
        let convoy = result_set.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
        assert_eq!(convoy.vessels.first().expect("vessel row").surface_state, SurfaceState::Available);
        assert_eq!(convoy.surface_state, SurfaceState::Available);

        let delta = timeout(Duration::from_secs(1), async {
            loop {
                if let DaemonEvent::ResultDelta(delta) = event_rx.recv().await.expect("aggregator event") {
                    if delta.query() == query {
                        break delta;
                    }
                }
            }
        })
        .await
        .expect("attention result delta");
        assert_eq!(delta.query(), query);
        let QueryChanges::Convoys { changed, .. } = &delta.changes else { panic!("expected convoy changes") };
        assert!(changed.iter().any(|convoy| convoy.name == "convoy-a" && convoy.surface_state == SurfaceState::Available));
    }

    #[tokio::test]
    async fn adopted_change_request_claim_does_not_raise_attention() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let mut convoy = convoy_with_vessel("adopted-pr").await;
        let subject = Subject {
            kind: flotilla_protocol::SubjectKind::ChangeRequest,
            source: flotilla_protocol::IssueSource { service: "github.com".into(), scope: "flotilla-org/flotilla".into() },
            id: "2303".into(),
        };
        convoy.spec.subjects.push(DeclaredSubject {
            subject: subject.clone(),
            relationship: flotilla_protocol::Relationship::Adopts,
            issue: None,
            change_request: None,
        });
        let status = convoy.status.as_mut().expect("status");
        status.phase = ResourceConvoyPhase::Landed;
        for source in [flotilla_resources::SubjectDiscoverySource::Branch, flotilla_resources::SubjectDiscoverySource::Claim] {
            status.discover_subject(subject.clone(), flotilla_protocol::Relationship::Produces, source, Utc::now());
        }
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        let result = state.result_set().await;
        let row = &result.rows.as_convoys().expect("convoys")[0];
        assert_ne!(row.surface_state, SurfaceState::NeedsYou);
        assert!(!row.message.as_deref().is_some_and(|message| message.contains("conflicting relationships")));
    }

    #[tokio::test]
    async fn multiple_produced_subjects_do_not_raise_attention() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let mut convoy = convoy_with_vessel("convoy-a").await;
        let now = Utc::now();
        for number in [281, 282] {
            convoy.status.as_mut().expect("status").discover_subject(
                flotilla_protocol::Subject {
                    kind: flotilla_protocol::SubjectKind::ChangeRequest,
                    source: flotilla_protocol::provider_data::IssueSource {
                        service: "github.com".into(),
                        scope: "flotilla-org/flotilla".into(),
                    },
                    id: number.to_string(),
                },
                flotilla_protocol::Relationship::Produces,
                flotilla_resources::SubjectDiscoverySource::Claim,
                now,
            );
        }
        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let result = state.result_set().await;
        let row = &result.rows.as_convoys().expect("convoys")[0];
        assert_ne!(row.surface_state, SurfaceState::NeedsYou);
        assert_eq!(row.subjects.len(), 2);

        let replacement = convoy.status.as_ref().expect("status").subjects[1].subject.clone();
        convoy.status.as_mut().expect("status").discover_subject(
            replacement,
            flotilla_protocol::Relationship::Supersedes,
            flotilla_resources::SubjectDiscoverySource::Operator,
            now,
        );
        aggregator.apply_convoy_event(WatchEvent::Modified(convoy)).await;
        let result = state.result_set().await;
        let row = &result.rows.as_convoys().expect("convoys")[0];
        assert_ne!(row.surface_state, SurfaceState::NeedsYou);
        assert_eq!(row.subjects.len(), 3);
    }

    // The TUI vessel message exposes an unreachable daemon and confirmed loss;
    // neither is hidden behind raw-resource inspection, and recovery clears it.
    #[tokio::test]
    async fn vessel_projection_surfaces_terminal_degradation_loss_and_recovery() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_vessel("health-convoy").await)).await;
        let mut session = session_object("health-session").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "health-convoy".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        session.status.as_mut().expect("status").daemon_unreachable = Some("daemon forward down: provider offline".into());
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session.clone())).await;
        let rows = state.result_set().await;
        let vessel = &rows.rows.as_convoys().expect("convoys")[0].vessels[0];
        assert!(vessel
            .message
            .as_deref()
            .is_some_and(|message| message.contains("provider unavailable") && message.contains("provider offline")));
        let status = session.status.as_mut().expect("status");
        status.phase = flotilla_resources::TerminalSessionPhase::Lost;
        status.message = Some("generation dead".into());
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Modified(session.clone())).await;
        let rows = state.result_set().await;
        let vessel = &rows.rows.as_convoys().expect("convoys")[0].vessels[0];
        assert!(vessel.message.as_deref().is_some_and(|message| message.contains("session lost") && message.contains("generation dead")));
        let status = session.status.as_mut().expect("status");
        status.phase = flotilla_resources::TerminalSessionPhase::Running;
        status.message = None;
        status.daemon_unreachable = None;
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Modified(session)).await;
        let rows = state.result_set().await;
        let vessel = &rows.rows.as_convoys().expect("convoys")[0].vessels[0];
        assert!(!vessel
            .message
            .as_deref()
            .is_some_and(|message| message.contains("provider unavailable") || message.contains("session lost")));
    }

    #[tokio::test]
    async fn vessel_needs_you_overrides_a_different_handled_stall() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let mut convoy = convoy_with_vessel("convoy-a").await;
        let status = convoy.status.as_mut().expect("convoy status");
        status
            .workflow_snapshot
            .as_mut()
            .expect("workflow")
            .vessels
            .push(VesselRequirement::builder().name("review".to_string()).crew(Vec::new()).build());
        status.work.insert("review".into(), WorkState::builder().phase(ResourceWorkPhase::Ready).build());
        status.stalled = Some(StalledCondition {
            leaves: Vec::new(),
            maker: Some(flotilla_resources::LeafMaker::Actor { vessel: "implement".into(), role: "coder".into() }),
            evidence: "idle at turn end".into(),
            source: flotilla_resources::StallEvidenceSource::Hook,
            cause: None,
            began_at: Utc::now(),
            rung: StallRung::Nudge,
            supervisor: None,
            supervision_message: None,
            supervision_index: None,
            supervision_exhausted: false,
            reason: None,
            proposed_disposition: None,
            nudge_history: Vec::new(),
        });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;

        let mut session = session_object("terminal-convoy-a-review").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "review".to_string())]);
        // An idle crew whose composer holds text needs the operator.
        let status = session.status.as_mut().expect("session status");
        status.attention = Some(TerminalAttention {
            state: TerminalAttentionState::Unobservable,
            as_of: Utc::now(),
            source: TerminalAttentionSource::Screen,
        });
        status.screen_obstruction = Some(flotilla_resources::ScreenObstruction::ComposerText { reply: None, chars: 3 });
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session)).await;

        let result = state.result_set().await;
        let convoy = &result.rows.as_convoys().expect("convoys")[0];
        assert_eq!(convoy.surface_state, SurfaceState::NeedsYou);
        assert_eq!(
            convoy.vessels.iter().find(|vessel| vessel.name == "implement").expect("maker vessel").surface_state,
            SurfaceState::StalledHandled { rung: flotilla_protocol::result_set::HandledRung::Nudge }
        );
        assert_eq!(
            convoy.vessels.iter().find(|vessel| vessel.name == "review").expect("review vessel").surface_state,
            SurfaceState::NeedsYou
        );
    }

    #[tokio::test]
    async fn replicated_session_materializes_remote_vessel_and_federates_attention() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(CountingAttachResolver::with_origin("feta-node-id", "feta"));
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("kiwi"), event_tx).with_attach_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_vessel("convoy-a").await;
        convoy.status.as_mut().expect("convoy status").work.get_mut("implement").expect("work status").placement = Some(PlacementStatus {
            fields: BTreeMap::from([
                ("host".to_string(), serde_json::json!("feta")),
                ("vessel_ref".to_string(), serde_json::json!("convoy-a-implement")),
            ]),
        });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;

        let now = Utc::now();
        let mut session = session_object("terminal-convoy-a-implement-coder").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        home_heard(&mut aggregator, "feta-node-id", now).await;
        agent_reports(
            &mut aggregator,
            Some("feta-node-id"),
            "terminal-convoy-a-implement-coder",
            flotilla_protocol::AgentActivity::NeedsInput,
            now,
        )
        .await;
        let endpoint = flotilla_protocol::result_set::CleatEndpoint {
            runtime_root: "/var/lib/flotilla/cleat".to_owned(),
            daemon: "work@3".to_owned(),
            session: "terminal-convoy-a-implement-coder".to_owned(),
        };
        session.status.as_mut().expect("running status").cleat_endpoint = Some(endpoint.clone());
        aggregator
            .replace_replica_sessions(vec![ReadResourceObject {
                object: session,
                provenance: ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new("feta-node-id"),
                    last_synced_at: now,
                },
            }])
            .await;

        settle_attach_refresh(&mut aggregator).await;
        let result = state.result_set().await;
        let convoy = result.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
        let vessel = convoy.vessels.first().expect("vessel row");
        assert_eq!(vessel.host, HostName::new("feta"));
        assert_eq!(vessel.materialize.as_deref(), Some("terminal-convoy-a-implement-coder"));
        assert_eq!(vessel.cleat_endpoint.as_ref(), Some(&endpoint));
        assert!(vessel.surface_state.needs_attention());
        assert!(convoy.surface_state.needs_attention());
    }

    #[tokio::test]
    async fn replicated_idle_agent_stays_stable_across_refreshes_and_unrelated_rebuilds() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(CountingAttachResolver::with_origin("feta-node-id", "feta"));
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("kiwi"), event_tx).with_attach_resolver(resolver);
        let mut convoy = convoy_with_vessel("convoy-a").await;
        convoy.status.as_mut().expect("convoy status").work.get_mut("implement").expect("work status").placement =
            Some(PlacementStatus { fields: BTreeMap::from([("host".to_string(), serde_json::json!("feta"))]) });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;

        let now = Utc::now();
        let mut session = session_object("terminal-convoy-a-implement-coder").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        home_heard(&mut aggregator, "feta-node-id", now).await;
        for age in [80, 40, 0] {
            // A reading's age is not its freshness: its home was heard from just now.
            agent_reports(
                &mut aggregator,
                Some("feta-node-id"),
                "terminal-convoy-a-implement-coder",
                flotilla_protocol::AgentActivity::Idle,
                now - chrono::Duration::seconds(age),
            )
            .await;
            aggregator
                .apply_replica_session_event(ReadWatchEvent::Modified(ReadResourceObject {
                    object: session.clone(),
                    provenance: ResourceProvenance::Replica {
                        origin_root: flotilla_protocol::NodeId::new("feta-node-id"),
                        last_synced_at: now,
                    },
                }))
                .await;
            let before = state.result_set().await;
            let convoy = before.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
            assert_eq!(convoy.surface_state, SurfaceState::Available, "availability dropped at observation age {age}s");
            assert_eq!(convoy.vessels[0].surface_state, SurfaceState::Available);
            aggregator.environment_changed(Some(EnvironmentChange::Changed("unrelated".into()))).await;
            let after = state.result_set().await;
            assert_eq!(after.seq, before.seq, "unrelated rebuild changed attention at observation age {age}s");
        }
    }

    // A home gone quiet: its crew's readings stop being current once, at the moment
    // HOME_HEARD_WITHIN after it was last heard, and hearing from it again restores them.
    #[tokio::test]
    async fn a_quiet_homes_agents_become_unobservable_once_at_its_deadline() {
        use flotilla_resources::Clock as _;
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(CountingAttachResolver::with_origin("feta-node-id", "feta"));
        // The daemon's clock, which the test moves: the deadline is judged against it.
        let clock = Arc::new(ManualClock(std::sync::Mutex::new(Utc::now())));
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("kiwi"), event_tx)
            .with_attach_resolver(resolver)
            .with_clock(Arc::clone(&clock) as Arc<dyn flotilla_resources::Clock>);
        let mut convoy = convoy_with_vessel("convoy-a").await;
        let status = convoy.status.as_mut().expect("convoy status");
        status.work.get_mut("implement").expect("work status").placement =
            Some(PlacementStatus { fields: BTreeMap::from([("host".to_string(), serde_json::json!("feta"))]) });
        status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0]
            .crew
            .push(CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "codex".to_string() }).build());
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        let now = clock.now();
        let heard = now - agent_activity::HOME_HEARD_WITHIN + chrono::Duration::milliseconds(200);
        home_heard(&mut aggregator, "feta-node-id", heard).await;
        let name = "terminal-convoy-a-implement-coder";
        agent_reports(&mut aggregator, Some("feta-node-id"), name, flotilla_protocol::AgentActivity::Idle, now).await;
        let mut session = session_object(name).await;
        session.spec.role = "coder".into();
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        aggregator
            .replace_replica_sessions(vec![ReadResourceObject {
                object: session,
                provenance: ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new("feta-node-id"),
                    last_synced_at: now,
                },
            }])
            .await;
        let before = state.result_set().await;
        assert_eq!(before.rows.as_convoys().expect("convoy rows")[0].surface_state, SurfaceState::Available);
        assert!(!aggregator.salience_facts_at(now).attention.is_empty());
        assert_eq!(aggregator.next_home_stale_delay(now), Some(Duration::from_millis(200)));

        clock.advance(aggregator.next_home_stale_delay(clock.now()).expect("stale wake") + Duration::from_millis(5));
        assert_eq!(aggregator.next_home_stale_delay(clock.now()), None, "a home already stale schedules no wake");
        aggregator.rebuild_local_projection().await;
        let stale = state.result_set().await;
        let convoy = &stale.rows.as_convoys().expect("convoy rows")[0];
        assert_eq!(convoy.vessels[0].surface_state, SurfaceState::Working, "an unobservable agent is not available");
        let agent = convoy.vessels[0].crew[0].agent_session.as_ref().expect("the row still shows the reading");
        assert!(agent.stale);
        assert_eq!(agent.activity, Some(flotilla_protocol::AgentActivity::Idle));
        assert!(aggregator.salience_facts_at(clock.now()).attention.is_empty());
        assert_eq!(stale.seq, before.seq + 1);
        aggregator.rebuild_local_projection().await;
        assert_eq!(state.result_set().await.seq, stale.seq, "going stale is reported once");

        // Heard from again: current, and a keepalive while current changes nothing.
        home_heard(&mut aggregator, "feta-node-id", clock.now()).await;
        let restored = state.result_set().await;
        assert_eq!(restored.rows.as_convoys().expect("convoy rows")[0].surface_state, SurfaceState::Available);
        assert!(!restored.rows.as_convoys().expect("convoy rows")[0].vessels[0].crew[0].agent_session.as_ref().expect("reading").stale);
        home_heard(&mut aggregator, "feta-node-id", clock.now()).await;
        assert_eq!(state.result_set().await.seq, restored.seq);
    }

    /// A wall clock a test sets.
    struct ManualClock(std::sync::Mutex<chrono::DateTime<Utc>>);

    impl ManualClock {
        fn advance(&self, by: Duration) {
            let mut now = self.0.lock().expect("clock");
            *now += chrono::Duration::from_std(by).expect("duration");
        }
    }

    impl flotilla_resources::Clock for ManualClock {
        fn now(&self) -> chrono::DateTime<Utc> {
            *self.0.lock().expect("clock")
        }
    }

    #[tokio::test]
    async fn same_named_local_session_does_not_make_remote_replica_attachable() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(HostSelectiveAttachResolver {
            attachable_host: HostName::new("kiwi"),
            origin_hosts: HashMap::from([(flotilla_protocol::NodeId::new("feta-node-id"), HostName::new("feta"))]),
        });
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("kiwi"), event_tx).with_attach_resolver(resolver);
        let mut convoy = convoy_with_vessel("convoy-a").await;
        convoy.status.as_mut().expect("convoy status").work.get_mut("implement").expect("work status").placement =
            Some(PlacementStatus { fields: BTreeMap::from([("host".to_string(), serde_json::json!("feta"))]) });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;

        let session_name = "terminal-convoy-a-implement-coder";
        let mut local_session = session_object(session_name).await;
        local_session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        aggregator.replace_session_source(LocalSource::Durable, vec![local_session]).await;

        let mut remote_session = session_object(session_name).await;
        remote_session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        aggregator
            .replace_replica_sessions(vec![ReadResourceObject {
                object: remote_session,
                provenance: ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new("feta-node-id"),
                    last_synced_at: Utc::now(),
                },
            }])
            .await;

        settle_attach_refresh(&mut aggregator).await;
        let result = state.result_set().await;
        let vessel = &result.rows.as_convoys().expect("convoy rows")[0].vessels[0];
        assert_eq!(vessel.host, HostName::new("feta"));
        assert_eq!(vessel.materialize, None);
    }

    #[tokio::test]
    async fn replicated_convoy_appears_origin_tagged_in_fleet_awareness() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("kiwi"), event_tx)
            .with_attach_resolver(Arc::new(CountingAttachResolver::with_origin("feta-node-id", "feta")));
        let now = Utc::now();
        aggregator
            .replace_replica_convoys(vec![ReadResourceObject {
                object: convoy_with_vessel("feta-convoy").await,
                provenance: ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new("feta-node-id"),
                    last_synced_at: now,
                },
            }])
            .await;

        let result = state
            .awareness_result_set(&None, flotilla_protocol::AwarenessGrouping::Convoy, flotilla_protocol::AwarenessLimit::default())
            .await;
        let rows = result.rows.as_awareness().expect("awareness rows");
        let convoy = rows
            .iter()
            .flat_map(|node| &node.entries)
            .find(|entry| entry.kind == flotilla_protocol::AwarenessKind::Convoy && entry.label == "feta-convoy")
            .expect("remote convoy awareness entry");
        assert_eq!(convoy.refs[0].host, Some(HostName::new("feta")));
        assert!(
            state.local_result_set().await.rows.as_convoys().expect("local convoy rows").is_empty(),
            "a replica must not be re-exported as this root's local fleet contribution"
        );
    }

    #[tokio::test]
    async fn remote_homed_idle_governor_remains_in_project_awareness() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("kiwi"), event_tx)
            .with_attach_resolver(Arc::new(CountingAttachResolver::with_origin("udder-node-id", "udder")));
        let now = Utc::now();
        let provenance = ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new("udder-node-id"), last_synced_at: now };
        let mut convoy = convoy_with_vessel("governor-andamento-01234567").await;
        convoy.spec.role = "governor".to_string();
        convoy.spec.project_ref = Some("andamento".to_string());
        aggregator.replace_replica_convoys(vec![ReadResourceObject { object: convoy, provenance: provenance.clone() }]).await;

        let mut session = session_object("terminal-governor-implement-coder").await;
        session.metadata.labels = BTreeMap::from([
            (CONVOY_LABEL.to_string(), "governor-andamento-01234567".to_string()),
            (VESSEL_LABEL.to_string(), "implement".to_string()),
        ]);
        home_heard(&mut aggregator, "udder-node-id", now).await;
        agent_reports(
            &mut aggregator,
            Some("udder-node-id"),
            "terminal-governor-implement-coder",
            flotilla_protocol::AgentActivity::Idle,
            now,
        )
        .await;
        aggregator.replace_replica_sessions(vec![ReadResourceObject { object: session, provenance }]).await;

        let facts = aggregator.salience_facts_at(now);
        assert_eq!(facts.attention.len(), 1, "replica provenance must resolve both resources to udder");
        assert_eq!(facts.attention[0].activity, flotilla_protocol::AgentActivity::Idle);
        let result = state
            .awareness_result_set(
                &Some(QueryScope::new("flotilla", "andamento")),
                flotilla_protocol::AwarenessGrouping::Project,
                Default::default(),
            )
            .await;
        let nodes = result.rows.as_awareness().expect("awareness rows");
        let node = nodes.iter().find(|node| node.scope.as_ref() == Some(&QueryScope::new("flotilla", "andamento"))).expect("project group");
        assert!(
            node.entries.iter().any(|entry| {
                entry.kind == flotilla_protocol::AwarenessKind::Convoy
                    && entry.id == "convoy/flotilla/governor-andamento-01234567"
                    && entry.label == "governor"
            }),
            "an idle standing convoy remains an awareness entry"
        );
        assert!(node.entries.iter().any(|entry| {
            entry.kind == flotilla_protocol::AwarenessKind::Vessel && entry.id == "vessel/flotilla/governor-andamento-01234567/implement"
        }));
    }

    #[tokio::test]
    async fn subscribed_fleet_awareness_emits_when_project_catalog_changes() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let query = QueryId::Awareness {
            scope: None,
            grouping: flotilla_protocol::AwarenessGrouping::Project,
            limit: flotilla_protocol::AwarenessLimit::default(),
        };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: query.clone(), since: None }]);

        aggregator.apply_project_event(local_read_event(WatchEvent::Added(project_object("widgets").await))).await;

        let result_set = timeout(Duration::from_secs(1), async {
            loop {
                let event = event_rx.recv().await.expect("aggregator event");
                if let DaemonEvent::ResultSet(result_set) = event {
                    if result_set.query() == query {
                        break result_set;
                    }
                }
            }
        })
        .await
        .expect("project catalog change should publish fleet awareness");
        assert_eq!(result_set.rows.as_awareness().expect("awareness rows")[0].label, "widgets");
    }

    #[tokio::test]
    async fn run_bootstraps_materialization_and_subscribed_fleet_awareness() {
        let durable = ResourceBackend::new(InMemoryBackend::default());
        let mut convoy = convoy_with_vessel("convoy-a").await;
        convoy.status.as_mut().expect("convoy status").work.get_mut("implement").expect("work status").placement =
            Some(PlacementStatus { fields: BTreeMap::from([("host".to_string(), serde_json::json!("feta"))]) });
        let convoy_resolver = durable.clone().using::<Convoy>("flotilla");
        let created = convoy_resolver
            .create(&InputMeta::builder().name(convoy.metadata.name.clone()).build(), &convoy.spec)
            .await
            .expect("seed local convoy");
        convoy_resolver
            .update_status(&created.metadata.name, &created.metadata.resource_version, convoy.status.as_ref().expect("convoy status"))
            .await
            .expect("seed local convoy status");

        let mut session = session_object("terminal-convoy-a-implement-coder").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        durable
            .replica_writer::<TerminalSession>(flotilla_protocol::NodeId::new("feta-node-id"), "flotilla")
            .replace(
                &ResourceList { items: vec![session], resource_version: "1".to_string(), generation: Some("feta-generation".to_string()) },
                Utc::now(),
            )
            .await
            .expect("seed remote session replica");
        let project = project_object("widgets").await;
        durable
            .clone()
            .using::<Project>("flotilla")
            .create(&InputMeta::builder().name(project.metadata.name).build(), &project.spec)
            .await
            .expect("seed project");

        let state = AggregatorProjectionState::new();
        let awareness_query = QueryId::Awareness {
            scope: None,
            grouping: flotilla_protocol::AwarenessGrouping::Project,
            limit: flotilla_protocol::AwarenessLimit::default(),
        };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: awareness_query.clone(), since: None }]);
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let attach_resolver = Arc::new(CountingAttachResolver::with_origin("feta-node-id", "feta"));
        let run_state = state.clone();
        let run = tokio::spawn(async move {
            Aggregator::new(run_state, HostName::new("kiwi"), event_tx)
                .with_attach_resolver(attach_resolver)
                .run(namespace_resolvers(&durable))
                .await
        });

        let (convoy, awareness) = timeout(Duration::from_secs(1), async {
            let mut convoy = None;
            let mut awareness = None;
            while convoy.as_ref().is_none_or(|row: &ConvoyRow| row.vessels[0].materialize.is_none()) || awareness.is_none() {
                let event = event_rx.recv().await.expect("aggregator bootstrap event");
                if let DaemonEvent::ResultDelta(delta) = &event {
                    if let Some(rows) = delta.changes.as_convoys() {
                        convoy = rows.first().cloned();
                    }
                }
                let DaemonEvent::ResultSet(result) = event else { continue };
                if result.query() == (QueryId::Convoys { scope: None }) {
                    convoy = result.rows.as_convoys().expect("convoy rows").first().cloned();
                } else if result.query() == awareness_query {
                    awareness = Some(result);
                }
            }
            (convoy.expect("local convoy"), awareness.expect("fleet awareness"))
        })
        .await
        .expect("aggregator bootstrap should publish convoy and fleet awareness result sets");
        assert_eq!(convoy.vessels[0].materialize.as_deref(), Some("terminal-convoy-a-implement-coder"));
        assert!(awareness.rows.as_awareness().expect("awareness rows").iter().any(|node| node.label == "widgets"));

        assert!(!run.is_finished(), "the resource watches remain live");
        run.abort();
        let _ = run.await;
    }

    // Deltas computed while bootstrapping are withheld, so a subscriber that
    // took its snapshot before they landed catches up from the full sets
    // published when bootstrap ends.
    #[tokio::test]
    async fn subscriber_joining_during_bootstrap_receives_every_materialized_family() {
        let durable = ResourceBackend::new(InMemoryBackend::default());
        let ensure = convoy_ensure_object("governor").await;
        durable
            .clone()
            .using::<ConvoyEnsure>("flotilla")
            .create(&InputMeta::builder().name(ensure.metadata.name).build(), &ensure.spec)
            .await
            .expect("seed standing role");
        let project = project_object("widgets").await;
        durable
            .clone()
            .using::<Project>("flotilla")
            .create(&InputMeta::builder().name(project.metadata.name).build(), &project.spec)
            .await
            .expect("seed project");

        let state = AggregatorProjectionState::new();
        let roles_query = QueryId::StandingRoles { scope: None };
        let projects_query = QueryId::ProjectRepositories { scope: None };
        let joined_roles = state.result_set_for(&roles_query).await.expect("standing roles snapshot");
        let joined_projects = state.result_set_for(&projects_query).await.expect("project repositories snapshot");
        assert!(joined_roles.rows.as_standing_roles().expect("standing role rows").is_empty());
        assert!(joined_projects.rows.as_project_repositories().expect("project rows").is_empty());

        let (event_tx, mut event_rx) = broadcast::channel(64);
        let run_state = state.clone();
        let run =
            tokio::spawn(
                async move { Aggregator::new(run_state, HostName::new("kiwi"), event_tx).run(namespace_resolvers(&durable)).await },
            );

        timeout(Duration::from_secs(1), async {
            let (mut role, mut project) = (false, false);
            while !(role && project) {
                let DaemonEvent::ResultSet(set) = event_rx.recv().await.expect("aggregator bootstrap event") else { continue };
                if let Some(rows) = set.rows.as_standing_roles() {
                    role |= rows.iter().any(|row| row.role == "governor");
                }
                if let Some(rows) = set.rows.as_project_repositories() {
                    project |= rows.iter().any(|row| row.resource.name == "widgets");
                }
            }
        })
        .await
        .expect("bootstrap end publishes the standing role and project rows the joined subscriber missed");

        run.abort();
        let _ = run.await;
    }

    fn namespace_resolvers(durable: &ResourceBackend) -> AggregatorResolvers {
        AggregatorResolvers::builder()
            .durable_convoys(durable.including_replicas::<Convoy>("flotilla"))
            .durable_convoy_ensures(durable.including_replicas::<ConvoyEnsure>("flotilla"))
            .durable_sessions(durable.including_replicas::<TerminalSession>("flotilla"))
            .durable_projects(durable.including_replicas::<Project>("flotilla"))
            .durable_repositories(durable.including_replicas::<Repository>("flotilla"))
            .durable_vessels(durable.including_replicas::<Vessel>("flotilla"))
            .durable_hosts(durable.including_replicas::<flotilla_resources::Host>("flotilla"))
            .build()
    }

    // #3045: after bootstrap the Aggregator maintains result sets from watch
    // events alone. Convoy/session changes publish deltas without durable-store
    // reads, on any task; warm subscriptions also read their resident result set.
    #[tokio::test]
    async fn steady_state_emits_deltas_without_store_reads() {
        steady_state_delta_read_contract(false).await;
    }

    // #3141 / #3120: definition watches resolve their winning source with gets.
    // Do not hide that work behind the zero-read convoy/session guard.
    #[tokio::test]
    #[should_panic(expected = "zero durable-store read budget exceeded")]
    async fn aggregator_definition_deltas_zero_reads_pending_3120() {
        steady_state_delta_read_contract(true).await;
    }

    async fn steady_state_delta_read_contract(definitions: bool) {
        use flotilla_store_testkit::ReadCountsBackendExt;
        let durable = ResourceBackend::new(InMemoryBackend::default());
        let counted_durable = durable.clone().with_read_counts();
        let convoys = durable.clone().using::<Convoy>("flotilla");
        let seed = convoy_with_vessel("convoy-a").await;
        let created = convoys.create(&InputMeta::builder().name("convoy-a".into()).build(), &seed.spec).await.expect("seed convoy");
        convoys.update_status("convoy-a", &created.metadata.resource_version, seed.status.as_ref().expect("status")).await.expect("status");

        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(64);
        let run_state = state.clone();
        let resolvers = namespace_resolvers(&counted_durable);
        let run = tokio::spawn(async move { Aggregator::new(run_state, HostName::new("kiwi"), event_tx).run(resolvers).await });
        async fn wait_for(receiver: &mut broadcast::Receiver<DaemonEvent>, name: &str) -> DaemonEvent {
            let names = |event: &DaemonEvent| match event {
                DaemonEvent::ResultSet(result) if result.query() == (QueryId::Convoys { scope: None }) => {
                    result.rows.as_convoys().map(|rows| rows.iter().map(|row| row.name.clone()).collect::<Vec<_>>())
                }
                DaemonEvent::ResultDelta(delta) => delta.changes.as_convoys().map(|rows| rows.iter().map(|row| row.name.clone()).collect()),
                _ => None,
            };
            timeout(Duration::from_secs(5), async {
                loop {
                    let event = receiver.recv().await.expect("aggregator event");
                    if names(&event).is_some_and(|names| names.iter().any(|n| n == name)) {
                        return event;
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("aggregator publishes {name}"))
        }
        wait_for(&mut event_rx, "convoy-a").await;
        let durable_bootstrap = counted_durable.read_snapshot();
        assert!(!durable_bootstrap.lists().is_empty(), "bootstrap lists each watched kind once");

        for index in 0..8 {
            let name = format!("convoy-{index}");
            let seed = convoy_with_vessel(&name).await;
            let created = convoys.create(&InputMeta::builder().name(name.clone()).build(), &seed.spec).await.expect("convoy");
            convoys.update_status(&name, &created.metadata.resource_version, seed.status.as_ref().expect("status")).await.expect("status");
            if definitions {
                durable
                    .clone()
                    .using::<Project>("flotilla")
                    .create(&InputMeta::builder().name(format!("project-{index}")).build(), &project_object("widgets").await.spec)
                    .await
                    .expect("project");
            }
            let session = session_object(&format!("terminal-{index}")).await;
            durable
                .clone()
                .using::<TerminalSession>("flotilla")
                .create(&InputMeta::builder().name(session.metadata.name.clone()).build(), &session.spec)
                .await
                .expect("session");
        }
        let emitted = wait_for(&mut event_rx, "convoy-7").await;
        assert!(matches!(emitted, DaemonEvent::ResultDelta(_)), "steady-state changes emit deltas");
        convoys.delete("convoy-0").await.expect("delete convoy");
        timeout(Duration::from_secs(5), async {
            loop {
                if let DaemonEvent::ResultDelta(delta) = event_rx.recv().await.expect("deletion delta") {
                    if delta.changes.removed_resources().is_some_and(|removed| removed.iter().any(|resource| resource.name == "convoy-0")) {
                        break;
                    }
                }
            }
        })
        .await
        .expect("convoy removal delivered");
        let query = QueryId::Convoys { scope: None };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: query.clone(), since: None }]);
        let warm = state.result_set_for(&query).await.expect("warm subscription snapshot");
        assert!(warm.rows.as_convoys().expect("convoy rows").iter().any(|row| row.name == "convoy-7"));
        let durable_reads = counted_durable.read_snapshot().since(&durable_bootstrap);
        // Preserve the existing no-list regression guard even while definition
        // gets keep their separate zero-budget guard expected to fail.
        durable_reads.assert_no_lists("aggregator durable watches");
        durable_reads.assert_zero("aggregator durable delta emission");
        assert!(!run.is_finished(), "the resource watches remain live");
        run.abort();
        let _ = run.await;
    }

    #[tokio::test]
    async fn project_repository_query_follows_definition_without_activity() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let query = QueryId::ProjectRepositories { scope: None };
        assert!(state.result_set_for(&query).await.expect("query").rows.is_empty());

        let repository = repository_object("https://github.com/flotilla-org/remote-only").await;
        let repository_key = repository.spec.key();
        let expected_slug = repository.spec.catalog_slug();
        let mut project = project_object("widgets").await;
        project.spec.repositories = vec![
            flotilla_resources::ProjectRepositorySpec::builder().repo(repository_key).subpath("src".to_owned()).build(),
            flotilla_resources::ProjectRepositorySpec::builder().repo(RepositoryKey("repo-b".into())).build(),
        ];
        aggregator.apply_project_event(local_read_event(WatchEvent::Added(project.clone()))).await;
        let rows = state.result_set_for(&query).await.expect("query").rows.as_project_repositories().expect("rows").to_vec();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].repositories.len(), 2);
        assert_eq!(rows[0].repositories[0].subpath.as_deref(), Some("src"));
        assert_eq!(rows[0].repositories[0].slug, None, "membership exists before repository definition");

        aggregator.apply_repository_event(WatchEvent::Added(repository.clone())).await;
        let rows = state.result_set_for(&query).await.expect("query").rows.as_project_repositories().expect("rows").to_vec();
        assert_eq!(rows[0].repositories[0].slug.as_deref(), Some(expected_slug.as_str()));

        aggregator.apply_repository_event(WatchEvent::Deleted(repository)).await;
        let rows = state.result_set_for(&query).await.expect("query").rows.as_project_repositories().expect("rows").to_vec();
        assert_eq!(rows[0].repositories[0].slug, None, "slug retracts when repository definition disappears");

        project.spec.repositories.clear();
        aggregator.apply_project_event(local_read_event(WatchEvent::Modified(project.clone()))).await;
        let rows = state.result_set_for(&query).await.expect("query").rows.as_project_repositories().expect("rows").to_vec();
        assert_eq!(rows.len(), 1, "known-empty Project retains its row");
        assert!(rows[0].repositories.is_empty());

        aggregator.apply_project_event(local_read_event(WatchEvent::Deleted(project))).await;
        assert!(state.result_set_for(&query).await.expect("query").rows.is_empty());
    }

    async fn convoy_ensure_object(role: &str) -> ResourceObject<ConvoyEnsure> {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        backend
            .using::<ConvoyEnsure>("flotilla")
            .create(
                &InputMeta::builder().name(format!("ensure-{role}")).build(),
                &flotilla_resources::ConvoyEnsureSpec::builder()
                    .project_ref("widgets".to_string())
                    .role(role.to_string())
                    .workflow_ref("govern".to_string())
                    .repositories(Vec::new())
                    .presents_as("fleet".to_string())
                    .build(),
            )
            .await
            .expect("create scripted convoy ensure")
    }

    /// Reads whatever reading a test sets, for any role.
    #[derive(Default)]
    struct ScriptedStandingRoles(std::sync::Mutex<Option<crate::ports::StandingRoleReading>>);

    #[async_trait]
    impl crate::ports::StandingRoles for ScriptedStandingRoles {
        async fn reading(&self, _: &str, project: &str, role: &str) -> Option<crate::ports::StandingRoleReading> {
            assert_eq!((project, role), ("widgets", "governor"), "the role is asked for by its project and name");
            self.0.lock().expect("scripted reading").clone()
        }
    }

    #[tokio::test]
    async fn standing_roles_show_the_reading_their_generations_give_and_unknown_until_it_reads() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let roles = Arc::new(ScriptedStandingRoles::default());
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_standing_roles(roles.clone());
        let query = QueryId::StandingRoles { scope: None };

        // Every root may hold a copy of the declaration; none carries the role's state.
        for provenance in [
            ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new("feta-node-id"), last_synced_at: Utc::now() },
            ResourceProvenance::Local,
        ] {
            let object = convoy_ensure_object("governor").await;
            aggregator.apply_convoy_ensure_event(ReadWatchEvent::Added(ReadResourceObject { object, provenance })).await;
        }
        let set = state.result_set_for(&query).await.expect("standing roles result set");
        let rows = set.rows.as_standing_roles().expect("standing role rows");
        assert_eq!(rows.len(), 1, "copies of one declaration fold onto one role");
        assert_eq!(rows[0].role, "governor");
        assert_eq!(rows[0].project_ref, "widgets");
        assert_eq!(rows[0].presents_as.as_deref(), Some("fleet"));
        assert!(rows[0].state_unknown, "a role that did not read is unknown");
        assert_eq!(rows[0].hold, None);

        // A generation's change re-reads its role.
        *roles.0.lock().expect("scripted reading") = Some(crate::ports::StandingRoleReading {
            convoy_ref: Some("convoy-a".to_string()),
            hold: Some(StandingRoleHold::RestartLimit),
            strikes: 3,
            next_attempt: None,
            last_failure: Some("ensured convoy entered a terminal failure phase".to_string()),
        });
        let mut convoy = convoy_with_vessel("convoy-a").await;
        convoy.metadata.annotations.insert(ENSURED_FROM_ANNOTATION.to_string(), "ensure-governor".to_string());
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        let convoys = state.result_set().await;
        let row = convoys.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
        assert_eq!(row.ensured_from.as_deref(), Some("ensure-governor"));
        let set = state.result_set_for(&query).await.expect("standing roles result set");
        let rows = set.rows.as_standing_roles().expect("standing role rows");
        assert!(!rows[0].state_unknown);
        assert_eq!(rows[0].convoy_ref.as_deref(), Some("convoy-a"));
        assert_eq!(rows[0].hold, Some(StandingRoleHold::RestartLimit));
        assert_eq!(rows[0].strikes, 3);
        assert_eq!(rows[0].last_failure.as_deref(), Some("ensured convoy entered a terminal failure phase"));

        for provenance in [
            ResourceProvenance::Local,
            ResourceProvenance::Replica { origin_root: flotilla_protocol::NodeId::new("feta-node-id"), last_synced_at: Utc::now() },
        ] {
            let tombstone = flotilla_resources::ResourceTombstone {
                name: "ensure-governor".to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "2".to_string(),
                annotations: BTreeMap::new(),
            };
            aggregator.apply_convoy_ensure_event(ReadWatchEvent::DeletedByName { tombstone, provenance }).await;
        }
        let set = state.result_set_for(&query).await.expect("standing roles result set");
        assert!(set.rows.is_empty(), "a removed declaration retracts its role");
    }

    #[tokio::test]
    async fn convoy_projection_carries_dispatching_principal_ref() {
        let principal = PrincipalRef::implicit_for_namespace("people");
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let mut convoy = convoy_with_vessel("convoy-a").await;
        convoy.spec.dispatching_principal_ref = principal.clone();

        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;

        let result_set = state.result_set().await;
        let row = result_set.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
        assert_eq!(row.dispatching_principal_ref, principal);
    }

    #[tokio::test]
    async fn awareness_salience_recomputes_on_agent_activity_and_open_attention_changes() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(16);
        let attentions = Arc::new(SetAttentions::default());
        let mut aggregator =
            Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attentions(attentions.clone(), "flotilla");
        let query = QueryId::Awareness {
            scope: None,
            grouping: flotilla_protocol::AwarenessGrouping::Convoy,
            limit: flotilla_protocol::AwarenessLimit::default(),
        };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: query.clone(), since: None }]);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_vessel("convoy-a").await)).await;

        let now = Utc::now();
        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        agent_reports(&mut aggregator, None, "terminal-convoy-a-implement", flotilla_protocol::AgentActivity::NeedsInput, now).await;
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session)).await;
        assert_eq!(awareness_vessel_salience(&state, &query).await, flotilla_protocol::Salience::Attention);

        let credential = open_attention(
            Sender::Capabilities,
            AttentionSubject::Vessel { convoy: "convoy-a".into(), vessel: "implement".into() },
            "github-app: refresh failed",
        );
        attentions.set(vec![credential.clone()]);
        aggregator.refresh_attentions().await;
        assert_eq!(awareness_vessel_salience(&state, &query).await, flotilla_protocol::Salience::Attention);

        let hold = OpenAttention {
            raised_at: now - HOLD_ESCALATES_AFTER,
            ..open_attention(Sender::Standing, AttentionSubject::convoy("convoy-a"), "held")
        };
        attentions.set(vec![credential, hold]);
        aggregator.refresh_attentions().await;
        assert_eq!(
            awareness_entry_salience(&state, &query, flotilla_protocol::AwarenessKind::Convoy).await,
            flotilla_protocol::Salience::Urgent
        );

        attentions.set(Vec::new());
        aggregator.refresh_attentions().await;
        assert_eq!(
            awareness_entry_salience(&state, &query, flotilla_protocol::AwarenessKind::Convoy).await,
            flotilla_protocol::Salience::None
        );
        assert_eq!(awareness_vessel_salience(&state, &query).await, flotilla_protocol::Salience::Attention);

        let working_at = now + chrono::Duration::seconds(2);
        agent_reports(&mut aggregator, None, "terminal-convoy-a-implement", flotilla_protocol::AgentActivity::Working, working_at).await;
        let (salience, as_of) = awareness_vessel_salience_and_time(&state, &query).await;
        assert_eq!(salience, flotilla_protocol::Salience::Info);
        assert_eq!(as_of, working_at);

        // A vessel's attention raises that vessel, not only its convoy.
        attentions.set(vec![open_attention(
            Sender::Capabilities,
            AttentionSubject::Vessel { convoy: "convoy-a".into(), vessel: "implement".into() },
            "github-app: refresh failed",
        )]);
        aggregator.refresh_attentions().await;
        assert_eq!(awareness_vessel_salience(&state, &query).await, flotilla_protocol::Salience::Attention);
    }

    async fn awareness_vessel_salience(state: &AggregatorProjectionState, query: &QueryId) -> flotilla_protocol::Salience {
        awareness_vessel_salience_and_time(state, query).await.0
    }

    async fn awareness_entry_salience(
        state: &AggregatorProjectionState,
        query: &QueryId,
        kind: flotilla_protocol::AwarenessKind,
    ) -> flotilla_protocol::Salience {
        let result = state.result_set_for(query).await.expect("awareness result set");
        let node = result.rows.as_awareness().expect("awareness rows").first().expect("convoy node");
        node.entries.iter().find(|entry| entry.kind == kind).expect("entry").salience
    }

    fn open_attention(sender: Sender, subject: AttentionSubject, reason: &str) -> OpenAttention {
        OpenAttention { sender, subject, reason: reason.into(), raised_at: Utc::now() }
    }

    /// The open attentions a test sets: the aggregator's reads of them, not their writers', are under test.
    #[derive(Default)]
    struct SetAttentions(std::sync::Mutex<Vec<OpenAttention>>);

    impl SetAttentions {
        fn set(&self, open: Vec<OpenAttention>) {
            *self.0.lock().expect("attentions") = open;
        }
    }

    #[async_trait]
    impl Attentions for SetAttentions {
        async fn raise(&self, _: &str, _: &flotilla_resources::attentions::Attention) -> Result<(), String> {
            Err("the test sets attentions".into())
        }
        async fn clear(&self, _: &str, _: Sender, _: &AttentionSubject) -> Result<(), String> {
            Err("the test sets attentions".into())
        }
        async fn open(&self, _: &str, from: Option<Sender>, scope: Option<&AttentionSubject>) -> Result<Vec<OpenAttention>, String> {
            Ok(self
                .0
                .lock()
                .expect("attentions")
                .iter()
                .filter(|open| from.is_none_or(|from| from == open.sender) && scope.is_none_or(|scope| open.subject.within(scope)))
                .cloned()
                .collect())
        }
        async fn clear_within(&self, _: &str, _: &AttentionSubject) -> Result<(), String> {
            Ok(())
        }
        async fn acknowledged(
            &self,
            _: &str,
            _: Sender,
            _: &AttentionSubject,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    async fn awareness_vessel_salience_and_time(
        state: &AggregatorProjectionState,
        query: &QueryId,
    ) -> (flotilla_protocol::Salience, chrono::DateTime<Utc>) {
        let result = state.result_set_for(query).await.expect("awareness result set");
        let node = result.rows.as_awareness().expect("awareness rows").first().expect("convoy node");
        let vessel = node.entries.iter().find(|entry| entry.kind == flotilla_protocol::AwarenessKind::Vessel).expect("vessel entry");
        (vessel.salience, vessel.as_of)
    }

    struct ScriptedSource<T: Resource> {
        lists: Mutex<VecDeque<ResourceList<T>>>,
        watches: Mutex<VecDeque<Result<WatchStream<T>, ResourceError>>>,
        list_calls: AtomicUsize,
        watch_calls: AtomicUsize,
    }

    impl<T: Resource> ScriptedSource<T> {
        fn new(lists: Vec<ResourceList<T>>, watches: Vec<Result<WatchStream<T>, ResourceError>>) -> Self {
            Self {
                lists: Mutex::new(lists.into()),
                watches: Mutex::new(watches.into()),
                list_calls: AtomicUsize::new(0),
                watch_calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl<T: Resource> AggregatorWatchSource<T> for ScriptedSource<T> {
        async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
            self.list_calls.fetch_add(1, Ordering::SeqCst);
            self.lists.lock().await.pop_front().ok_or_else(|| ResourceError::other("scripted list exhausted"))
        }

        async fn watch(&self, _start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
            self.watch_calls.fetch_add(1, Ordering::SeqCst);
            self.watches.lock().await.pop_front().ok_or_else(|| ResourceError::other("scripted watch exhausted"))?
        }
    }

    #[async_trait]
    impl<T: Resource> AggregatorReplicaWatchSource<T> for ScriptedSource<T> {
        async fn list_and_watch(
            &self,
        ) -> Result<(ReadResourceList<T>, BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>), ResourceError> {
            let listed = AggregatorWatchSource::list(self).await?;
            let watch = AggregatorWatchSource::watch(self, WatchStart::resuming_from(&listed)).await?;
            let items =
                listed.items.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect();
            Ok((ReadResourceList { items }, watch.map(|event| event.map(local_read_event)).boxed()))
        }
    }

    struct CountingAttachResolver {
        calls: AtomicUsize,
        origin_hosts: HashMap<flotilla_protocol::NodeId, HostName>,
    }

    struct HostSelectiveAttachResolver {
        attachable_host: HostName,
        origin_hosts: HashMap<flotilla_protocol::NodeId, HostName>,
    }

    struct FlippingAttachResolver {
        calls: AtomicUsize,
    }

    struct ToggleAttachResolver {
        live: AtomicBool,
    }

    struct ScriptedChangeRequestResolver {
        results: Mutex<VecDeque<Result<Option<flotilla_protocol::ConvoyChangeRequest>, String>>>,
        branches: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    struct BlockingChangeRequestResolver;

    type BoundChangeRequestLookup = (Vec<RepositoryKey>, String, Option<String>);

    struct RecordingBoundChangeRequestResolver {
        calls: Mutex<Vec<BoundChangeRequestLookup>>,
    }

    impl CountingAttachResolver {
        fn new() -> Self {
            Self { calls: AtomicUsize::new(0), origin_hosts: HashMap::new() }
        }

        fn with_origin(origin: &str, host: &str) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                origin_hosts: HashMap::from([(flotilla_protocol::NodeId::new(origin), HostName::new(host))]),
            }
        }
    }

    #[async_trait]
    impl ConvoyChangeRequestResolver for BlockingChangeRequestResolver {
        async fn refresh_branch(
            &self,
            _repositories: &[RepositoryKey],
            _branch: &str,
            _binding: Option<&BoundChangeRequest>,
        ) -> ConvoyBranchRefresh {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl ConvoyChangeRequestResolver for ScriptedChangeRequestResolver {
        async fn refresh_branch(
            &self,
            repositories: &[RepositoryKey],
            branch: &str,
            _binding: Option<&BoundChangeRequest>,
        ) -> ConvoyBranchRefresh {
            assert_eq!(repositories, [RepositoryKey("repo_flotilla".into())]);
            self.branches.lock().await.push(branch.to_string());
            self.calls.fetch_add(1, Ordering::SeqCst);
            let primary = self.results.lock().await.pop_front().unwrap_or(Ok(None));
            ConvoyBranchRefresh { repositories: vec![(repositories[0].clone(), primary.clone())], primary }
        }
    }

    #[async_trait]
    impl ConvoyChangeRequestResolver for RecordingBoundChangeRequestResolver {
        async fn refresh_branch(
            &self,
            repositories: &[RepositoryKey],
            branch: &str,
            binding: Option<&BoundChangeRequest>,
        ) -> ConvoyBranchRefresh {
            let binding = binding.expect("bound lookup should carry a binding");
            self.calls.lock().await.push((repositories.to_vec(), branch.to_string(), Some(binding.id.clone())));
            ConvoyBranchRefresh {
                primary: Ok(Some(ConvoyChangeRequest {
                    id: binding.id.clone(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: binding.repository_ref.clone(),
                })),
                repositories: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl AttachCapabilityResolver for CountingAttachResolver {
        async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![true; targets.len()])
        }

        async fn origin_host_names(&self, origins: &HashSet<flotilla_protocol::NodeId>) -> HashMap<flotilla_protocol::NodeId, HostName> {
            self.origin_hosts
                .iter()
                .filter(|(origin, _)| origins.contains(*origin))
                .map(|(origin, host)| (origin.clone(), host.clone()))
                .collect()
        }
    }

    #[async_trait]
    impl AttachCapabilityResolver for HostSelectiveAttachResolver {
        async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
            Ok(targets.iter().map(|(_, host)| host == &self.attachable_host).collect())
        }

        async fn origin_host_names(&self, origins: &HashSet<flotilla_protocol::NodeId>) -> HashMap<flotilla_protocol::NodeId, HostName> {
            self.origin_hosts
                .iter()
                .filter(|(origin, _)| origins.contains(*origin))
                .map(|(origin, host)| (origin.clone(), host.clone()))
                .collect()
        }
    }

    #[async_trait]
    impl AttachCapabilityResolver for FlippingAttachResolver {
        async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(vec![call != 0; targets.len()])
        }
    }

    #[async_trait]
    impl AttachCapabilityResolver for ToggleAttachResolver {
        async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
            Ok(vec![self.live.load(Ordering::SeqCst); targets.len()])
        }
    }

    fn pending_watch<T: Resource>() -> WatchStream<T> {
        WatchStream::new(None, Box::pin(stream::pending()))
    }

    fn watch_events<T: Resource>(events: Vec<WatchEvent<T>>) -> WatchStream<T> {
        WatchStream::new(None, Box::pin(stream::iter(events.into_iter().map(Ok)).chain(stream::pending())))
    }

    fn expiring_watch<T: Resource>() -> WatchStream<T> {
        WatchStream::new(
            None,
            Box::pin(stream::once(async {
                Err::<WatchEvent<T>, _>(ResourceError::WatchExpired {
                    requested_version: "1".to_string(),
                    compacted_through: Some("2".to_string()),
                })
            })),
        )
    }

    fn failing_watch<T: Resource>(message: &'static str) -> WatchStream<T> {
        WatchStream::new(None, Box::pin(stream::once(async move { Err::<WatchEvent<T>, _>(ResourceError::other(message)) })))
    }

    fn empty_list<T: Resource>() -> ResourceList<T> {
        ResourceList { items: Vec::new(), resource_version: "2".to_string(), generation: None }
    }

    async fn run_with_test_sources(
        aggregator: Aggregator,
        durable_convoys: &dyn AggregatorReplicaWatchSource<Convoy>,
    ) -> Result<(), ResourceError> {
        let durable_sessions = ScriptedSource::<TerminalSession>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_projects = ScriptedSource::<Project>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_convoy_ensures = ScriptedSource::<ConvoyEnsure>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_repositories = ScriptedSource::<Repository>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_vessels = ScriptedSource::<Vessel>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_hosts = ScriptedSource::<flotilla_resources::Host>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let sources = AggregatorSourceRefs::builder()
            .durable_convoys(durable_convoys)
            .durable_sessions(&durable_sessions)
            .durable_projects(&durable_projects)
            .durable_convoy_ensures(&durable_convoy_ensures)
            .durable_repositories(&durable_repositories)
            .durable_vessels(&durable_vessels)
            .durable_hosts(&durable_hosts)
            .build();
        aggregator.run_with_sources(sources).await
    }

    // One 30 KB replica event must copy a bounded number of bodies even with
    // hundreds of unrelated replicas. The real event and projection path runs.
    #[tokio::test]
    async fn replica_convoy_event_work_measurement() {
        let state = AggregatorProjectionState::new();
        let (tx, _) = broadcast::channel(1024);
        struct Hosts;
        #[async_trait]
        impl AttachCapabilityResolver for Hosts {
            async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
                Ok(vec![false; targets.len()])
            }
            async fn origin_host_names(
                &self,
                origins: &HashSet<flotilla_protocol::NodeId>,
            ) -> HashMap<flotilla_protocol::NodeId, HostName> {
                origins.iter().map(|origin| (origin.clone(), HostName::new("remote"))).collect()
            }
        }
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx).with_attach_resolver_arc(Arc::new(Hosts));
        let mut records = Vec::new();
        for index in 0..320 {
            let mut object = convoy_object(&format!("remote-{index}")).await;
            object.spec.instruction = Some("x".repeat(30_000));
            object.status.get_or_insert_with(Default::default).message = Some("progress-00".into());
            records.push(ReadResourceObject {
                object,
                provenance: ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new("remote"),
                    last_synced_at: chrono::Utc::now(),
                },
            });
        }
        aggregator.replace_replica_convoys(records.clone()).await;
        // Keep a mapped origin without a host registry collaborator.
        aggregator.origin_hosts.insert(flotilla_protocol::NodeId::new("remote"), HostName::new("remote"));
        let before = aggregator.copied_convoys.load(std::sync::atomic::Ordering::Relaxed);
        let summarized = aggregator.summarized_convoys.load(std::sync::atomic::Ordering::Relaxed);
        let started = std::time::Instant::now();
        for index in 0..20 {
            let mut event = records[0].clone();
            event.object.status.as_mut().unwrap().message = Some(format!("progress-{index:02}"));
            aggregator.apply_replica_convoy_event(ReadWatchEvent::Modified(event)).await;
        }
        let result = state.result_set().await;
        let rows = result.rows.as_convoys().unwrap();
        assert_eq!(rows.len(), 320);
        assert!(rows.iter().find(|row| row.resource.name == "remote-0").unwrap().message.as_ref().unwrap().ends_with("19"));
        let copied = aggregator.copied_convoys.load(std::sync::atomic::Ordering::Relaxed) - before;
        eprintln!("replica aggregator: bodies/event={}, elapsed/event={:?}", copied / 20, started.elapsed() / 20);
        assert_eq!(copied, 0, "ordinary events share previous/current bodies");
        assert_eq!(
            aggregator.summarized_convoys.load(std::sync::atomic::Ordering::Relaxed) - summarized,
            20,
            "summarize only the event key"
        );
    }

    // Unknown roots stay buffered without repeated discovery. When discovering
    // a new root also resolves an older one, the fallback must project both;
    // subsequent progress events must return to one-key work.
    #[tokio::test]
    async fn origin_discovery_reprojects_buffered_keys_then_returns_to_keyed_updates() {
        #[derive(Default)]
        struct Hosts {
            mapping: Mutex<HashMap<flotilla_protocol::NodeId, HostName>>,
            calls: AtomicUsize,
        }
        #[async_trait]
        impl AttachCapabilityResolver for Hosts {
            async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
                Ok(vec![false; targets.len()])
            }
            async fn origin_host_names(
                &self,
                origins: &HashSet<flotilla_protocol::NodeId>,
            ) -> HashMap<flotilla_protocol::NodeId, HostName> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                self.mapping
                    .lock()
                    .await
                    .iter()
                    .filter(|(root, _)| origins.contains(*root))
                    .map(|(root, host)| (root.clone(), host.clone()))
                    .collect()
            }
        }
        let state = AggregatorProjectionState::new();
        let (tx, _) = broadcast::channel(32);
        let hosts = Arc::new(Hosts::default());
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx).with_attach_resolver_arc(hosts.clone());
        let mut first = ReadResourceObject {
            object: convoy_with_vessel("buffered").await,
            provenance: ResourceProvenance::Replica {
                origin_root: flotilla_protocol::NodeId::new("first-root"),
                last_synced_at: Utc::now(),
            },
        };
        aggregator.replace_replica_convoys(vec![first.clone()]).await;
        assert!(state.result_set().await.rows.as_convoys().unwrap().is_empty());
        let calls = hosts.calls.load(Ordering::Relaxed);
        let copied = aggregator.copied_convoys.load(Ordering::Relaxed);
        first.object.status.as_mut().unwrap().work.get_mut("implement").unwrap().phase = ResourceWorkPhase::Running;
        for _ in 0..3 {
            aggregator.apply_replica_convoy_event(ReadWatchEvent::Modified(first.clone())).await;
        }
        assert_eq!(hosts.calls.load(Ordering::Relaxed), calls, "do not rediscover the same unresolved origin per event");
        assert_eq!(aggregator.copied_convoys.load(Ordering::Relaxed), copied);
        hosts.mapping.lock().await.extend([
            (flotilla_protocol::NodeId::new("first-root"), HostName::new("first-host")),
            (flotilla_protocol::NodeId::new("second-root"), HostName::new("second-host")),
        ]);
        aggregator
            .apply_replica_convoy_event(ReadWatchEvent::Added(ReadResourceObject {
                object: convoy_object("new-origin").await,
                provenance: ResourceProvenance::Replica {
                    origin_root: flotilla_protocol::NodeId::new("second-root"),
                    last_synced_at: Utc::now(),
                },
            }))
            .await;
        let result = state.result_set().await;
        let rows = result.rows.as_convoys().unwrap();
        assert_eq!(rows.len(), 2, "discovery must also release previously buffered keys");
        assert_eq!(rows.iter().find(|row| row.resource.name == "buffered").unwrap().resource.host, Some(HostName::new("first-host")));
        assert_eq!(
            rows.iter().find(|row| row.resource.name == "buffered").unwrap().vessels[0].phase,
            WorkPhase::Running,
            "fallback must use the latest nested child state, not the initial buffered snapshot"
        );
        let untouched = rows.iter().find(|row| row.resource.name == "new-origin").unwrap().clone();
        assert!(aggregator.copied_convoys.load(Ordering::Relaxed) > copied, "the origin-change fallback ran");
        let copied = aggregator.copied_convoys.load(Ordering::Relaxed);
        let summarized = aggregator.summarized_convoys.load(Ordering::Relaxed);
        let calls = hosts.calls.load(Ordering::Relaxed);
        let mut changed = first;
        changed.object.status.as_mut().unwrap().work.get_mut("implement").unwrap().phase = ResourceWorkPhase::Failed;
        aggregator.apply_replica_convoy_event(ReadWatchEvent::Modified(changed)).await;
        assert_eq!(aggregator.copied_convoys.load(Ordering::Relaxed), copied);
        assert_eq!(aggregator.summarized_convoys.load(Ordering::Relaxed), summarized + 1);
        assert_eq!(hosts.calls.load(Ordering::Relaxed), calls);
        let result = state.result_set().await;
        assert_eq!(
            result.rows.as_convoys().unwrap().iter().find(|row| row.resource.name == "buffered").unwrap().vessels[0].phase,
            WorkPhase::Failed,
            "same-key events must refresh changed nested child state"
        );
        assert_eq!(result.rows.as_convoys().unwrap().iter().find(|row| row.resource.name == "new-origin"), Some(&untouched));
    }

    async fn convoy_object(name: &str) -> ResourceObject<Convoy> {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        backend
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &ConvoySpec::builder().workflow_ref("scratch".to_string()).build(),
            )
            .await
            .expect("create scripted convoy")
    }

    async fn project_object(name: &str) -> ResourceObject<Project> {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        backend
            .using::<Project>("flotilla")
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &ProjectSpec::builder()
                    .display_name(name.to_string())
                    .default_workflow_ref("scratch".to_string())
                    .repositories(Vec::new())
                    .build(),
            )
            .await
            .expect("create scripted project")
    }

    async fn convoy_with_branch(name: &str) -> ResourceObject<Convoy> {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        backend
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &ConvoySpec::builder()
                    .workflow_ref("scratch".to_string())
                    .r#ref("feat/convoy".to_string())
                    .repositories(vec![ConvoyRepositorySpec::builder()
                        .url("https://github.com/flotilla-org/flotilla".to_string())
                        .repo_ref(RepositoryKey("repo_flotilla".into()))
                        .source_ref("main".to_string())
                        .target_ref("main".to_string())
                        .workspace_slug("flotilla".to_string())
                        .subpaths(Vec::new())
                        .build()])
                    .build(),
            )
            .await
            .expect("create branch-backed convoy")
    }

    /// `convoy` bound to change request 2301 of its repository.
    fn bound(mut convoy: ResourceObject<Convoy>) -> ResourceObject<Convoy> {
        convoy.spec.change_request =
            Some(BoundChangeRequest { id: "2301".into(), repository_ref: RepositoryKey("repo_flotilla".into()), title: "Bound".into() });
        convoy
    }

    pub(super) async fn convoy_with_vessel(name: &str) -> ResourceObject<Convoy> {
        let backend = ResourceBackend::new(flotilla_store_memory::InMemoryBackend::default());
        let resolver = backend.using::<Convoy>("flotilla");
        let created = resolver
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &ConvoySpec::builder().workflow_ref("scratch".to_string()).build(),
            )
            .await
            .expect("create convoy with vessel");
        let status = ConvoyStatus {
            phase: ResourceConvoyPhase::Active,
            workflow_snapshot: Some(WorkflowSnapshot {
                cascade: None,
                stall_nudges: Default::default(),
                supervision: None,
                exit: None,
                turn_delivery: Default::default(),
                vessels: vec![VesselRequirement::builder().name("implement".to_string()).crew(Vec::new()).build()],
            }),
            work: BTreeMap::from([("implement".to_string(), WorkState::builder().phase(ResourceWorkPhase::Ready).build())]),
            ..Default::default()
        };
        resolver.update_status(name, &created.metadata.resource_version, &status).await.expect("set convoy vessel status")
    }

    async fn session_object(name: &str) -> ResourceObject<TerminalSession> {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let resolver = backend.using::<TerminalSession>("flotilla");
        let created = resolver
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .annotations(BTreeMap::from([(flotilla_resources::terminal_records::LAUNCH_ANNOTATION.to_string(), test_launch(name))]))
                    .build(),
                &TerminalSessionSpec::builder()
                    .env_ref("local".to_string())
                    .role("observer".to_string())
                    .source(TerminalSessionSource::Tool { command: "bash".to_string() })
                    .cwd("/repo".to_string())
                    .pool("test".to_string())
                    .build(),
            )
            .await
            .expect("create scripted terminal session");
        resolver
            .update_status(
                name,
                &created.metadata.resource_version,
                &TerminalSessionStatus { phase: TerminalSessionPhase::Running, session_id: Some(name.to_string()), ..Default::default() },
            )
            .await
            .expect("set scripted terminal session status")
    }

    #[tokio::test]
    async fn repository_catalog_reads_replicas_and_retains_other_origins_after_deletion() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let repository = repository_object("https://github.com/flotilla-org/remote-only").await;
        let key = repository.spec.key();
        let now = Utc::now();
        let writer = backend.replica_writer::<Repository>(flotilla_protocol::NodeId::new("feta-root"), "flotilla");
        writer.apply(WatchEvent::Added(repository.clone()), now).await.expect("replicate repository");
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state, HostName::new("kiwi"), event_tx);
        let mut events =
            aggregator.recover_repository_watch(&backend.including_replicas::<Repository>("flotilla")).await.expect("merge-view watch");
        assert!(aggregator.repository_labels().contains_key(&key), "replica-only repositories appear in the catalogue");

        let local = backend.using::<Repository>("flotilla");
        local.create(&InputMeta::from(&repository.metadata), &repository.spec).await.expect("local observation");
        aggregator
            .apply_repository_read_event(
                tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                    .await
                    .expect("watch event deadline")
                    .expect("added event")
                    .expect("valid event"),
            )
            .await;
        writer.apply(WatchEvent::Deleted(repository.clone()), now).await.expect("delete replica");
        aggregator
            .apply_repository_read_event(
                tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                    .await
                    .expect("watch event deadline")
                    .expect("deleted event")
                    .expect("valid event"),
            )
            .await;
        assert!(aggregator.repository_labels().contains_key(&key), "removing a replica retains the local observation");

        writer.apply(WatchEvent::Added(repository.clone()), now + chrono::Duration::seconds(1)).await.expect("replica returns");
        aggregator
            .apply_repository_read_event(
                tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                    .await
                    .expect("watch event deadline")
                    .expect("added event")
                    .expect("valid event"),
            )
            .await;
        local.delete(&repository.metadata.name).await.expect("delete local observation");
        aggregator
            .apply_repository_read_event(
                tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                    .await
                    .expect("watch event deadline")
                    .expect("deleted event")
                    .expect("valid event"),
            )
            .await;
        assert!(aggregator.repository_labels().contains_key(&key), "removing the local observation retains the replica");

        writer.apply(WatchEvent::Deleted(repository), now + chrono::Duration::seconds(2)).await.expect("delete final source");
        aggregator
            .apply_repository_read_event(
                tokio::time::timeout(std::time::Duration::from_secs(5), events.next())
                    .await
                    .expect("watch event deadline")
                    .expect("deleted event")
                    .expect("valid event"),
            )
            .await;
        assert!(!aggregator.repository_labels().contains_key(&key));
    }

    async fn repository_object(url: &str) -> ResourceObject<Repository> {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let spec = RepositorySpec::remote(url).expect("valid repository URL");
        backend
            .using::<Repository>("flotilla")
            .create(&InputMeta::builder().name(spec.key().to_string()).build(), &spec)
            .await
            .expect("create scripted repository")
    }

    /// Launch `#1` of `checkout`, adopted by the engine at `home` and ready.
    fn adopted_record(home: &str, checkout: &ResourceObject<Checkout>) -> flotilla_resources::checkout_records::CheckoutRecord {
        flotilla_resources::checkout_records::CheckoutRecord {
            name: format!("{}#1", checkout.metadata.name),
            home: home.to_owned(),
            spec: checkout.spec.clone(),
            holder: None,
            requested_at: chrono::DateTime::UNIX_EPOCH,
            reading: flotilla_protocol::LifecycleReading::Ready { identity: String::new() },
            degraded: None,
            integration: None,
        }
    }

    /// Holds `records` as the engine's adopted checkouts, `home` being this one, as a checkout
    /// change rereads them.
    async fn hold_engine_checkouts(
        aggregator: &mut Aggregator,
        home: &str,
        records: Vec<flotilla_resources::checkout_records::CheckoutRecord>,
    ) {
        aggregator.engine_checkout_home = Some(home.to_owned());
        aggregator.engine_checkouts = records;
        aggregator.refresh_origin_hosts().await;
        aggregator.rebuild_checkout_rows().await.expect("checkout rows");
    }

    /// Holds `checkouts` as this host's discovered checkouts in `flotilla`, as a refresh replaces them.
    async fn discover_checkouts(aggregator: &mut Aggregator, checkouts: &[ResourceObject<Checkout>]) {
        aggregator.discovered_namespace = "flotilla".to_owned();
        let mut found = BTreeMap::<RepositoryKey, Vec<DiscoveredCheckout>>::new();
        for checkout in checkouts {
            let CheckoutSpec::Observed(spec) = &checkout.spec else { panic!("a discovered checkout is observed") };
            found.entry(spec.repo_ref.clone()).or_default().push(DiscoveredCheckout {
                name: checkout.metadata.name.clone(),
                repo_slug: spec.repo_ref.to_string(),
                spec: spec.clone(),
            });
        }
        for (repository, found) in found {
            aggregator.discovered_checkouts.replace(&repository, found);
        }
        aggregator.discovered_checkouts_changed().await.expect("checkout rows");
    }

    async fn checkout_object(name: &str, path: &str, repo_ref: RepositoryKey) -> ResourceObject<Checkout> {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        backend
            .using::<Checkout>("flotilla")
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &CheckoutSpec::Observed(ObservedCheckoutSpec {
                    r#ref: "main".to_string(),
                    path: path.to_string(),
                    repo_ref,
                    host_ref: "local".to_string(),
                    is_main: true,
                }),
            )
            .await
            .expect("create scripted checkout")
    }

    #[tokio::test]
    async fn convoy_projection_presents_role_identity_instead_of_record_name() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(1);
        let aggregator = Aggregator::new(state, HostName::new("local"), event_tx);
        let mut convoy = convoy_with_branch("convoy-0123456789abcdef").await;
        convoy.spec.role = "governor".to_string();
        convoy.spec.generation = 3;
        convoy.spec.project_ref = Some("andamento".to_string());
        let reference = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "convoy-0123456789abcdef");

        let row = aggregator.summarize(&reference, &convoy);

        assert_eq!(row.name, "governor");
        assert_eq!(row.generation, 3);
        assert_eq!(row.project_ref.as_deref(), Some("andamento"));
        assert_eq!(row.resource.name, "convoy-0123456789abcdef");
    }

    #[tokio::test]
    async fn convoy_repo_fact_comes_from_single_repository_knowledge() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(1);
        let mut aggregator = Aggregator::new(state, HostName::new("local"), event_tx);
        let repository = repository_object("https://github.com/flotilla-org/flotilla").await;
        let repository_key = repository.spec.key();
        aggregator.apply_repository_event(WatchEvent::Added(repository)).await;
        let mut convoy = convoy_with_branch("dialect").await;
        convoy.spec.repositories[0].repo_ref = repository_key;
        let reference = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "dialect");

        let row = aggregator.summarize(&reference, &convoy);

        assert_eq!(row.repo, Some(flotilla_protocol::RepoKey("flotilla-org/flotilla".to_string())));
    }

    #[tokio::test]
    async fn checkout_for_missing_convoy_is_an_attention_condition() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(1);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let repository = repository_object("https://github.com/flotilla-org/flotilla").await;
        let checkout = checkout_object("orphan", "/work/orphan", repository.spec.key()).await;
        aggregator.apply_repository_event(WatchEvent::Added(repository)).await;

        // The engine adopted it for a convoy launch this host does not know.
        let mut record = adopted_record("local-home", &checkout);
        record.holder = Some(flotilla_resources::checkout_records::CheckoutHolder::Convoy("missing-convoy#1".to_owned()));
        aggregator.engine_namespace = "flotilla".to_owned();
        hold_engine_checkouts(&mut aggregator, "local-home", vec![record]).await;

        let result = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout result set");
        let row = &result.rows.as_checkouts().expect("checkout rows")[0];
        assert!(row.needs_attention);
        assert!(
            row.attention_reason.as_deref().is_some_and(|reason| reason.contains("orphan") && reason.contains("missing-convoy")),
            "attention must identify the checkout and missing convoy: {:?}",
            row.attention_reason
        );

        let convoy = convoy_object("missing-convoy").await;
        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;

        let result = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout result set after convoy appears");
        let row = &result.rows.as_checkouts().expect("checkout rows")[0];
        assert!(!row.needs_attention);
        assert_eq!(row.attention_reason, None);

        let mut deleting_convoy = convoy;
        deleting_convoy.metadata.deletion_timestamp = Some(Utc::now());
        aggregator.apply_convoy_event(WatchEvent::Modified(deleting_convoy)).await;

        let result = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout result set during convoy teardown");
        let row = &result.rows.as_checkouts().expect("checkout rows")[0];
        assert!(!row.needs_attention, "a checkout is not orphaned while its convoy is soft-deleting");
        assert_eq!(row.attention_reason, None);
    }

    #[tokio::test]
    async fn failed_convoy_remains_projected_until_reaping_finishes() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(1);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let mut convoy = convoy_object("failed-convoy").await;
        convoy.status = Some(ConvoyStatus {
            phase: ResourceConvoyPhase::Failed,
            message: Some("acceptance criteria not met".to_string()),
            ..Default::default()
        });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;

        convoy.metadata.deletion_timestamp = Some(Utc::now());
        aggregator.apply_convoy_event(WatchEvent::Modified(convoy.clone())).await;

        let result = state.result_set().await;
        let rows = result.rows.as_convoys().expect("convoy rows");
        assert!(
            matches!(rows, [row] if row.name == "failed-convoy" && row.phase == Some(ConvoyPhase::Failed)),
            "a terminal convoy must remain visible while its finalizers reap child resources: {rows:?}"
        );

        aggregator.apply_convoy_event(WatchEvent::Deleted(convoy)).await;

        let result = state.result_set().await;
        assert!(result.rows.as_convoys().expect("convoy rows").is_empty(), "a reaped convoy must leave the projection");
    }

    #[tokio::test]
    async fn reclaim_refusal_attention_marks_convoy_for_attention_with_reason() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let attentions = Arc::new(SetAttentions::default());
        let mut aggregator =
            Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attentions(attentions.clone(), "flotilla");
        let convoy = convoy_object("stuck-reclaim").await;
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        let awareness_query =
            QueryId::Awareness { scope: None, grouping: flotilla_protocol::AwarenessGrouping::Convoy, limit: Default::default() };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: awareness_query.clone(), since: None }]);
        let reason = "checkout checkout-orphan: Pushed=False (1 unpushed commit)";
        attentions.set(vec![open_attention(Sender::Teardown, AttentionSubject::convoy("stuck-reclaim"), reason)]);
        aggregator.refresh_attentions().await;

        timeout(Duration::from_secs(1), async {
            loop {
                let event = event_rx.recv().await.expect("aggregator event");
                if matches!(event, DaemonEvent::ResultSet(result_set) if result_set.query() == awareness_query) {
                    break;
                }
            }
        })
        .await
        .expect("reclaim refusal demand should publish fleet awareness");

        let result = state.result_set().await;
        let row = &result.rows.as_convoys().expect("convoy rows")[0];
        assert!(row.surface_state.needs_attention());
        assert_eq!(row.message.as_deref(), Some(reason));
    }

    #[tokio::test]
    async fn awareness_repository_groups_exclude_observed_checkouts() {
        // #1758 deliberately removes repository-keyed pseudo-groups created
        // solely from observed checkouts; only the real independent remains.
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(16);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let repositories = [
            ("flotilla", Some("/work/flotilla"), repository_object("https://github.com/flotilla-org/flotilla").await),
            ("flotilla-widgets", Some("/work/flotilla-widgets"), repository_object("https://github.com/flotilla-org/widgets").await),
            ("acme-widgets", Some("/work/acme-widgets"), repository_object("https://gitlab.com/acme/widgets").await),
            ("reticulate", None, repository_object("https://github.com/changedirection/reticulate").await),
        ];
        let reticulate_key = repositories[3].2.spec.key();

        for (_, _, repository) in &repositories {
            aggregator.apply_repository_event(WatchEvent::Added(repository.clone())).await;
        }
        let mut checkouts = Vec::new();
        for (name, path, repository) in repositories {
            let Some(path) = path else { continue };
            checkouts.push(checkout_object(name, path, repository.spec.key()).await);
        }
        discover_checkouts(&mut aggregator, &checkouts).await;
        let mut reticulate_session = session_object("reticulate-scratch").await;
        reticulate_session.metadata.labels = BTreeMap::from([
            (REPO_KEY_LABEL.to_string(), reticulate_key.to_string()),
            (REPO_LABEL.to_string(), reticulate_key.to_string()),
        ]);
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(reticulate_session)).await;

        let result = state.awareness_result_set(&None, flotilla_protocol::AwarenessGrouping::Project, Default::default()).await;
        let mut labels = result.rows.as_awareness().expect("awareness rows").iter().map(|node| node.label.as_str()).collect::<Vec<_>>();
        labels.sort_unstable();

        assert_eq!(labels, ["changedirection/reticulate"]);
    }

    #[tokio::test]
    async fn fleet_awareness_excludes_other_hosts_checkout_repository_groups() {
        // Other hosts' adopted checkouts' repositories are observation inputs, not real
        // Projects, so they must not create awareness tree nodes.
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(16);
        let resolver = CountingAttachResolver {
            calls: AtomicUsize::new(0),
            origin_hosts: HashMap::from([
                (flotilla_protocol::NodeId::new("github-host"), HostName::new("github-host")),
                (flotilla_protocol::NodeId::new("gitlab-host"), HostName::new("gitlab-host")),
            ]),
        };
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("receiver"), event_tx).with_attach_resolver(Arc::new(resolver));
        let mut adopted = Vec::new();
        for (host, url) in [("github-host", "https://github.com/acme/widgets"), ("gitlab-host", "https://gitlab.com/acme/widgets")] {
            let repository = repository_object(url).await;
            let key = repository.spec.key();
            aggregator.apply_repository_event(WatchEvent::Added(repository)).await;
            adopted.push(adopted_record(host, &checkout_object(host, &format!("/work/{host}"), key).await));
        }
        hold_engine_checkouts(&mut aggregator, "receiver", adopted).await;
        assert_eq!(
            state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkouts").rows.as_checkouts().expect("rows").len(),
            2
        );

        let result = state.awareness_result_set(&None, flotilla_protocol::AwarenessGrouping::Project, Default::default()).await;
        let mut labels = result.rows.as_awareness().expect("awareness rows").iter().map(|node| node.label.as_str()).collect::<Vec<_>>();
        labels.sort_unstable();

        assert!(labels.is_empty());
    }

    #[tokio::test]
    async fn independents_projection_resolves_each_target_only_once() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let resolver = Arc::new(CountingAttachResolver::new());
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attach_resolver(Arc::clone(&resolver));

        aggregator
            .replace_session_source(LocalSource::Durable, vec![session_object("session-a").await, session_object("session-b").await])
            .await;

        settle_attach_refresh(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        aggregator.rebuild_independents_projection().await;
        settle_attach_refresh(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        let result_set = state.independents_result_set(&None).await;
        let rows = result_set.rows.as_independents().expect("session rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.attach.as_deref() == Some(row.name.as_str())));
    }

    // #2775: a burst is one delivery boundary even with many presentation clients.
    #[tokio::test(start_paused = true)]
    async fn convoy_subscription_burst_load_is_coalesced() {
        let state = AggregatorProjectionState::new();
        let queries = (0..32)
            .map(|index| QueryId::Convoys { scope: Some(QueryScope::new("flotilla", format!("project-{index}"))) })
            .collect::<Vec<_>>();
        for _ in 0..2 {
            state.replace_subscriber(
                Uuid::new_v4(),
                &queries.iter().map(|query| flotilla_protocol::QueryCursor { query: query.clone(), since: None }).collect::<Vec<_>>(),
            );
        }
        let mut initial = Vec::new();
        for index in 0..512 {
            let mut convoy = convoy_with_branch(&format!("work-{index}")).await;
            convoy.spec.project_ref = Some(format!("project-{}", index % 32));
            initial.push(convoy);
        }
        let events = (0..100)
            .map(|index| {
                let mut convoy = initial[0].clone();
                convoy.status.get_or_insert_with(Default::default).message = Some(format!("progress-{index}"));
                WatchEvent::Modified(convoy)
            })
            .collect();
        let durable_convoys = ScriptedSource::new(
            vec![ResourceList { items: initial, resource_version: "1".into(), generation: None }],
            vec![Ok(watch_events(events))],
        );
        let (tx, mut rx) = broadcast::channel(1024);
        let run = run_with_test_sources(Aggregator::new(state.clone(), HostName::new("local"), tx), &durable_convoys);
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("aggregator stopped: {result:?}"),
            () = async {
                recv_query_event(&mut rx, QueryId::Convoys { scope: None }, "initial load").await;
                for query in &queries { state.result_set_for(query).await.expect("initial scoped snapshot"); }
                let delta = recv_query_event(&mut rx, QueryId::Convoys { scope: None }, "burst delivery").await;
                let DaemonEvent::ResultDelta(delta) = delta else { panic!("burst delta") };
                let QueryChanges::Convoys { changed, .. } = &delta.changes else { panic!("convoy changes") };
                assert_eq!(changed.len(), 1);
                assert_eq!(changed[0].message.as_deref(), Some("progress-99"));
                assert_eq!(delta.seq, 2, "100 updates must produce one projection rebuild");
                let scoped = recv_query_event(&mut rx, queries[0].clone(), "affected scope").await;
                let DaemonEvent::ResultSet(scoped) = scoped else { panic!("scoped snapshot") };
                assert_eq!(scoped.seq, 2);
                assert_eq!(scoped.rows.len(), 16);
            } => {}
        }
    }

    #[tokio::test]
    async fn convoy_row_insert_and_removal_update_global_and_scoped_queries_without_waiting_for_change_request_enrichment() {
        let state = AggregatorProjectionState::new();
        let scoped_query = QueryId::Convoys { scope: Some(QueryScope::new("flotilla", "roadmap")) };
        state.replace_subscriber(Uuid::new_v4(), &[flotilla_protocol::QueryCursor { query: scoped_query.clone(), since: None }]);
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let mut convoy = convoy_with_branch("convoy-a").await;
        convoy.spec.project_ref = Some("roadmap".into());
        let mut unrelated_convoy = convoy_with_branch("convoy-b").await;
        unrelated_convoy.spec.project_ref = Some("other".into());
        let mut deleting_convoy = convoy.clone();
        deleting_convoy.metadata.deletion_timestamp = Some(Utc::now());
        let durable_convoys = ScriptedSource::new(
            vec![empty_list()],
            vec![Ok(WatchStream::new(
                None,
                Box::pin(
                    stream::iter(vec![
                        WatchEvent::Added(convoy),
                        WatchEvent::Added(unrelated_convoy),
                        WatchEvent::Modified(deleting_convoy.clone()),
                        WatchEvent::Deleted(deleting_convoy),
                    ])
                    .then(|event| async move {
                        // Distinct lifecycle observations need separate delivery windows;
                        // an insert and delete inside one burst intentionally coalesce.
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        Ok(event)
                    })
                    .chain(stream::pending()),
                ),
            ))],
        );
        let run = run_with_test_sources(
            Aggregator::new(state, HostName::new("local"), event_tx).with_change_request_resolver(Arc::new(BlockingChangeRequestResolver)),
            &durable_convoys,
        );
        tokio::pin!(run);

        tokio::select! {
            result = &mut run => panic!("aggregator stopped before row lifecycle events: {result:?}"),
            () = async {
                let initial = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "initial convoy result set").await;
                let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial result set") };
                assert!(initial.rows.is_empty());

                let inserted = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "convoy insert delta").await;
                let DaemonEvent::ResultDelta(inserted) = inserted else { panic!("expected insert delta") };
                let QueryChanges::Convoys { changed, removed, .. } = &inserted.changes else { panic!("expected convoy changes") };
                assert_eq!(changed.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), vec!["convoy-a"]);
                assert!(removed.is_empty());

                let scoped = recv_query_event(&mut event_rx, scoped_query.clone(), "scoped convoy insert result set").await;
                let DaemonEvent::ResultSet(scoped) = scoped else { panic!("expected scoped result set") };
                assert_eq!(scoped.rows.as_convoys().expect("scoped convoy rows")[0].name, "convoy-a");

                let unrelated =
                    recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "unrelated convoy insert delta").await;
                let DaemonEvent::ResultDelta(unrelated) = unrelated else { panic!("expected unrelated insert delta") };
                let QueryChanges::Convoys { changed, removed, .. } = &unrelated.changes else { panic!("expected convoy changes") };
                assert_eq!(changed.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), vec!["convoy-b"]);
                assert!(removed.is_empty());

                let removed = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "convoy removal delta").await;
                let DaemonEvent::ResultDelta(removed) = removed else { panic!("expected removal delta") };
                let QueryChanges::Convoys { changed, removed, .. } = &removed.changes else { panic!("expected convoy changes") };
                assert!(changed.is_empty());
                assert_eq!(
                    removed.iter().map(|resource| resource.name.as_str()).collect::<Vec<_>>(),
                    vec!["convoy-a"]
                );

                let scoped = recv_query_event(&mut event_rx, scoped_query, "scoped convoy removal result set").await;
                let DaemonEvent::ResultSet(scoped) = scoped else { panic!("expected scoped result set") };
                assert!(scoped.rows.is_empty());
            } => {}
        }
    }

    #[tokio::test]
    async fn soft_deleting_convoy_remains_in_rows_and_salience() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let now = Utc::now();
        let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Landed).work_phase(ResourceWorkPhase::Complete).call();
        convoy.metadata.deletion_timestamp = Some(now);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;

        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        agent_reports(&mut aggregator, None, "terminal-convoy-a-implement", flotilla_protocol::AgentActivity::Idle, now).await;
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session)).await;

        let result = state.result_set().await;
        assert_eq!(result.rows.as_convoys().expect("convoy rows")[0].name, "convoy-a");
        assert_eq!(aggregator.salience_facts_at(now).attention[0].activity, flotilla_protocol::AgentActivity::Idle);
    }

    #[tokio::test]
    async fn deleted_convoy_prunes_enrichment_generation_without_reusing_stale_token() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator =
            Aggregator::new(state, HostName::new("local"), event_tx).with_change_request_resolver(Arc::new(BlockingChangeRequestResolver));
        let mut convoy = convoy_with_branch("convoy-a").await;
        let reference = aggregator.convoy_ref(&convoy.metadata.namespace, &convoy.metadata.name);

        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let stale_generation = *aggregator.change_request_refresh_generations.get(&reference).expect("active enrichment generation");

        convoy.metadata.deletion_timestamp = Some(Utc::now());
        aggregator.apply_convoy_event(WatchEvent::Modified(convoy.clone())).await;
        assert!(!aggregator.change_request_refresh_generations.contains_key(&reference));

        convoy.metadata.deletion_timestamp = None;
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        assert_ne!(aggregator.change_request_refresh_generations.get(&reference), Some(&stale_generation));

        aggregator
            .apply_change_request_resolution(ChangeRequestResolution {
                reference: reference.clone(),
                generation: stale_generation,
                branch: "feat/convoy".to_string(),
                result: ConvoyBranchRefresh {
                    primary: Ok(Some(ConvoyChangeRequest {
                        id: "stale".to_string(),
                        status: flotilla_protocol::ChangeRequestStatus::Open,
                        repository_key: RepositoryKey("repo_flotilla".into()),
                    })),
                    repositories: Vec::new(),
                },
            })
            .await;
        assert!(!aggregator.convoy_change_requests.contains_key(&reference));
    }

    #[tokio::test(start_paused = true)]
    async fn recreated_convoy_can_refresh_change_request_immediately() {
        let (event_tx, _) = broadcast::channel(4);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        let convoy = convoy_with_branch("convoy-a").await;
        let reference = aggregator.convoy_ref(&convoy.metadata.namespace, &convoy.metadata.name);

        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        aggregator.apply_convoy_event(WatchEvent::Deleted(convoy.clone())).await;
        assert!(!aggregator.change_request_refresh_started.contains_key(&reference));

        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn environment_event_refreshes_convoy_vessel_materialization_capability() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let resolver = Arc::new(FlippingAttachResolver { calls: AtomicUsize::new(0) });
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attach_resolver(resolver);
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_vessel("convoy-a").await)).await;

        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        aggregator.replace_session_source(LocalSource::Durable, vec![session]).await;

        settle_attach_refresh(&mut aggregator).await;
        let before = state.result_set().await;
        assert_eq!(before.rows.as_convoys().expect("convoy rows")[0].vessels[0].materialize, None);

        aggregator.environment_changed(Some(EnvironmentChange::Changed("local".into()))).await;

        settle_attach_refresh(&mut aggregator).await;
        let after = state.result_set().await;
        assert_eq!(after.rows.as_convoys().expect("convoy rows")[0].vessels[0].materialize.as_deref(), Some("terminal-convoy-a-implement"));
    }

    #[tokio::test]
    async fn peer_route_changes_retract_and_restore_materialization_without_restart() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let resolver = Arc::new(ToggleAttachResolver { live: AtomicBool::new(true) });
        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        let durable_convoys = ScriptedSource::new(
            vec![ResourceList { items: vec![convoy_with_vessel("convoy-a").await], resource_version: "1".to_string(), generation: None }],
            vec![Ok(pending_watch())],
        );
        let durable_sessions = ScriptedSource::new(
            vec![ResourceList { items: vec![session], resource_version: "1".to_string(), generation: None }],
            vec![Ok(pending_watch())],
        );
        let durable_projects = ScriptedSource::<Project>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_convoy_ensures = ScriptedSource::<ConvoyEnsure>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_repositories = ScriptedSource::<Repository>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_vessels = ScriptedSource::<Vessel>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_hosts = ScriptedSource::<flotilla_resources::Host>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let sources = AggregatorSourceRefs::builder()
            .durable_convoys(&durable_convoys)
            .durable_sessions(&durable_sessions)
            .durable_projects(&durable_projects)
            .durable_convoy_ensures(&durable_convoy_ensures)
            .durable_repositories(&durable_repositories)
            .durable_vessels(&durable_vessels)
            .durable_hosts(&durable_hosts)
            .build();
        let run = Aggregator::new(state, HostName::new("kiwi"), event_tx.clone())
            .with_attach_resolver(Arc::clone(&resolver))
            .run_with_sources(sources);
        tokio::pin!(run);

        tokio::select! {
            result = &mut run => panic!("aggregator stopped during route transition test: {result:?}"),
            () = async {
                // Bootstrap publishes promptly, then a capability delta supplies
                // the materialization recipe once the provider check finishes.
                timeout(Duration::from_secs(1), async {
                    loop {
                        let event = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "initial convoy enrichment").await;
                        let rows = match &event {
                            DaemonEvent::ResultSet(set) => set.rows.as_convoys().expect("convoy rows"),
                            DaemonEvent::ResultDelta(delta) => delta.changes.as_convoys().expect("convoy changes"),
                            _ => unreachable!(),
                        };
                        if rows.first().is_some_and(|row| row.vessels[0].materialize.as_deref() == Some("terminal-convoy-a-implement")) { break; }
                    }
                }).await.expect("bootstrap recipe becomes available");

                resolver.live.store(false, Ordering::SeqCst);
                let _ = event_tx.send(DaemonEvent::PeerStatusChanged {
                    node_id: flotilla_protocol::NodeId::new("feta"),
                    status: flotilla_protocol::PeerConnectionState::Disconnected,
                });
                let disconnected = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "disconnect should retract recipe").await;
                let DaemonEvent::ResultDelta(disconnected) = disconnected else { panic!("expected disconnect delta") };
                assert_eq!(disconnected.changes.as_convoys().expect("convoy changes")[0].vessels[0].materialize, None);

                resolver.live.store(true, Ordering::SeqCst);
                let _ = event_tx.send(DaemonEvent::PeerStatusChanged {
                    node_id: flotilla_protocol::NodeId::new("feta"),
                    status: flotilla_protocol::PeerConnectionState::Connected,
                });
                let reconnected = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "reconnect should restore recipe").await;
                let DaemonEvent::ResultDelta(reconnected) = reconnected else { panic!("expected reconnect delta") };
                assert_eq!(
                    reconnected.changes.as_convoys().expect("convoy changes")[0].vessels[0].materialize.as_deref(),
                    Some("terminal-convoy-a-implement")
                );
            } => {}
        }
    }

    #[tokio::test]
    async fn terminal_convoy_phase_does_not_refresh_its_change_request_reference() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Ok(Some(flotilla_protocol::ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
                Ok(Some(flotilla_protocol::ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Merged,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator =
            Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;

        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let DaemonEvent::ResultSet(initial) = event_rx.recv().await.expect("initial result set") else {
            panic!("expected initial result set");
        };
        assert!(initial.rows.as_convoys().expect("convoy rows")[0].subjects.is_empty());

        apply_next_change_request_resolution(&mut aggregator).await;
        assert!(state.result_set().await.rows.as_convoys().expect("convoy rows")[0].subjects.is_empty());

        convoy.status =
            Some(ConvoyStatus { phase: ResourceConvoyPhase::Landed, disposition: Some("shipped".to_string()), ..Default::default() });
        aggregator.apply_convoy_event(WatchEvent::Modified(convoy)).await;
        let DaemonEvent::ResultDelta(phase_delta) = event_rx.recv().await.expect("phase delta") else {
            panic!("expected result delta");
        };
        let phase_row = &phase_delta.changes.as_convoys().expect("convoy changes")[0];
        assert_eq!(phase_row.phase, Some(ConvoyPhase::Landed));
        assert_eq!(phase_row.disposition.as_deref(), Some("shipped"));
        assert!(phase_row.subjects.is_empty());

        assert!(!aggregator.change_request_refresh_tasks.contains_key(&aggregator.convoy_ref("flotilla", "convoy-a")));
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn bound_convoy_enrichment_looks_up_the_admitted_pr_by_id() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let resolver = Arc::new(RecordingBoundChangeRequestResolver { calls: Mutex::new(Vec::new()) });
        let mut aggregator =
            Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;
        convoy.spec.change_request = Some(BoundChangeRequest {
            id: "1071".to_string(),
            repository_ref: RepositoryKey("repo_flotilla".to_string()),
            title: "Existing PR".to_string(),
        });

        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        let _ = event_rx.recv().await.expect("initial result set");
        apply_next_change_request_resolution(&mut aggregator).await;
        let result_set = state.result_set().await;
        let bound = &result_set.rows.as_convoys().expect("convoy result rows")[0].subjects[0];
        assert_eq!(bound.subject.id, "1071");
        assert_eq!(bound.relationship, flotilla_protocol::Relationship::Adopts);
        assert_eq!(
            resolver.calls.lock().await.as_slice(),
            &[(vec![RepositoryKey("repo_flotilla".to_string())], "feat/convoy".to_string(), Some("1071".to_string()),)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn convoy_branch_change_clears_the_previous_change_request_before_refresh() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(4);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
                Err("forge unavailable".into()),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(state, HostName::new("local"), event_tx).with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;

        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        assert!(matches!(event_rx.recv().await.expect("initial result set"), DaemonEvent::ResultSet(_)));
        apply_next_change_request_resolution(&mut aggregator).await;

        convoy.spec.r#ref = Some("feat/rebased".into());
        aggregator.apply_convoy_event(WatchEvent::Modified(convoy)).await;
        assert!(aggregator.convoy_change_requests.is_empty());
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.branches.lock().await.as_slice(), ["feat/convoy", "feat/rebased"]);
    }

    #[tokio::test]
    async fn unchanged_provider_refresh_does_not_refresh_convoy_change_requests() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Closed,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let durable_convoys = ScriptedSource::new(
            vec![ResourceList { items: vec![convoy_with_branch("convoy-a").await], resource_version: "1".into(), generation: None }],
            vec![Ok(pending_watch())],
        );
        let run = run_with_test_sources(
            Aggregator::new(state, HostName::new("local"), event_tx.clone()).with_change_request_resolver(Arc::clone(&resolver)),
            &durable_convoys,
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("aggregator stopped before refresh: {result:?}"),
            () = async {
                let initial = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "initial convoy result set").await;
                assert!(matches!(initial, DaemonEvent::ResultSet(_)));
                tokio::task::yield_now().await;
                event_tx
                    .send(DaemonEvent::RepoRefreshCompleted {
                        repo_identity: flotilla_protocol::RepoIdentity {
                            authority: "github.com".into(),
                            path: "flotilla-org/flotilla".into(),
                        },
                        repo: Some(std::path::PathBuf::from("/repo")),
                    })
                    .expect("publish provider refresh");

                assert!(timeout(Duration::from_millis(50), recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "unexpected convoy delta")).await.is_err());
            } => {}
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn unrelated_repository_refresh_does_not_resolve_convoy_again() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Closed,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let durable_convoys = ScriptedSource::new(
            vec![ResourceList { items: vec![convoy_with_branch("convoy-a").await], resource_version: "1".into(), generation: None }],
            vec![Ok(pending_watch())],
        );
        let run = run_with_test_sources(
            Aggregator::new(state, HostName::new("local"), event_tx.clone()).with_change_request_resolver(Arc::clone(&resolver)),
            &durable_convoys,
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("aggregator stopped before refresh: {result:?}"),
            () = async {
                assert!(matches!(
                    recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "initial convoy result set").await,
                    DaemonEvent::ResultSet(_)
                ));
                tokio::task::yield_now().await;
                for repo_identity in [
                    RepoIdentity { authority: "gitlab.com".into(), path: "other/widgets".into() },
                    RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() },
                ] {
                    event_tx
                        .send(DaemonEvent::RepoRefreshCompleted {
                            repo_identity,
                            repo: Some(std::path::PathBuf::from("/repo")),
                        })
                        .expect("publish provider refresh");
                }
                assert!(timeout(Duration::from_millis(50), recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "unexpected convoy delta")).await.is_err());
            } => {}
        }

        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "only the initial convoy lookup should resolve");
    }

    #[tokio::test(start_paused = true)]
    async fn repository_refresh_deduplicates_shared_repository_and_branch_lookup() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        for name in ["convoy-a", "convoy-b"] {
            aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_branch(name).await)).await;
        }
        apply_next_change_request_resolution(&mut aggregator).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        resolver.calls.store(0, Ordering::SeqCst);

        aggregator
            .refresh_repository_change_requests(&RepoIdentity {
                authority: "github.com".to_string(),
                path: "flotilla-org/flotilla".to_string(),
            })
            .await;
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn repeated_change_request_triggers_observe_per_convoy_minimum_interval() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        // Only a bound lookup can still reach the forge, so only it keeps the minimum interval.
        let convoy_a = bound(convoy_with_branch("convoy-a").await);
        let mut convoy_b = bound(convoy_with_branch("convoy-b").await);
        convoy_b.spec.r#ref = Some("feat/other".into());
        for convoy in [&convoy_a, &convoy_b] {
            aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
            apply_next_change_request_resolution(&mut aggregator).await;
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);

        let repo = RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() };
        aggregator.schedule_change_request_refresh(&convoy_a);
        for _ in 0..3 {
            aggregator.refresh_repository_change_requests(&repo).await;
        }
        tokio::task::yield_now().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL - Duration::from_secs(1)).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        tokio::time::advance(Duration::from_secs(1)).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_phase_change_keeps_the_admission_lookup_in_flight() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;
        convoy.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Pending, ..Default::default() });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let reference = aggregator.convoy_ref("flotilla", "convoy-a");
        assert!(aggregator.change_request_refresh_tasks.contains_key(&reference), "admission schedules a lookup at once");

        let mut active = convoy.clone();
        active.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Active, ..Default::default() });
        aggregator.apply_convoy_event(WatchEvent::Modified(active.clone())).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "the admission lookup completes without waiting a refresh interval");

        let mut landed = active;
        landed.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Landed, ..Default::default() });
        aggregator.schedule_change_request_refresh(&convoy);
        aggregator.apply_convoy_event(WatchEvent::Modified(landed)).await;
        assert!(!aggregator.change_request_refresh_tasks.contains_key(&reference), "an ended convoy still cancels its lookup");
    }

    // Behaviour (#3206): a lookup that finished while its resolution waits in
    // the queue is still pending; a phase change before it is applied keeps it
    // rather than replacing it and dropping its result as stale.
    #[tokio::test(start_paused = true)]
    async fn a_phase_change_keeps_a_finished_lookup_awaiting_application() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;
        convoy.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Pending, ..Default::default() });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
        let reference = aggregator.convoy_ref("flotilla", "convoy-a");
        while !aggregator.change_request_refresh_tasks[&reference].is_finished() {
            tokio::task::yield_now().await;
        }
        let mut active = convoy;
        active.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Active, ..Default::default() });
        aggregator.apply_convoy_event(WatchEvent::Modified(active)).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert!(
            !aggregator.change_request_refresh_tasks.contains_key(&reference),
            "the queued resolution was applied, not dropped as stale"
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    // Behaviour (#3206): a repository trigger arriving after a lookup started
    // (here, finished but not yet applied) does not replace it, but the convoy
    // looks again at its next interval, so the trigger's change is still read.
    #[tokio::test(start_paused = true)]
    async fn a_trigger_during_a_running_lookup_looks_again() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_branch("convoy-a").await)).await;
        let reference = aggregator.convoy_ref("flotilla", "convoy-a");
        while !aggregator.change_request_refresh_tasks[&reference].is_finished() {
            tokio::task::yield_now().await;
        }
        let repo = RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() };
        aggregator.refresh_repository_change_requests(&repo).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert!(aggregator.change_request_refresh_tasks.contains_key(&reference), "a follow-up lookup is scheduled");
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert!(!aggregator.change_request_refresh_tasks.contains_key(&reference), "and only one");
    }

    // Behaviour: a resolver that never answers fails its lookup after the
    // lookup timeout, so the convoy's refresh slot is released and retried. A
    // bound lookup is the one that can still reach the forge and retries.
    #[tokio::test(start_paused = true)]
    async fn a_hung_lookup_fails_at_the_timeout() {
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::new(BlockingChangeRequestResolver));
        aggregator.apply_convoy_event(WatchEvent::Added(bound(convoy_with_branch("convoy-a").await))).await;
        let reference = aggregator.convoy_ref("flotilla", "convoy-a");
        // Let the lookup start, so its timeout is running, then pass it.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(CHANGE_REQUEST_LOOKUP_TIMEOUT + Duration::from_secs(1)).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(aggregator.change_request_refresh_failures.get(&reference), Some(&1));
        assert!(aggregator.change_request_refresh_tasks.contains_key(&reference), "the failed lookup is retried");
    }

    // Behaviour (#3108): a lookup by branch reads only the forge observers' records. One that fails
    // is not retried by itself and does not wait an interval; the next change to the records looks
    // again at once.
    #[tokio::test(start_paused = true)]
    async fn a_failed_branch_lookup_waits_for_the_records_to_change() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Err("records unreadable".to_string()),
                Ok(Some(ConvoyChangeRequest {
                    id: "2301".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;
        convoy.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Active, ..Default::default() });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        let reference = aggregator.convoy_ref("flotilla", "convoy-a");
        apply_next_change_request_resolution(&mut aggregator).await;
        assert!(!aggregator.change_request_refresh_tasks.contains_key(&reference), "no retry of its own");
        aggregator.recheck_unproduced_change_requests();
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert_eq!(aggregator.convoy_change_requests[&reference].id, "2301");
    }

    // Behaviour (#3206): repository change-request churn does not defer a
    // convoy's pending first lookup; it completes without waiting an interval.
    #[tokio::test(start_paused = true)]
    async fn repository_triggers_keep_a_pending_first_lookup() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_branch("convoy-a").await)).await;
        let repo = RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() };
        for _ in 0..3 {
            aggregator.refresh_repository_change_requests(&repo).await;
        }
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "the admission lookup ran at once");
    }

    #[tokio::test]
    async fn deduplicated_refresh_keeps_independently_cancellable_convoy_tasks() {
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::new(BlockingChangeRequestResolver));
        for name in ["convoy-a", "convoy-b"] {
            aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_branch(name).await)).await;
        }

        aggregator
            .refresh_repository_change_requests(&RepoIdentity {
                authority: "github.com".to_string(),
                path: "flotilla-org/flotilla".to_string(),
            })
            .await;
        let convoy_a = aggregator.convoy_ref("flotilla", "convoy-a");
        let convoy_b = aggregator.convoy_ref("flotilla", "convoy-b");
        assert!(aggregator.change_request_refresh_tasks.contains_key(&convoy_a));
        assert!(aggregator.change_request_refresh_tasks.contains_key(&convoy_b));

        aggregator.invalidate_change_request(&convoy_a);

        assert!(!aggregator.change_request_refresh_tasks.contains_key(&convoy_a));
        assert!(aggregator.change_request_refresh_tasks.contains_key(&convoy_b));
        assert!(!aggregator.change_request_refresh_tasks[&convoy_b].is_finished());
    }

    #[tokio::test(start_paused = true)]
    async fn lag_recovery_global_refresh_still_resolves_convoys() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::new()),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        aggregator.apply_convoy_event(WatchEvent::Added(convoy_with_branch("convoy-a").await)).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        resolver.calls.store(0, Ordering::SeqCst);

        // The daemon-event lag branch uses this unscoped pass because the
        // identities of the skipped refreshes are unknown.
        aggregator.refresh_all_change_requests().await;
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
        apply_next_change_request_resolution(&mut aggregator).await;

        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn failed_home_lookup_retries_and_recovers_without_a_phase_change() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Err("rate limited".to_string()),
                Ok(Some(ConvoyChangeRequest {
                    id: "2301".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        // Only a bound lookup can still reach the forge, so only it retries under the backoff.
        aggregator.apply_convoy_event(WatchEvent::Added(bound(convoy_with_branch("convoy-a").await))).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "retry must respect backoff");
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert_eq!(aggregator.convoy_change_requests[&aggregator.convoy_ref("flotilla", "convoy-a")].id, "2301");
    }

    #[tokio::test(start_paused = true)]
    async fn a_change_request_record_change_rechecks_an_active_convoy_with_no_subject() {
        let (event_tx, _) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Ok(None),
                Ok(Some(ConvoyChangeRequest {
                    id: "2301".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;
        convoy.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Active, ..Default::default() });
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        apply_next_change_request_resolution(&mut aggregator).await;

        aggregator.recheck_unproduced_change_requests();
        tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
        assert_eq!(aggregator.convoy_change_requests[&aggregator.convoy_ref("flotilla", "convoy-a")].id, "2301");
    }

    /// Counts lookups, finds nothing, and ticks its record changes when the test sends.
    struct TickingChangeRequestResolver {
        calls: AtomicUsize,
        changes: Mutex<Option<futures::channel::mpsc::UnboundedReceiver<()>>>,
    }

    #[async_trait]
    impl ConvoyChangeRequestResolver for TickingChangeRequestResolver {
        async fn refresh_branch(
            &self,
            repositories: &[RepositoryKey],
            _branch: &str,
            _binding: Option<&BoundChangeRequest>,
        ) -> ConvoyBranchRefresh {
            self.calls.fetch_add(1, Ordering::SeqCst);
            ConvoyBranchRefresh { primary: Ok(None), repositories: repositories.iter().map(|key| (key.clone(), Ok(None))).collect() }
        }

        async fn change_request_changes(&self) -> Option<BoxStream<'static, ()>> {
            self.changes.lock().await.take().map(StreamExt::boxed)
        }
    }

    // No timer looks a convoy's change request up again (#3108): an active convoy that produces
    // none is looked up again when the change request records change, as a forge observer's
    // branch probe writes them.
    #[tokio::test(start_paused = true)]
    async fn a_convoy_without_a_change_request_is_looked_up_again_when_the_records_change() {
        let durable = ResourceBackend::new(InMemoryBackend::default());
        let template = convoy_with_branch("convoy-a").await;
        let convoys = durable.using::<Convoy>("flotilla");
        let created = convoys.create(&InputMeta::builder().name("convoy-a".to_string()).build(), &template.spec).await.expect("convoy");
        convoys
            .update_status(
                "convoy-a",
                &created.metadata.resource_version,
                &ConvoyStatus { phase: ResourceConvoyPhase::Active, ..Default::default() },
            )
            .await
            .expect("active");
        let (ticks, changes) = futures::channel::mpsc::unbounded();
        let resolver = Arc::new(TickingChangeRequestResolver { calls: AtomicUsize::new(0), changes: Mutex::new(Some(changes)) });
        let (tx, _rx) = broadcast::channel(4096);
        let aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), tx)
            .with_change_request_resolver(Arc::clone(&resolver));
        let task = tokio::spawn(aggregator.run(resource_resolvers(&durable)));
        let looked_up = |count: usize| {
            let resolver = Arc::clone(&resolver);
            async move {
                while resolver.calls.load(Ordering::SeqCst) < count {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        };
        timeout(Duration::from_secs(60), looked_up(1)).await.expect("the convoy's first lookup");
        tokio::time::sleep(Duration::from_secs(60 * 60)).await;
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1, "nothing looks again while the records are unchanged");
        ticks.unbounded_send(()).expect("tick");
        timeout(CHANGE_REQUEST_MIN_REFRESH_INTERVAL + Duration::from_secs(60), looked_up(2)).await.expect("looked up again");
        task.abort();
    }

    // RepoDelta invalidation re-resolves convoy PR state through the resource-backed resolver.
    #[tokio::test(start_paused = true)]
    async fn repo_delta_refreshes_convoy_change_requests() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(16);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Closed,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let durable_convoys = ScriptedSource::new(
            vec![ResourceList { items: vec![convoy_with_branch("convoy-a").await], resource_version: "1".into(), generation: None }],
            vec![Ok(pending_watch())],
        );
        let run = run_with_test_sources(
            Aggregator::new(state, HostName::new("local"), event_tx.clone()).with_change_request_resolver(Arc::clone(&resolver)),
            &durable_convoys,
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("aggregator stopped before repo delta: {result:?}"),
            () = async {
                let initial = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "initial convoy result set").await;
                assert!(matches!(initial, DaemonEvent::ResultSet(_)));
                tokio::task::yield_now().await;
                tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
                // A change-request delta triggers a repository-scoped resource resolver refresh.
                event_tx.send(DaemonEvent::RepoDelta(Box::new(RepoDelta {
                    seq: 1, prev_seq: 0,
                    repo_identity: RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() },
                    repo: None,
                    changes: vec![Change::ChangeRequest { key: "815".into(), op: flotilla_protocol::EntryOp::Removed }],
                }))).expect("publish repo delta");

                tokio::task::yield_now().await;
                tokio::time::advance(CHANGE_REQUEST_MIN_REFRESH_INTERVAL).await;
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
            } => {}
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn unrelated_repo_delta_does_not_refresh_convoy_change_requests() {
        let (event_tx, _) = broadcast::channel(1);
        let aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx);
        let repo_identity = RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() };
        let empty = RepoDelta { seq: 1, prev_seq: 0, repo_identity: repo_identity.clone(), repo: None, changes: Vec::new() };
        assert!(!aggregator.repo_delta_changed_change_requests(&empty));
        let checkout = RepoDelta {
            changes: vec![Change::Checkout {
                key: flotilla_protocol::qualified_path::QualifiedPath::from_host_name(&HostName::new("local"), "/work/flotilla"),
                op: flotilla_protocol::EntryOp::Removed,
            }],
            ..empty.clone()
        };
        assert!(!aggregator.repo_delta_changed_change_requests(&checkout));
        let removed =
            RepoDelta { changes: vec![Change::ChangeRequest { key: "815".into(), op: flotilla_protocol::EntryOp::Removed }], ..empty };
        assert!(aggregator.repo_delta_changed_change_requests(&removed));
    }

    fn convoy_names(rows: &Rows) -> Vec<&str> {
        rows.as_convoys().expect("convoy rows").iter().map(|row| row.name.as_str()).collect()
    }

    async fn recv_query_event(event_rx: &mut broadcast::Receiver<DaemonEvent>, query: QueryId, timeout_message: &str) -> DaemonEvent {
        loop {
            let event = timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{timeout_message}"))
                .expect("aggregator event");
            let matches_query = match &event {
                DaemonEvent::ResultSet(result_set) => result_set.query() == query,
                DaemonEvent::ResultDelta(delta) => delta.query() == query,
                _ => false,
            };
            if matches_query {
                return event;
            }
        }
    }

    async fn apply_next_change_request_resolution(aggregator: &mut Aggregator) {
        let resolution = timeout(Duration::from_secs(1), aggregator.change_request_refresh_queue.rx.recv())
            .await
            .expect("change request resolution timed out")
            .expect("change request resolution channel closed");
        aggregator.apply_change_request_resolution(resolution).await;
    }

    #[bon::builder]
    fn convoy_with_work(convoy_phase: ResourceConvoyPhase, work_phase: Option<ResourceWorkPhase>) -> ResourceObject<Convoy> {
        let definition = VesselRequirement::builder().name("implement".to_string()).crew(Vec::new()).build();
        let work = work_phase.map(|phase| ("implement".to_string(), WorkState::builder().phase(phase).build())).into_iter().collect();
        ResourceObject {
            metadata: ObjectMeta {
                name: "convoy-a".to_string(),
                namespace: "flotilla".to_string(),
                resource_version: "1".to_string(),
                labels: BTreeMap::new(),
                annotations: BTreeMap::new(),
                owner_references: Vec::new(),
                finalizers: Vec::new(),
                deletion_timestamp: None,
                creation_timestamp: Utc::now(),
                merge: None,
            },
            spec: ConvoySpec::builder().workflow_ref("scratch".to_string()).build(),
            status: Some(ConvoyStatus {
                phase: convoy_phase,
                workflow_snapshot: Some(WorkflowSnapshot {
                    cascade: None,
                    stall_nudges: Default::default(),
                    supervision: None,
                    exit: None,
                    turn_delivery: Default::default(),
                    vessels: vec![definition],
                }),
                work,
                // Admitted on the engine: its checkouts are held for this launch.
                engine_launch: Some("convoy-a#1".to_string()),
                ..Default::default()
            }),
        }
    }

    async fn create_placement_record<T: Resource>(spec: T::Spec) -> ResourceObject<T> {
        ResourceBackend::new(InMemoryBackend::default())
            .using::<T>("flotilla")
            .create(&InputMeta::builder().name("actual-kind".into()).build(), &spec)
            .await
            .expect("placement record")
    }

    async fn vessel_observing(version: &str, observed: bool) -> ResourceObject<Vessel> {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        let mut vessel = backend
            .using::<Vessel>("flotilla")
            .create(
                &InputMeta::builder().name("vessel".into()).build(),
                &flotilla_resources::VesselSpec {
                    convoy_ref: "convoy-a".into(),
                    vessel_name: "implement".into(),
                    fulfilment_kind_ref: if observed { "desired-kind" } else { "actual-kind" }.into(),
                    adopted_checkout_refs: Default::default(),
                },
            )
            .await
            .expect("vessel");
        vessel.status = observed.then(|| flotilla_resources::VesselStatus {
            observed_kind_ref: Some("actual-kind".into()),
            observed_kind_version: Some(version.to_string()),
            ..Default::default()
        });
        vessel
    }

    // Vessel rows carry the observed placement record's execution kind, including replicas.
    // Exhaustive kind/origin cases cover missing evidence, a later version and deletion;
    // no process or transport is involved in this resource-store join.
    async fn assert_vessel_environment_kind_follows<T: Resource + Clone>(
        record: ResourceObject<T>,
        expected: EnvironmentKind,
        records: fn(&mut Aggregator) -> &mut BTreeMap<RepositorySourceKey, ReadResourceObject<T>>,
    ) {
        for (remote, foreign_record, observed) in [(false, false, true), (true, false, true), (true, true, true), (false, false, false)] {
            let context = format!("remote={remote}, foreign_record={foreign_record}, observed={observed}, expected={expected:?}");
            let vessel = vessel_observing(&record.metadata.resource_version, observed).await;
            let (events, _) = broadcast::channel(8);
            let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), events);
            let provenance = if remote {
                let origin_root = flotilla_protocol::NodeId::new("remote-origin");
                aggregator.origin_hosts.insert(origin_root.clone(), HostName::new("local"));
                ResourceProvenance::Replica { origin_root, last_synced_at: Utc::now() }
            } else {
                ResourceProvenance::Local
            };
            apply_readiness_event(
                &mut aggregator.readiness_vessels,
                ReadWatchEvent::Added(ReadResourceObject { object: vessel, provenance: provenance.clone() }),
            );
            let convoy = convoy_with_vessel("convoy-a").await;
            let reference = ResourceRef::new("flotilla/v1", "Convoy", "flotilla", "convoy-a").on_host(HostName::new("local"));
            let status = convoy.status.as_ref().expect("status");
            let definition = &status.workflow_snapshot.as_ref().expect("snapshot").vessels[0];
            let row = |aggregator: &Aggregator| {
                aggregator.summarize_vessel(&reference, definition, status.work.get("implement"), None, None, &Ok(BTreeMap::new()))
            };
            assert_eq!(row(&aggregator).environment_kind, None, "{context}");
            let source = ReadResourceObject {
                object: record.clone(),
                provenance: if foreign_record { ResourceProvenance::Local } else { provenance },
            };
            apply_readiness_event(records(&mut aggregator), ReadWatchEvent::Added(source.clone()));
            assert_eq!(row(&aggregator).environment_kind, Some(expected), "{context}");
            // A later version is not evidence of the running vessel's kind.
            let mut changed = source.clone();
            changed.object.metadata.resource_version.push_str("-changed");
            apply_readiness_event(records(&mut aggregator), ReadWatchEvent::Modified(changed));
            assert_eq!(row(&aggregator).environment_kind, if observed { None } else { Some(expected) }, "{context}");
            apply_readiness_event(records(&mut aggregator), ReadWatchEvent::Deleted(source));
            assert_eq!(row(&aggregator).environment_kind, None, "{context}");
        }
    }

    #[tokio::test]
    async fn vessel_environment_kind_follows_observed_fulfilment_kind_and_withdraws_on_deletion() {
        for (realisation, expected) in [
            (FulfilmentRealisation::HostDirect, EnvironmentKind::Direct),
            (FulfilmentRealisation::DockerPerVessel { image: None }, EnvironmentKind::Provisioned),
            (
                FulfilmentRealisation::DockerPerVessel {
                    image: Some(flotilla_resources::PlacementImageSource::Literal("ubuntu:24.04".into())),
                },
                EnvironmentKind::Provisioned,
            ),
        ] {
            let spec = flotilla_resources::FulfilmentKindSpec::builder()
                .host_ref("local".into())
                .pool("arbitrary-instance".into())
                .realisation(realisation)
                .build();
            let kind = create_placement_record::<FulfilmentKind>(spec).await;
            assert_vessel_environment_kind_follows(kind, expected, |aggregator| &mut aggregator.fulfilment_kinds).await;
        }
    }

    #[tokio::test]
    async fn landing_reason_only_describes_landing_phases() {
        for phase in [ResourceConvoyPhase::Active, ResourceConvoyPhase::Landing, ResourceConvoyPhase::Landed] {
            let mut convoy = convoy_with_work().convoy_phase(phase).call();
            convoy.status.as_mut().expect("status").landing_entry = Some(flotilla_resources::LandingEntry {
                entered_at: Utc::now(),
                claims: vec![flotilla_resources::LandingClaim::builder()
                    .vessel("work".into())
                    .role("coder".into())
                    .completed_while_crew_active(false)
                    .build()],
                event_emitted: true,
            });
            let state = AggregatorProjectionState::new();
            let (tx, _rx) = broadcast::channel(1);
            let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);
            aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
            let result = state.result_set().await;
            let reason = &result.rows.as_convoys().expect("convoys")[0].landing_reason;
            assert_eq!(reason.is_some(), matches!(phase, ResourceConvoyPhase::Landing | ResourceConvoyPhase::Landed));
            if let Some(reason) = reason {
                assert!(reason.ends_with(": coder complete"));
            }
        }
    }

    async fn emitted_vessel(convoy: ResourceObject<Convoy>) -> VesselRow {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(1);
        let mut aggregator = Aggregator::new(state, HostName::new("local"), tx);

        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;

        let DaemonEvent::ResultSet(result_set) = rx.recv().await.expect("initial result set") else {
            panic!("expected result set");
        };
        let rows = result_set.rows.as_convoys().expect("convoy rows");
        let convoy = rows.first().expect("convoy row");
        convoy.vessels.first().expect("vessel row").clone()
    }

    // Environment loss is recoverable and blocked, never provisioning or failed.
    // Both local and replicated vessel evidence must show the cause before the
    // convoy's asynchronous work roll-up has caught up.
    #[tokio::test]
    async fn lost_vessel_readiness_is_recoverable_and_explains_the_cause() {
        use flotilla_protocol::result_set::ReadinessState;
        use flotilla_resources::{VesselPhase, VesselSpec, VesselStatus};
        for remote in [false, true] {
            for phase in [ResourceWorkPhase::Running, ResourceWorkPhase::Interrupted] {
                let backend = ResourceBackend::new(InMemoryBackend::default());
                let convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(phase).call();
                let (events, _) = broadcast::channel(128);
                // The engine holds no launch of this vessel, so its stored status is read.
                let engine = flotilla_provisioning_testkit::VesselsFixture::at(OWN_HOME);
                let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("kiwi"), events)
                    .with_lifecycle_reads(engine.reads.clone());
                let provenance = if remote {
                    let origin = flotilla_protocol::NodeId::new("remote-origin");
                    aggregator.origin_hosts.insert(origin.clone(), HostName::new("kiwi"));
                    ResourceProvenance::Replica { origin_root: origin, last_synced_at: Utc::now() }
                } else {
                    ResourceProvenance::Local
                };
                let mut vessel = backend
                    .using::<Vessel>("flotilla")
                    .create(
                        &InputMeta::builder().name("lost-vessel".into()).build(),
                        &VesselSpec {
                            convoy_ref: convoy.metadata.name.clone(),
                            vessel_name: "implement".into(),
                            fulfilment_kind_ref: "local".into(),
                            adopted_checkout_refs: Default::default(),
                        },
                    )
                    .await
                    .expect("vessel");
                let reason = "lost, recoverable: container gone; rehydration is not available yet (#2872)";
                vessel.status = Some(VesselStatus { phase: VesselPhase::Lost, message: Some(reason.into()), ..Default::default() });
                apply_readiness_event(
                    &mut aggregator.readiness_vessels,
                    ReadWatchEvent::Added(ReadResourceObject { object: vessel, provenance }),
                );
                let status = convoy.status.as_ref().expect("status");
                let definition = &status.workflow_snapshot.as_ref().expect("snapshot").vessels[0];
                let reference =
                    ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", &convoy.metadata.name).on_host(HostName::new("kiwi"));
                let row = aggregator.summarize_vessel(
                    &reference,
                    definition,
                    status.work.get("implement"),
                    None,
                    status.crew_work.get("implement"),
                    &Ok(BTreeMap::new()),
                );
                assert_eq!(row.readiness.state, ReadinessState::Blocked);
                assert_eq!(row.readiness.blockers[0].phase, "Lost");
                assert_eq!(row.readiness.blockers[0].reason, reason);
                assert_eq!(row.surface_state, SurfaceState::NeedsYou);
                assert_eq!(row.message.as_deref(), Some(reason));
            }
        }
    }

    // #1961: live checkout evidence distinguishes waiting, blocked retries and
    // confirmed failure. Recovery and replayed updates must clear old reasons.
    #[hegel::test]
    fn provisioning_readiness_tracks_current_checkout_evidence(tc: hegel::TestCase) {
        use flotilla_protocol::{result_set::ReadinessState, LifecycleReading};
        use flotilla_resources::{VesselPhase, VesselSpec, VesselStatus};
        use hegel::generators as gs;
        // Includes ordinary tasks and standing governors, repeats, failure,
        // retry, success, empty status and deletion, in arbitrary order.
        let governor = tc.draw(gs::booleans());
        let remote = tc.draw(gs::booleans());
        let operations = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(10)))
            .map(|_| tc.draw(gs::integers::<usize>().min_value(0).max_value(6)))
            .collect::<Vec<_>>();
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Ready).call();
            if governor {
                convoy.spec.role = "governor".into();
            }
            let (events, _) = broadcast::channel(128);
            let reads = Arc::new(ScriptedReadings::default());
            let mut aggregator =
                Aggregator::new(AggregatorProjectionState::new(), HostName::new("kiwi"), events).with_lifecycle_reads(reads.clone());
            let host = HostName::new(if remote { "feta" } else { "kiwi" });
            let provenance = if remote {
                let origin = flotilla_protocol::NodeId::new("remote-origin");
                aggregator.origin_hosts.insert(origin.clone(), host.clone());
                ResourceProvenance::Replica { origin_root: origin, last_synced_at: Utc::now() }
            } else {
                ResourceProvenance::Local
            };
            convoy.status.as_mut().expect("status").work.get_mut("implement").expect("work").placement =
                Some(PlacementStatus { fields: BTreeMap::from([("host".into(), serde_json::json!(host.as_str()))]) });
            // The work's launch at the vessel's home, read as the old Vessel view.
            let launch = reads.launch(if remote { "remote-origin" } else { "kiwi" }, &convoy.metadata.name, "implement", host.as_str());
            let mut vessel = flotilla_provisioning_testkit::object::<Vessel>(
                &launch,
                VesselSpec {
                    convoy_ref: convoy.metadata.name.clone(),
                    vessel_name: "implement".into(),
                    fulfilment_kind_ref: "local".into(),
                    adopted_checkout_refs: BTreeMap::new(),
                },
                VesselStatus { phase: VesselPhase::Provisioning, checkout_refs: BTreeMap::new(), ..Default::default() },
            );
            apply_readiness_event(
                &mut aggregator.readiness_vessels,
                ReadWatchEvent::Added(ReadResourceObject { object: vessel.clone(), provenance: provenance.clone() }),
            );
            // A worktree the engine holds for the convoy at the vessel's home: its clone's retries
            // are the engine's reading of the clone, read at the checkout's home whether the
            // checkout is local or replicated (#3108).
            let spec = CheckoutSpec::Worktree(
                flotilla_resources::CheckoutWorktreeSpec::builder()
                    .repo_ref(RepositoryKey("repo".into()))
                    .env_ref("local".into())
                    .r#ref("main".into())
                    .target_path("/work/ops".into())
                    .clone_ref("ops-clone".into())
                    .build(),
            );
            let home = if remote { "remote-origin" } else { "kiwi" };
            reads.checkouts.lock().expect("checkouts").insert("checkout-a".into(), (home.into(), spec));
            let reference =
                ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", &convoy.metadata.name).on_host(HostName::new("kiwi"));
            let mut terminal = session_object("terminal-ops-coder").await;
            terminal.metadata.labels.insert(CONVOY_LABEL.into(), convoy.metadata.name.clone());
            terminal.metadata.labels.insert(VESSEL_LABEL.into(), "implement".into());
            for op in operations.into_iter().chain([1, 3, 6]) {
                let expected = match op {
                    0 => ReadinessState::Provisioning,
                    1 => ReadinessState::Blocked,
                    2 => ReadinessState::Failed,
                    3 | 6 => ReadinessState::Ready,
                    _ => ReadinessState::Provisioning,
                };
                let cause = "Forgejo authentication refused".to_owned();
                // The checkout's own reading: given up with the cause, realised, or still being
                // made; deleted (5) it is held no more.
                let checkout = match op {
                    2 => Some(LifecycleReading::GaveUp { cause: cause.clone() }),
                    3 | 6 => Some(LifecycleReading::Ready { identity: "/work/ops".into() }),
                    5 => None,
                    _ => Some(LifecycleReading::InProgress),
                };
                reads.set(crate::ports::ProvisioningKind::Checkout, "checkout-a", checkout);
                let reading = match op {
                    1 | 6 => Some(LifecycleReading::Retrying { failures: 1, next: Utc::now(), cause }),
                    2 => Some(LifecycleReading::GaveUp { cause }),
                    3 => Some(LifecycleReading::Ready { identity: "/work/ops".into() }),
                    4 => None,
                    _ => Some(LifecycleReading::InProgress),
                };
                reads.set(crate::ports::ProvisioningKind::Clone, "ops-clone", reading);
                vessel.status.as_mut().expect("status").phase =
                    if op == 3 || op == 6 { VesselPhase::Ready } else { VesselPhase::Provisioning };
                reads.realise(op == 3 || op == 6);
                apply_readiness_event(
                    &mut aggregator.readiness_vessels,
                    ReadWatchEvent::Modified(ReadResourceObject { object: vessel.clone(), provenance: provenance.clone() }),
                );
                aggregator.terminal_sessions.clear();
                aggregator.attachable_sessions.clear();
                if op == 3 || op == 6 {
                    let source = ReadResourceObject { object: terminal.clone(), provenance: provenance.clone() };
                    aggregator.attachable_sessions.insert(session_key(&source));
                    aggregator.terminal_sessions.insert(session_key(&source), source);
                }
                let row = aggregator.summarize(&reference, &convoy);
                assert_eq!(row.phase, Some(ConvoyPhase::Active), "readiness must not rewrite lifecycle");
                assert_eq!(row.readiness().state, expected, "operation {op}");
                if op == 1 || op == 2 {
                    assert!(row.readiness().explanation().contains("Forgejo authentication refused"));
                    assert!(row.vessels[0].materialize.is_none());
                }
                if op == 3 || op == 6 {
                    assert!(row.readiness().blockers.is_empty(), "recovery clears all blockers");
                    assert_eq!(row.vessels[0].materialize.as_deref(), Some("terminal-ops-coder"));
                }
            }
        });
    }

    /// Readings scripted per resource, recording each keyed read: the kind, name and origin.
    #[derive(Default)]
    struct ScriptedReadings {
        errors: std::sync::Mutex<HashMap<(crate::ports::ProvisioningKind, String), String>>,
        readings: std::sync::Mutex<HashMap<(crate::ports::ProvisioningKind, String), flotilla_protocol::LifecycleReading>>,
        reads: std::sync::Mutex<Vec<(crate::ports::ProvisioningKind, String, Option<String>)>>,
        /// The home and spec of each checkout the engine holds: its reading is scripted as a
        /// [`crate::ports::ProvisioningKind::Checkout`] reading.
        checkouts: std::sync::Mutex<BTreeMap<String, (String, CheckoutSpec)>>,
        /// The vessel's launch at its home, once requested: still being realised, and realised.
        launches: std::sync::OnceLock<(flotilla_provisioning_testkit::VesselsFixture, flotilla_provisioning_testkit::VesselsFixture)>,
        /// Whether the vessel reads as its realised launch.
        realised: std::sync::atomic::AtomicBool,
    }

    impl ScriptedReadings {
        /// Requests the launch serving `work` of `convoy` on `host` at `home`, and returns its
        /// name; it reads as being realised until [`Self::realise`].
        fn launch(&self, home: &str, convoy: &str, work: &str, host: &str) -> String {
            let (pending, realised) = self.launches.get_or_init(|| {
                (flotilla_provisioning_testkit::VesselsFixture::at(home), flotilla_provisioning_testkit::VesselsFixture::at(home))
            });
            let launch = pending.request(convoy, work, host);
            assert_eq!(realised.request(convoy, work, host), launch, "both launches share one name");
            realised.ready(&launch, "env");
            launch
        }

        /// Whether the vessel reads as realised or still being realised.
        fn realise(&self, realised: bool) {
            self.realised.store(realised, std::sync::atomic::Ordering::SeqCst);
        }

        fn set(&self, kind: crate::ports::ProvisioningKind, name: &str, reading: Option<flotilla_protocol::LifecycleReading>) {
            let mut readings = self.readings.lock().expect("script");
            match reading {
                Some(reading) => readings.insert((kind, name.to_owned()), reading),
                None => readings.remove(&(kind, name.to_owned())),
            };
        }
    }

    impl crate::ports::LifecycleReads for ScriptedReadings {
        fn reading(
            &self,
            kind: crate::ports::ProvisioningKind,
            name: &str,
            origin: Option<&str>,
        ) -> Option<flotilla_protocol::LifecycleReading> {
            self.reads.lock().expect("reads").push((kind, name.to_owned(), origin.map(str::to_owned)));
            self.readings.lock().expect("script").get(&(kind, name.to_owned())).cloned()
        }
        fn try_reading(
            &self,
            kind: crate::ports::ProvisioningKind,
            name: &str,
            origin: Option<&str>,
        ) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
            // Fault injection at the keyed storage read boundary.
            match self.errors.lock().expect("errors").get(&(kind, name.to_owned())).cloned() {
                Some(error) => Err(error),
                None => Ok(self.reading(kind, name, origin)),
            }
        }
        fn clones(&self) -> Result<&dyn flotilla_resources::clone_records::CloneRecords, String> {
            Ok(self)
        }
        fn checkouts(&self) -> Result<&dyn flotilla_resources::checkout_records::CheckoutRecords, String> {
            Ok(self)
        }
        fn vessels(&self) -> Result<&dyn flotilla_resources::vessel_records::VesselRecords, String> {
            let (pending, realised) = self.launches.get().ok_or("no vessel launched")?;
            let fixture = if self.realised.load(std::sync::atomic::Ordering::SeqCst) { realised } else { pending };
            Ok(fixture.vessels.as_ref())
        }
    }

    /// The checkouts held for any convoy: each scripted checkout with a reading, as its first
    /// launch at its home. A scripted read error fails the whole read.
    impl flotilla_resources::checkout_records::CheckoutRecords for ScriptedReadings {
        fn get(&self, _: &str, _: Option<&str>) -> Result<Option<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            Err("readiness reads checkouts by holder".into())
        }
        fn held(
            &self,
            _: &flotilla_resources::checkout_records::CheckoutHolder,
        ) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            use crate::ports::{LifecycleReads as _, ProvisioningKind};
            let checkouts = self.checkouts.lock().expect("checkouts").clone();
            let mut held = Vec::new();
            for (name, (home, spec)) in checkouts {
                if let Some(reading) = self.try_reading(ProvisioningKind::Checkout, &name, Some(&home))? {
                    held.push(flotilla_resources::checkout_records::CheckoutRecord {
                        name: format!("{name}#1"),
                        home,
                        spec,
                        holder: None,
                        requested_at: chrono::DateTime::UNIX_EPOCH,
                        reading,
                        degraded: None,
                        integration: None,
                    });
                }
            }
            Ok(held)
        }
        fn of_repository(&self, _: &RepositoryKey) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            Ok(Vec::new())
        }
        fn in_environment(&self, _: &str, _: Option<&str>) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            Ok(Vec::new())
        }
        fn here(&self) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            Ok(Vec::new())
        }
        fn home(&self) -> &str {
            "home"
        }
        fn every(&self) -> Result<Vec<flotilla_resources::checkout_records::CheckoutRecord>, String> {
            self.held(&flotilla_resources::checkout_records::CheckoutHolder::Convoy(String::new()))
        }
        fn request(
            &self,
            _: &str,
            _: &CheckoutSpec,
            _: Option<&flotilla_resources::checkout_records::CheckoutHolder>,
        ) -> Result<String, String> {
            Err("readiness requests no checkout".into())
        }
        fn inspected(&self, _: &str, _: flotilla_resources::checkout_records::Inspection) -> Result<bool, String> {
            Err("readiness inspects no checkout".into())
        }
        fn inspection_failed(&self, _: &str) -> Result<(), String> {
            Err("readiness inspects no checkout".into())
        }
    }

    /// Clones read by key from the same script as [`crate::ports::ProvisioningKind::Clone`].
    impl flotilla_resources::clone_records::CloneRecords for ScriptedReadings {
        fn get(&self, name: &str, home: Option<&str>) -> Result<Option<flotilla_resources::clone_records::CloneRecord>, String> {
            use crate::ports::LifecycleReads as _;
            let reading = self.try_reading(crate::ports::ProvisioningKind::Clone, name, home)?;
            Ok(reading.map(|reading| flotilla_resources::clone_records::CloneRecord {
                name: format!("{name}#1"),
                home: home.unwrap_or("kiwi").to_owned(),
                spec: flotilla_resources::CloneSpec::builder()
                    .repo_ref(RepositoryKey("repo".into()))
                    .url("https://forgejo.test/ops".into())
                    .env_ref("local".into())
                    .path("/work/ops".into())
                    .build(),
                discovered: false,
                reading,
                default_branch: None,
            }))
        }
        fn of_repository(&self, _: &RepositoryKey) -> Result<Vec<flotilla_resources::clone_records::CloneRecord>, String> {
            Ok(Vec::new())
        }
        fn here(&self) -> Result<Vec<flotilla_resources::clone_records::CloneRecord>, String> {
            Ok(Vec::new())
        }
        fn every(&self) -> Result<Vec<flotilla_resources::clone_records::CloneRecord>, String> {
            Ok(Vec::new())
        }
        fn request(&self, _: &flotilla_resources::RepositorySpec, _: &flotilla_resources::CloneSpec, _: bool) -> Result<String, String> {
            Err("readiness requests no clone".into())
        }
        fn renew(&self, _: &str) -> Result<flotilla_resources::clone_records::CloneRenewal, String> {
            Err("readiness renews no clone".into())
        }
    }

    /// A convoy with a provisioning vessel on `host` (a replica's when `remote`), and a checkout
    /// `checkout-a` the engine holds for it at the vessel's home, made as a worktree of the clone
    /// `ops-clone` when `clone` is set; `reads` scripts both readings (#3108).
    async fn engine_readiness_fixture(
        remote: bool,
        clone: bool,
        reads: Arc<ScriptedReadings>,
    ) -> (Aggregator, ResourceRef, ResourceObject<Convoy>) {
        use flotilla_resources::{VesselPhase, VesselSpec, VesselStatus};
        let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Ready).call();
        let (events, _) = broadcast::channel(128);
        let mut aggregator =
            Aggregator::new(AggregatorProjectionState::new(), HostName::new("kiwi"), events).with_lifecycle_reads(reads.clone());
        let host = HostName::new(if remote { "feta" } else { "kiwi" });
        let provenance = if remote {
            let origin = flotilla_protocol::NodeId::new("remote-origin");
            aggregator.origin_hosts.insert(origin.clone(), host.clone());
            ResourceProvenance::Replica { origin_root: origin, last_synced_at: Utc::now() }
        } else {
            ResourceProvenance::Local
        };
        convoy.status.as_mut().expect("status").work.get_mut("implement").expect("work").placement =
            Some(PlacementStatus { fields: BTreeMap::from([("host".into(), serde_json::json!(host.as_str()))]) });
        // The work's launch at the vessel's home, read as the old Vessel view.
        let launch = reads.launch(if remote { "remote-origin" } else { "kiwi" }, &convoy.metadata.name, "implement", host.as_str());
        let vessel = flotilla_provisioning_testkit::object::<Vessel>(
            &launch,
            VesselSpec {
                convoy_ref: convoy.metadata.name.clone(),
                vessel_name: "implement".into(),
                fulfilment_kind_ref: "local".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
            VesselStatus { phase: VesselPhase::Provisioning, checkout_refs: BTreeMap::new(), ..Default::default() },
        );
        apply_readiness_event(
            &mut aggregator.readiness_vessels,
            ReadWatchEvent::Added(ReadResourceObject { object: vessel, provenance: provenance.clone() }),
        );
        let mut checkout = checkout_object("checkout-a", "/work/repo", RepositoryKey("repo".into())).await;
        if clone {
            checkout.spec = CheckoutSpec::Worktree(
                flotilla_resources::CheckoutWorktreeSpec::builder()
                    .repo_ref(RepositoryKey("repo".into()))
                    .env_ref("local".into())
                    .r#ref("main".into())
                    .target_path("/work/ops".into())
                    .clone_ref("ops-clone".into())
                    .build(),
            );
        }
        let home = if remote { "remote-origin" } else { "kiwi" };
        reads.checkouts.lock().expect("checkouts").insert("checkout-a".into(), (home.into(), checkout.spec));
        let reference = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", &convoy.metadata.name).on_host(HostName::new("kiwi"));
        (aggregator, reference, convoy)
    }

    /// The readiness state, and each checkout and clone blocker's resource name and reason.
    fn readiness_blockers(
        aggregator: &Aggregator,
        reference: &ResourceRef,
        convoy: &ResourceObject<Convoy>,
    ) -> (flotilla_protocol::result_set::ReadinessState, Vec<(String, String)>) {
        let row = aggregator.summarize(reference, convoy);
        let readiness = row.readiness().clone();
        let provisioning =
            |blocker: &&flotilla_protocol::result_set::ReadinessBlocker| matches!(blocker.resource.kind.as_str(), "Checkout" | "Clone");
        (
            readiness.state,
            readiness.blockers.iter().filter(provisioning).map(|blocker| (blocker.resource.name.clone(), blocker.reason.clone())).collect(),
        )
    }

    // Behaviour (#3629 layer 4, #3108): the engine's reading of a checkout it holds for the convoy
    // at the vessel's home (a replica's origin root) decides its readiness; one held at another
    // home is not the vessel's, and a read that fails blocks the vessel.
    #[tokio::test]
    async fn engine_readings_decide_checkout_readiness() {
        use flotilla_protocol::{result_set::ReadinessState, LifecycleReading};

        use crate::ports::ProvisioningKind::Checkout as Kind;
        let reads = Arc::new(ScriptedReadings::default());
        let (aggregator, reference, convoy) = engine_readiness_fixture(true, false, reads.clone()).await;
        let checkout = |reading| {
            reads.set(Kind, "checkout-a", reading);
            readiness_blockers(&aggregator, &reference, &convoy)
        };
        let blocker = |reason: &str| vec![("checkout-a".to_owned(), reason.to_owned())];

        assert!(checkout(None).1.is_empty(), "nothing held: no checkout blocks");
        assert_eq!(
            checkout(Some(LifecycleReading::GaveUp { cause: "branch conflicts".into() })),
            (ReadinessState::Failed, blocker("branch conflicts"))
        );
        let (state, blockers) = checkout(Some(LifecycleReading::Retrying {
            failures: 2,
            next: chrono::DateTime::UNIX_EPOCH,
            cause: "worktree add failed".into(),
        }));
        assert_eq!(state, ReadinessState::Blocked);
        assert!(blockers[0].1.starts_with("worktree add failed (2 failed"));
        assert_eq!(checkout(Some(LifecycleReading::InProgress)), (ReadinessState::Provisioning, blocker("being realised")));
        assert_eq!(checkout(Some(LifecycleReading::Lost { reason: None })), (ReadinessState::Failed, blocker("observed absent")));
        assert!(checkout(Some(LifecycleReading::Ready { identity: "/work/repo".into() })).1.is_empty());
        // Held at another home, it is not the remote vessel's checkout.
        reads.checkouts.lock().expect("checkouts").get_mut("checkout-a").expect("scripted").0 = "kiwi".into();
        assert!(checkout(Some(LifecycleReading::InProgress)).1.is_empty(), "another home's checkout is not the vessel's");
        // The held checkouts' read fails: the vessel is blocked on its checkouts, with the reason.
        reads.errors.lock().expect("errors").insert((Kind, "checkout-a".into()), "checkout store unavailable".into());
        let row = aggregator.summarize(&reference, &convoy);
        assert_eq!(row.readiness().state, ReadinessState::Blocked);
        assert!(row.readiness().explanation().contains("checkout store unavailable"), "{}", row.readiness().explanation());
    }

    // Behaviour (#3629 layer 4, #3731 review, #3108): a worktree's clone is read from the engine
    // too, at its checkout's home (here, for a local checkout); it blocks the vessel beside its
    // checkout.
    #[tokio::test]
    async fn engine_readings_decide_clone_readiness() {
        use flotilla_protocol::{result_set::ReadinessState, LifecycleReading};

        use crate::ports::ProvisioningKind;
        let reads = Arc::new(ScriptedReadings::default());
        let (aggregator, reference, convoy) = engine_readiness_fixture(false, true, reads.clone()).await;
        reads.set(ProvisioningKind::Checkout, "checkout-a", Some(LifecycleReading::InProgress));
        reads.set(ProvisioningKind::Clone, "ops-clone", Some(LifecycleReading::GaveUp { cause: "repository not found".into() }));
        let (state, blockers) = readiness_blockers(&aggregator, &reference, &convoy);
        assert_eq!(state, ReadinessState::Failed);
        assert_eq!(
            blockers,
            vec![("ops-clone".to_owned(), "repository not found".to_owned()), ("checkout-a".to_owned(), "being realised".to_owned())]
        );
        reads.set(ProvisioningKind::Clone, "ops-clone", Some(LifecycleReading::Ready { identity: "/work/ops".into() }));
        assert_eq!(
            readiness_blockers(&aggregator, &reference, &convoy),
            (ReadinessState::Provisioning, vec![("checkout-a".to_owned(), "being realised".to_owned())])
        );
        reads.errors.lock().expect("errors").insert((ProvisioningKind::Clone, "ops-clone".into()), "decided store unavailable".into());
        assert_eq!(
            readiness_blockers(&aggregator, &reference, &convoy),
            (ReadinessState::Blocked, vec![("ops-clone".into(), "decided store unavailable".into())])
        );
        let reads = reads.reads.lock().expect("reads").clone();
        assert!(reads.contains(&(ProvisioningKind::Clone, "ops-clone".into(), Some("kiwi".into()))), "{reads:?}");
    }

    // Engine readings decide readiness despite stale lifecycle phase/message; interrupted roles remain observations.
    #[hegel::test]
    fn the_engine_reading_decides_a_vessel_view(tc: hegel::TestCase) {
        // Every old phase, live readings and endings, with empty and Unicode causes; no old copy is published.
        let old = [
            VesselPhase::Pending,
            VesselPhase::Provisioning,
            VesselPhase::Ready,
            VesselPhase::Interrupted,
            VesselPhase::Failed,
            VesselPhase::Lost,
        ][tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(5))];
        let reason = ["", "backing failed", "失敗"][tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(2))];
        let mode = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(7));
        assert_vessel_readiness_contract(old, reason, mode);
    }

    // Fixed boundaries complement generated phase/ending coverage under the bounded default profile.
    #[test]
    fn vessel_readiness_exposes_exhaustion_and_missing_failure_text() {
        assert_vessel_readiness_contract(VesselPhase::Ready, "", 2);
        assert_vessel_readiness_contract(VesselPhase::Ready, "provider refused", 7);
    }

    fn assert_vessel_readiness_contract(old: VesselPhase, reason: &str, mode: usize) {
        use flotilla_protocol::{CanonicalHostId, LifecycleReading, PlacementDecision, PlacementTargetHost};
        use flotilla_provisioning_testkit::{object, VesselsFixture};
        use flotilla_resources::{vessel_records::VesselStep, VesselSpec, VesselStatus};
        let recorded_reason = reason;
        let ready = |environment: &str| VesselStep::Realised { environment: environment.into() };
        let (steps, expected, reason) = match mode {
            0 => (vec![VesselStep::Released], VesselPhase::Lost, "released".to_owned()),
            1 => {
                (vec![ready("env"), VesselStep::Lost { reason: "observed absent".into() }], VesselPhase::Lost, "observed absent".to_owned())
            }
            2 => (
                vec![ready("env"), VesselStep::Broke { reason: reason.into() }],
                VesselPhase::Failed,
                if reason.is_empty() { "failed (no reason recorded)".to_owned() } else { reason.to_owned() },
            ),
            3 => (vec![VesselStep::Failed { cause: reason.into(), retryable: false }], VesselPhase::Failed, reason.to_owned()),
            4 => (Vec::new(), VesselPhase::Provisioning, "being realised".to_owned()),
            5 => (vec![ready("env")], VesselPhase::Interrupted, "crew roles are interrupted: coder".to_owned()),
            6 => (
                vec![VesselStep::Failed { cause: "provider is busy".into(), retryable: true }],
                VesselPhase::Provisioning,
                "provider is busy (1 failed; next try at".to_owned(),
            ),
            // Its tries spent and no give-up written yet: a restart between the two leaves it so.
            _ => (Vec::new(), VesselPhase::Failed, "provider refused (attempts spent)".to_owned()),
        };
        let convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Running).call();
        let status = convoy.status.as_ref().expect("status");
        let definition = &status.workflow_snapshot.as_ref().expect("snapshot").vessels[0];
        let fixture = VesselsFixture::default();
        let launch = fixture.request(&convoy.metadata.name, &definition.name, "host");
        let vessel = object::<Vessel>(
            &launch,
            VesselSpec {
                convoy_ref: convoy.metadata.name.clone(),
                vessel_name: definition.name.clone(),
                fulfilment_kind_ref: "local".into(),
                adopted_checkout_refs: Default::default(),
            },
            VesselStatus {
                phase: old,
                message: Some("stale".into()),
                interrupted_roles: BTreeSet::from(["coder".into()]),
                checkout_refs: BTreeMap::from([(flotilla_resources::RepositoryKey("stale".into()), "missing".into())]),
                placement_decision: Some(
                    PlacementDecision::builder()
                        .kind("local".into())
                        .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved("host"), display_name: "host".into() })
                        .build(),
                ),
                ..Default::default()
            },
        );
        for step in steps {
            fixture.step(&launch, step);
        }
        if mode == 7 {
            fixture.exhaust_before_give_up(&launch, "provider refused");
        }
        let (events, _) = broadcast::channel(8);
        let mut aggregator =
            Aggregator::new(AggregatorProjectionState::new(), HostName::new("host"), events).with_lifecycle_reads(fixture.reads.clone());
        let read = ReadResourceObject { object: vessel, provenance: ResourceProvenance::Local };
        // Empty interrupted-role evidence uses a complete generic reason, never a dangling label or stale message.
        if mode == 5 {
            let mut empty = read.clone();
            empty.object.status.as_mut().expect("status").interrupted_roles.clear();
            assert_eq!(
                readiness::vessel_blocker_reason(
                    Some(&LifecycleReading::Ready { identity: "env".into() }),
                    &empty,
                    VesselPhase::Interrupted
                ),
                "vessel is not ready"
            );
        }

        apply_readiness_event(&mut aggregator.readiness_vessels, ReadWatchEvent::Added(read.clone()));
        let reference = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", &convoy.metadata.name).on_host(HostName::new("host"));
        let (readiness, _) = aggregator.vessel_readiness(
            &reference,
            definition,
            status.work.get(&definition.name),
            None,
            &HostName::new("host"),
            &Ok(BTreeMap::new()),
        );
        assert_eq!(readiness.blockers.len(), 1, "stale checkout refs are not combined with engine evidence");
        // A retrying launch's next try is at the engine's clock; the rest of its reason is fixed.
        if mode == 6 {
            assert!(readiness.blockers[0].reason.starts_with(&reason), "{}", readiness.blockers[0].reason);
        } else {
            assert_eq!(readiness.blockers[0].reason, reason);
        }
        assert_eq!(readiness.blockers[0].phase, format!("{expected:?}"));
        if mode == 2 {
            assert_eq!(
                flotilla_resources::vessel_records::engine_vessel(fixture.reads.as_ref(), &read.object, None)
                    .map(|record| record.map(|record| record.reading)),
                Ok(Some(LifecycleReading::Failed { reason: recorded_reason.into() }))
            );
        }
        fixture.poison();
        let (readiness, _) = aggregator.vessel_readiness(
            &reference,
            definition,
            status.work.get(&definition.name),
            None,
            &HostName::new("host"),
            &Ok(BTreeMap::new()),
        );
        assert_eq!(readiness.blockers[0].phase, "Unknown");
        assert!(readiness.blockers[0].reason.contains("poisoned"));
    }

    // #1961: Active plus pending agent work must remain provisioning, even
    // when the workflow has reached Running. Dormant handoff roles are excluded.
    #[tokio::test]
    async fn pending_agent_readiness_clears_when_crew_starts() {
        use flotilla_protocol::result_set::ReadinessState;
        use flotilla_resources::{CrewWorkPhase, CrewWorkState, Selector};
        let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Running).call();
        let status = convoy.status.as_mut().expect("status");
        for role in ["coder", "reviewer"] {
            status.workflow_snapshot.as_mut().expect("snapshot").vessels[0].crew.push(
                CrewSpec::builder()
                    .role(role.into())
                    .source(CrewSource::Agent { selector: Selector::for_capability("code"), prompt: None, brief_template: None })
                    .build(),
            );
        }
        let row = emitted_vessel(convoy.clone()).await;
        assert_eq!(row.readiness.state, ReadinessState::Provisioning);
        assert!(row.readiness.explanation().contains("coder has not started"));
        assert!(!row.readiness.explanation().contains("reviewer"));
        convoy
            .status
            .as_mut()
            .expect("status")
            .crew_work
            .insert("implement".into(), BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build())]));
        let row = emitted_vessel(convoy).await;
        assert_eq!(row.readiness.state, ReadinessState::Ready);
        assert!(row.readiness.blockers.is_empty());
    }

    // Terminal convoys report no working crew surface, with or without a
    // frozen workflow or stale crew phases from previous-generation records.
    #[tokio::test]
    async fn terminal_convoy_has_no_working_crew_surface() {
        use flotilla_resources::{CrewWorkPhase, CrewWorkState};
        for convoy_phase in [ResourceConvoyPhase::Abandoned, ResourceConvoyPhase::Landed, ResourceConvoyPhase::Failed] {
            for snapshot in [false, true] {
                for phase in [CrewWorkPhase::Pending, CrewWorkPhase::Working, CrewWorkPhase::Stalled, CrewWorkPhase::Interrupted] {
                    let state = AggregatorProjectionState::new();
                    let (event_tx, _) = broadcast::channel(8);
                    let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
                    let mut convoy = convoy_with_work().convoy_phase(convoy_phase).work_phase(ResourceWorkPhase::Abandoned).call();
                    let status = convoy.status.as_mut().unwrap();
                    if !snapshot {
                        status.workflow_snapshot = None;
                    }
                    status
                        .crew_work
                        .insert("implement".into(), BTreeMap::from([("coder".into(), CrewWorkState::builder().phase(phase).build())]));
                    if snapshot {
                        let mut session = session_object("terminal-abandoned-coder").await;
                        session.metadata.labels.insert(CONVOY_LABEL.into(), convoy.metadata.name.clone());
                        session.metadata.labels.insert(VESSEL_LABEL.into(), "implement".into());
                        session.spec.role = "coder".into();
                        agent_reports(
                            &mut aggregator,
                            None,
                            "terminal-abandoned-coder",
                            flotilla_protocol::AgentActivity::Working,
                            Utc::now(),
                        )
                        .await;
                        let source = ReadResourceObject { object: session, provenance: ResourceProvenance::Local };
                        aggregator.terminal_sessions.insert(session_key(&source), source);
                    }
                    aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
                    let result = state.result_set().await;
                    let row = &result.rows.as_convoys().unwrap()[0];
                    assert_eq!(row.phase, Some(super::convoy_phase(convoy_phase)));
                    assert_eq!(row.surface_state, SurfaceState::Available);
                    assert!(row.vessels.iter().all(|vessel| vessel.surface_state == SurfaceState::Available));
                }
            }
        }
    }

    #[tokio::test]
    async fn terminal_work_is_not_completable() {
        for phase in [ResourceWorkPhase::Complete, ResourceWorkPhase::Failed, ResourceWorkPhase::Cancelled] {
            let vessel = emitted_vessel(convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(phase).call()).await;

            assert!(!vessel.complete_work, "{phase:?} work must not expose the completion override");
        }
    }

    #[tokio::test]
    async fn work_missing_from_status_is_not_completable() {
        let vessel = emitted_vessel(convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).call()).await;

        assert!(!vessel.complete_work);
    }

    #[tokio::test]
    async fn vessel_and_crew_rows_expose_requested_and_effective_stance() {
        let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Running).call();
        let status = convoy.status.as_mut().expect("convoy status");
        let definition = &mut status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0];
        definition
            .crew
            .push(CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "cargo test".to_string() }).build());
        status.work.get_mut("implement").expect("work state").placement = Some(PlacementStatus {
            fields: BTreeMap::from([
                ("requested_stance".to_string(), serde_json::json!("workspace-write")),
                ("effective_stance".to_string(), serde_json::json!("contained")),
                ("image_ref".to_string(), serde_json::json!("registry.example/crew:latest")),
                ("local_image_id".to_string(), serde_json::json!("sha256:test-image")),
                ("environment_ref".to_string(), serde_json::json!("env-work")),
                ("configured_limits".to_string(), serde_json::json!({"cpus": 4, "build_jobs": 4, "linker_threads": 4})),
            ]),
        });

        let vessel = emitted_vessel(convoy).await;

        assert_eq!(vessel.requested_stance.as_deref(), Some("workspace-write"));
        assert_eq!(vessel.effective_stance.as_deref(), Some("contained"));
        assert_eq!(vessel.image_ref.as_deref(), Some("registry.example/crew:latest"));
        assert_eq!(vessel.local_image_id.as_deref(), Some("sha256:test-image"));
        // #2650: frozen placement facts remain visible even when the terminal
        // replica has not arrived; unknown launch adapter/model stay absent.
        assert_eq!(vessel.environment_ref.as_deref(), Some("env-work"));
        assert_eq!(
            vessel.configured_limits,
            Some(ConfiguredResourceLimits { cpus: Some(4), cpu_shares: None, build_jobs: Some(4), linker_threads: Some(4) })
        );
        assert_eq!(vessel.crew[0].adapter, None);
        assert_eq!(vessel.crew[0].model, None);
        assert_eq!(vessel.crew[0].requested_stance.as_deref(), Some("workspace-write"));
        assert_eq!(vessel.crew[0].effective_stance.as_deref(), Some("contained"));
    }

    // #2650: launch facts come from the matching observed session, including
    // replicas; a configured launch model may remain unknown.
    #[tokio::test]
    async fn vessel_crew_launch_facts_follow_session_provenance() {
        for remote in [false, true] {
            for model in [None, Some("gpt-6.1-sol".to_string())] {
                let state = AggregatorProjectionState::new();
                let (events, _) = broadcast::channel(16);
                let mut aggregator = Aggregator::new(state, HostName::new("kiwi"), events)
                    .with_attach_resolver(Arc::new(CountingAttachResolver::with_origin("remote-placement", "feta")));
                let host = HostName::new(if remote { "feta" } else { "kiwi" });
                let provenance = if remote {
                    let origin = flotilla_protocol::NodeId::new("remote-placement");
                    aggregator.origin_hosts.insert(origin.clone(), host.clone());
                    ResourceProvenance::Replica { origin_root: origin, last_synced_at: Utc::now() }
                } else {
                    ResourceProvenance::Local
                };
                let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Running).call();
                let status = convoy.status.as_mut().expect("convoy status");
                let definition = &mut status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0];
                definition
                    .crew
                    .push(CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "codex".to_string() }).build());
                let definition = definition.clone();
                let work = status.work.get_mut("implement").expect("implement work");
                work.placement = Some(PlacementStatus {
                    fields: BTreeMap::from([
                        ("host".to_string(), serde_json::json!(host)),
                        ("configured_limits".to_string(), serde_json::json!({"cpus": 8, "build_jobs": 4, "linker_threads": 4})),
                    ]),
                });
                let mut session = session_object("terminal-placement-coder").await;
                session.spec.role = "coder".into();
                session.metadata.labels =
                    BTreeMap::from([(CONVOY_LABEL.into(), convoy.metadata.name.clone()), (VESSEL_LABEL.into(), "implement".into())]);
                session.status.as_mut().expect("running session status").crew = Some(
                    flotilla_resources::CrewSessionStatus::builder()
                        .id("crew-placement".to_string())
                        .adapter("codex".to_string())
                        .maybe_model(model.clone())
                        .stance("contained".to_string())
                        .build(),
                );
                session.status.as_mut().expect("session status").configured_limits =
                    Some(ConfiguredResourceLimits { cpus: None, cpu_shares: None, build_jobs: Some(2), linker_threads: None });
                aggregator.replace_replica_sessions(vec![ReadResourceObject { object: session, provenance }]).await;
                let resource = ResourceRef::new("flotilla/v1", "Convoy", "flotilla", &convoy.metadata.name).on_host(HostName::new("kiwi"));
                let row = aggregator.summarize_vessel(&resource, &definition, Some(work), None, None, &Ok(BTreeMap::new()));
                assert_eq!(row.crew[0].adapter.as_deref(), Some("codex"));
                assert_eq!(row.crew[0].model, model);
                assert_eq!(
                    row.configured_limits,
                    Some(ConfiguredResourceLimits { cpus: Some(8), cpu_shares: None, build_jobs: Some(2), linker_threads: Some(4) })
                );
                assert_eq!(row.crew[0].session.as_deref(), Some(format!("{host}/flotilla/terminal-placement-coder").as_str()));
            }
        }
    }

    #[tokio::test]
    async fn remotely_placed_vessel_exposes_placement_host() {
        let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Running).call();
        convoy.status.as_mut().expect("convoy status").work.get_mut("implement").expect("work state").placement = Some(PlacementStatus {
            fields: BTreeMap::from([
                ("host".to_string(), serde_json::json!("feta")),
                ("vessel_ref".to_string(), serde_json::json!("convoy-a-implement")),
            ]),
        });

        let vessel = emitted_vessel(convoy).await;

        assert_eq!(vessel.host, HostName::new("feta"));
        assert_eq!(vessel.vessel_resource.expect("vessel resource").host, Some(HostName::new("feta")));
    }

    // #4021: a vessel on an SSH host is acted on by the Host's
    // declared owning daemon, which writes every record about it. Joins use
    // that daemon; the machine the vessel runs on is information only.
    #[tokio::test]
    async fn vessel_on_ssh_host_joins_records_through_its_owning_daemon() {
        use flotilla_protocol::{result_set::ReadinessState, CanonicalHostId, PlacementDecision, PlacementTargetHost};
        use flotilla_resources::{HostConnection, HostSpec, VesselPhase, VesselSpec, VesselStatus};
        // A drydock cloud sandbox (Daytona) is placed on its controller's
        // daemon Host, which the same mapping resolves to itself.
        for (target, machine) in [("host-b1", "b1"), ("host-udder", "udder")] {
            let backend = ResourceBackend::new(InMemoryBackend::default());
            let (events, _) = broadcast::channel(128);
            // The engine holds no launch of this vessel, so its stored status is read.
            let engine = flotilla_provisioning_testkit::VesselsFixture::at(OWN_HOME);
            let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("kiwi"), events)
                .with_attach_resolver(Arc::new(CountingAttachResolver::with_origin("udder-node", "udder")))
                .with_lifecycle_reads(engine.reads.clone());
            let origin = flotilla_protocol::NodeId::new("udder-node");
            aggregator.origin_hosts.insert(origin.clone(), HostName::new("udder"));
            let provenance = ResourceProvenance::Replica { origin_root: origin, last_synced_at: Utc::now() };
            for (name, display_name, connection) in [
                ("host-udder", "udder", HostConnection::Daemon),
                ("host-b1", "b1", HostConnection::SshHost { owning_daemon: "host-udder".into(), destination: "b1.lan".into() }),
            ] {
                let host = backend
                    .using::<flotilla_resources::Host>("flotilla")
                    .create(
                        &InputMeta::builder().name(name.into()).build(),
                        &HostSpec { display_name: display_name.into(), connection, ..Default::default() },
                    )
                    .await
                    .expect("host");
                assert!(
                    aggregator.apply_host_event(ReadWatchEvent::Added(ReadResourceObject { object: host, provenance: provenance.clone() }))
                );
            }

            let mut convoy = convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(ResourceWorkPhase::Running).call();
            let status = convoy.status.as_mut().expect("convoy status");
            status.workflow_snapshot.as_mut().expect("workflow snapshot").vessels[0]
                .crew
                .push(CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "codex".to_string() }).build());
            let decision = PlacementDecision::builder()
                .kind("b1".into())
                .target_host(PlacementTargetHost { reference: CanonicalHostId::resolved(target), display_name: machine.into() })
                .build();
            status.work.get_mut("implement").expect("work state").placement = Some(PlacementStatus {
                fields: BTreeMap::from([("placement_decision".to_string(), serde_json::to_value(decision).expect("decision"))]),
            });

            let mut vessel = backend
                .using::<Vessel>("flotilla")
                .create(
                    &InputMeta::builder().name("ssh-vessel".into()).build(),
                    &VesselSpec {
                        convoy_ref: convoy.metadata.name.clone(),
                        vessel_name: "implement".into(),
                        fulfilment_kind_ref: "b1".into(),
                        adopted_checkout_refs: Default::default(),
                    },
                )
                .await
                .expect("vessel");
            vessel.status = Some(VesselStatus { phase: VesselPhase::Provisioning, ..Default::default() });
            apply_readiness_event(
                &mut aggregator.readiness_vessels,
                ReadWatchEvent::Added(ReadResourceObject { object: vessel, provenance: provenance.clone() }),
            );

            let mut session = session_object("terminal-ssh-coder").await;
            session.spec.role = "coder".into();
            session.metadata.labels =
                BTreeMap::from([(CONVOY_LABEL.into(), convoy.metadata.name.clone()), (VESSEL_LABEL.into(), "implement".into())]);
            home_heard(&mut aggregator, "udder-node", Utc::now()).await;
            agent_reports(&mut aggregator, Some("udder-node"), "terminal-ssh-coder", flotilla_protocol::AgentActivity::Working, Utc::now())
                .await;
            let session = ReadResourceObject { object: session, provenance };
            let key = session_key(&session);
            aggregator.replace_replica_sessions(vec![session]).await;
            aggregator.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
            // Attach capability resolves asynchronously; settle it as attachable.
            aggregator.attachable_sessions.insert(key);
            let status = convoy.status.as_ref().expect("convoy status");
            let row = aggregator.summarize_vessel(
                &aggregator.convoy_ref("flotilla", &convoy.metadata.name),
                &status.workflow_snapshot.as_ref().expect("workflow snapshot").vessels[0],
                status.work.get("implement"),
                None,
                status.crew_work.get("implement"),
                &Ok(BTreeMap::new()),
            );

            assert_eq!(row.host, HostName::new("udder"));
            assert_eq!(row.carried_by, Some(HostName::new(machine)));
            assert_eq!(row.materialize.as_deref(), Some("terminal-ssh-coder"));
            assert_eq!(row.crew[0].session.as_deref(), Some("udder/flotilla/terminal-ssh-coder"));
            assert_eq!(row.surface_state, SurfaceState::Working);
            assert_eq!(row.readiness.state, ReadinessState::Provisioning);
            assert_eq!(row.readiness.blockers[0].resource.host, Some(HostName::new("udder")));
            assert!(aggregator.salience_facts().attention.iter().any(|fact| fact.target == row.resource));
        }
    }

    // #4021: Host heartbeats do not reproject vessel rows; a change to the
    // declared authority (display name, connection) or a removal does.
    #[tokio::test]
    async fn host_events_report_authority_changes_only() {
        use flotilla_resources::{HostConnection, HostSpec, HostStatus};
        let (events, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("kiwi"), events);
        let read = |object| ReadResourceObject { object, provenance: ResourceProvenance::Local };
        let mut host = ResourceBackend::new(InMemoryBackend::default())
            .using::<flotilla_resources::Host>("flotilla")
            .create(
                &InputMeta::builder().name("host-b1".into()).build(),
                &HostSpec {
                    display_name: "b1".into(),
                    connection: HostConnection::SshHost { owning_daemon: "host-udder".into(), destination: "b1.lan".into() },
                    ..Default::default()
                },
            )
            .await
            .expect("host");
        assert!(aggregator.apply_host_event(ReadWatchEvent::Added(read(host.clone()))));

        host.status = Some(HostStatus { heartbeat_at: Some(Utc::now()), ..Default::default() });
        assert!(!aggregator.apply_host_event(ReadWatchEvent::Modified(read(host.clone()))), "heartbeat");

        host.spec.connection = HostConnection::SshHost { owning_daemon: "host-feta".into(), destination: "b1.lan".into() };
        assert!(aggregator.apply_host_event(ReadWatchEvent::Modified(read(host.clone()))), "new owning daemon");
        assert_eq!(aggregator.acting_daemon("host-b1"), Some(HostName::new("host-feta")));

        assert!(aggregator.apply_host_event(ReadWatchEvent::Deleted(read(host))));
        assert_eq!(aggregator.acting_daemon("host-b1"), None);
    }

    #[tokio::test]
    async fn non_terminal_work_is_completable() {
        for phase in [ResourceWorkPhase::Pending, ResourceWorkPhase::Ready, ResourceWorkPhase::Launching, ResourceWorkPhase::Running] {
            let vessel = emitted_vessel(convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(phase).call()).await;

            assert!(vessel.complete_work, "{phase:?} work must expose the completion override");
        }
    }

    // #742: each origin retains its checkout rows independently. Replayed
    // updates and deletes cannot remove a same-named checkout on another host.
    #[hegel::test]
    fn checkout_replica_events_preserve_origin_identity(tc: hegel::TestCase) {
        use hegel::generators as gs;
        // Two colliding origins; short sequences include repeated adds, edits,
        // deletes, and empty remote views. The local row always survives.
        let operations = (0..tc.draw(gs::integers::<usize>().min_value(1).max_value(12)))
            .map(|_| {
                (tc.draw(gs::integers::<usize>().min_value(0).max_value(1)), tc.draw(gs::integers::<usize>().min_value(0).max_value(2)))
            })
            .collect::<Vec<_>>();
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let state = AggregatorProjectionState::new();
            let (event_tx, _) = broadcast::channel(128);
            let resolver = CountingAttachResolver {
                calls: AtomicUsize::new(0),
                origin_hosts: HashMap::from([
                    (flotilla_protocol::NodeId::new("origin-0"), HostName::new("remote-0")),
                    (flotilla_protocol::NodeId::new("origin-1"), HostName::new("remote-1")),
                ]),
            };
            let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attach_resolver(Arc::new(resolver));
            let object = checkout_object("same-name", "/srv/widgets", RepositoryKey("widgets".into())).await;
            discover_checkouts(&mut aggregator, std::slice::from_ref(&object)).await;
            let mut expected = [None, None];
            for (origin, op) in operations {
                let branch = format!("branch-{op}");
                expected[origin] = (op != 2).then_some(branch);
                let adopted = expected
                    .iter()
                    .enumerate()
                    .filter_map(|(origin, branch)| {
                        let mut updated = object.clone();
                        if let CheckoutSpec::Observed(spec) = &mut updated.spec {
                            spec.r#ref = branch.clone()?;
                        }
                        Some(adopted_record(&format!("origin-{origin}"), &updated))
                    })
                    .collect();
                hold_engine_checkouts(&mut aggregator, "local-home", adopted).await;
                let set = state.result_set_for(&QueryId::Checkouts { scope: None }).await.expect("checkout query");
                let rows = set.rows.as_checkouts().expect("checkout rows");
                assert_eq!(rows.len(), 1 + expected.iter().filter(|entry| entry.is_some()).count());
                assert!(rows.iter().any(|row| row.host == HostName::new("local") && row.branch == "main"));
                for (origin, branch) in expected.iter().enumerate() {
                    let row = rows.iter().find(|row| row.host == HostName::new(format!("remote-{origin}")));
                    assert_eq!(row.map(|row| &row.branch), branch.as_ref());
                }
                assert!(rows.iter().all(|row| row.resource.host.as_ref() == Some(&row.host)));
            }
        });
    }

    #[tokio::test]
    async fn restart_relist_removes_local_rows_missing_from_stores() {
        let durable = ResourceBackend::new(InMemoryBackend::default());
        let state = AggregatorProjectionState::new();
        let stale_ref = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "deleted-during-outage");
        state.write().await.local_rows.insert(
            stale_ref.clone(),
            ConvoyRow::builder()
                .resource(stale_ref)
                .name("deleted-during-outage")
                .workflow_ref("scratch")
                .phase(ConvoyPhase::Active)
                .build(),
        );
        let (event_tx, mut event_rx) = broadcast::channel(8);

        let task = tokio::spawn(
            Aggregator::new(state.clone(), HostName::new("local"), event_tx).run(
                AggregatorResolvers::builder()
                    .durable_convoys(durable.including_replicas::<Convoy>("flotilla"))
                    .durable_convoy_ensures(durable.including_replicas::<ConvoyEnsure>("flotilla"))
                    .durable_sessions(durable.including_replicas::<TerminalSession>("flotilla"))
                    .durable_projects(durable.including_replicas::<Project>("flotilla"))
                    .durable_repositories(durable.including_replicas::<Repository>("flotilla"))
                    .durable_vessels(durable.including_replicas::<Vessel>("flotilla"))
                    .durable_hosts(durable.including_replicas::<flotilla_resources::Host>("flotilla"))
                    .build(),
            ),
        );

        let DaemonEvent::ResultSet(result_set) =
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv()).await.expect("relist timeout").expect("relist snapshot")
        else {
            panic!("expected relist result set");
        };
        assert!(result_set.rows.is_empty());
        assert!(state.result_set().await.rows.is_empty());
        assert!(!task.is_finished(), "the watch remains live after bootstrap");
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn expired_convoy_watch_relists_its_source_and_removes_missed_deletion() {
        let stale = convoy_object("deleted-while-watch-expired").await;
        let durable_convoys = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![stale], resource_version: "1".to_string(), generation: None }, empty_list()],
            vec![Ok(expiring_watch()), Ok(pending_watch())],
        ));
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);

        let run_durable_convoys = Arc::clone(&durable_convoys);
        let run_state = state.clone();
        let task = tokio::spawn(async move {
            run_with_test_sources(Aggregator::new(run_state, HostName::new("local"), event_tx), run_durable_convoys.as_ref()).await
        });

        let initial = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "initial result set timeout").await;
        let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial result set") };
        assert_eq!(convoy_names(&initial.rows), vec!["deleted-while-watch-expired"]);

        let removal = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "relist delta timeout").await;
        let DaemonEvent::ResultDelta(removal) = removal else { panic!("expected relist delta") };
        let removed = removal.changes.removed_resources().expect("convoy removals");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "deleted-while-watch-expired");
        assert!(state.result_set().await.rows.is_empty());
        assert_eq!(durable_convoys.list_calls.load(Ordering::SeqCst), 2);
        assert_eq!(durable_convoys.watch_calls.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished(), "aggregator should remain alive after in-place relist");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn expired_session_watch_relists_its_source_and_removes_missed_deletion() {
        let stale = session_object("deleted-while-watch-expired").await;
        let durable_convoys = Arc::new(ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_sessions = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![stale], resource_version: "1".to_string(), generation: None }, empty_list()],
            vec![Ok(expiring_watch()), Ok(pending_watch())],
        ));
        let durable_projects = Arc::new(ScriptedSource::<Project>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_convoy_ensures = Arc::new(ScriptedSource::<ConvoyEnsure>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_repositories = Arc::new(ScriptedSource::<Repository>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_vessels = ScriptedSource::<Vessel>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_hosts = ScriptedSource::<flotilla_resources::Host>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);

        let run_state = state.clone();
        let run_durable_sessions = Arc::clone(&durable_sessions);
        let task = tokio::spawn(async move {
            let sources = AggregatorSourceRefs::builder()
                .durable_convoys(durable_convoys.as_ref())
                .durable_sessions(run_durable_sessions.as_ref())
                .durable_projects(durable_projects.as_ref())
                .durable_convoy_ensures(durable_convoy_ensures.as_ref())
                .durable_repositories(durable_repositories.as_ref())
                .durable_vessels(&durable_vessels)
                .durable_hosts(&durable_hosts)
                .build();
            Aggregator::new(run_state, HostName::new("local"), event_tx).run_with_sources(sources).await
        });

        let initial =
            recv_query_event(&mut event_rx, QueryId::Independents { scope: None }, "initial independents result set timeout").await;
        let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial independents result set") };
        assert_eq!(initial.rows.as_independents().expect("independent rows")[0].name, "deleted-while-watch-expired");

        let removal = recv_query_event(&mut event_rx, QueryId::Independents { scope: None }, "independents relist delta timeout").await;
        let DaemonEvent::ResultDelta(removal) = removal else { panic!("expected independents relist delta") };
        let removed = removal.changes.removed_resources().expect("independent removals");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "deleted-while-watch-expired");
        assert!(state.independents_result_set(&None).await.rows.is_empty());
        assert_eq!(durable_sessions.list_calls.load(Ordering::SeqCst), 2);
        assert_eq!(durable_sessions.watch_calls.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished(), "aggregator should remain alive after session relist");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn expiry_between_list_and_watch_relists_before_publishing_source_snapshot() {
        let stale = convoy_object("deleted-before-watch-started").await;
        let durable_convoys = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![stale], resource_version: "1".to_string(), generation: None }, empty_list()],
            vec![
                Err(ResourceError::WatchExpired { requested_version: "1".to_string(), compacted_through: Some("2".to_string()) }),
                Ok(pending_watch()),
            ],
        ));
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);

        let run_durable_convoys = Arc::clone(&durable_convoys);
        let run_state = state.clone();
        let task = tokio::spawn(async move {
            run_with_test_sources(Aggregator::new(run_state, HostName::new("local"), event_tx), run_durable_convoys.as_ref()).await
        });

        let initial = recv_query_event(&mut event_rx, QueryId::Convoys { scope: None }, "initial result set timeout").await;
        let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial result set") };
        assert!(initial.rows.is_empty(), "failed watch attempt must not publish its stale list");
        assert!(state.result_set().await.rows.is_empty());
        assert_eq!(durable_convoys.list_calls.load(Ordering::SeqCst), 2);
        assert_eq!(durable_convoys.watch_calls.load(Ordering::SeqCst), 2);
        assert!(!task.is_finished(), "aggregator should remain alive after startup relist");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn non_expiry_watch_error_still_exits_aggregator() {
        let durable_convoys = ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(failing_watch("convoy watch failed"))]);
        let state = AggregatorProjectionState::new();
        let (event_tx, _event_rx) = broadcast::channel(8);

        let result = timeout(
            Duration::from_secs(1),
            run_with_test_sources(Aggregator::new(state, HostName::new("local"), event_tx), &durable_convoys),
        )
        .await
        .expect("aggregator should return the watch error")
        .expect_err("non-expiry watch error should reach supervision");

        assert_eq!(result, ResourceError::other("convoy watch failed"));
    }

    #[test]
    fn terminal_convoy_without_workflow_snapshot_is_not_initializing() {
        let status =
            ConvoyStatus { phase: ResourceConvoyPhase::Failed, message: Some("missing input 'topic'".into()), ..Default::default() };

        assert!(!convoy_is_initializing(Some(&status)));
    }
    async fn settle_attach_refresh(aggregator: &mut Aggregator) {
        timeout(Duration::from_secs(1), async {
            while let Some(result) = aggregator.attach_refresh_tasks.join_next().await {
                if let Ok(resolution) = result {
                    aggregator.apply_attach_capability_resolution(resolution).await.expect("apply capability result");
                }
            }
        })
        .await
        .expect("capability refresh finishes");
    }

    // Provider-boundary stand-in: terminal liveness can fail or return an invalid batch.
    struct ScriptedAttachResult(Mutex<Result<Vec<bool>, String>>);

    #[async_trait]
    impl AttachCapabilityResolver for ScriptedAttachResult {
        async fn resolvable_attach_targets(&self, _: &[(String, HostName)]) -> Result<Vec<bool>, String> {
            self.0.lock().await.clone()
        }
    }

    // A resolver installed after sessions were observed must verify them on the
    // next refresh. Replacement with identical attach inputs keeps that verified
    // capability: resource metadata is not part of the terminal's attach target.
    #[tokio::test]
    async fn late_attach_resolver_and_identical_target_replacement() {
        let state = AggregatorProjectionState::new();
        let (tx, _) = broadcast::channel(64);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);
        aggregator.replace_session_source(LocalSource::Durable, vec![session_object("same-target").await]).await;
        assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_none());
        let resolver = Arc::new(CountingAttachResolver::new());
        aggregator = aggregator.with_attach_resolver(resolver.clone());
        aggregator.rebuild_independents_projection().await;
        settle_attach_refresh(&mut aggregator).await;
        assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_some());
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        let mut replacement = session_object("same-target").await;
        replacement.metadata.resource_version = "replacement-version".into();
        aggregator.replace_session_source(LocalSource::Durable, vec![replacement]).await;
        settle_attach_refresh(&mut aggregator).await;
        assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_some());
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    // Ordinary running-status updates must preserve a verified opening action
    // and must not schedule another provider check.
    #[tokio::test]
    async fn status_churn_preserves_attach_action() {
        let state = AggregatorProjectionState::new();
        let (tx, _) = broadcast::channel(64);
        let resolver = Arc::new(ScriptedAttachResult(Mutex::new(Ok(vec![true]))));
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx).with_attach_resolver(resolver);
        let mut session = session_object("busy").await;
        aggregator.replace_session_source(LocalSource::Durable, vec![session.clone()]).await;
        settle_attach_refresh(&mut aggregator).await;
        session.status.as_mut().unwrap().message = Some("working".into());
        aggregator.replace_session_source(LocalSource::Durable, vec![session.clone()]).await;
        assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_some());
        assert!(aggregator.attach_refresh_tasks.is_empty());
        session.status.as_mut().unwrap().phase = TerminalSessionPhase::Stopped;
        aggregator.replace_session_source(LocalSource::Durable, vec![session]).await;
        assert!(aggregator.attachable_sessions.is_empty());
        assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap().iter().all(|row| row.attach.is_none()));
    }

    // Only current attach inputs may change a capability. Generate response,
    // deletion/recreation and host/endpoint changes, checking pending and settled
    // projections at every step; metadata versions deliberately do not matter.
    #[hegel::test]
    fn capability_results_do_not_restore_superseded_sessions(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let steps = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
        let ops = (0..steps).map(|_| (tc.draw(gs::booleans()), tc.draw(gs::booleans()), tc.draw(gs::booleans()))).collect::<Vec<_>>();
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime").block_on(async {
            let state = AggregatorProjectionState::new();
            let (tx, _) = broadcast::channel(64);
            let resolver = Arc::new(ScriptedAttachResult(Mutex::new(Ok(vec![true]))));
            let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx).with_attach_resolver(resolver.clone());
            for (step, (recreate, change_host, answer)) in ops.into_iter().enumerate() {
                *resolver.0.lock().await = Ok(vec![true]);
                let mut session = session_object("changing-session").await;
                aggregator.replace_session_source(LocalSource::Durable, vec![session.clone()]).await;
                settle_attach_refresh(&mut aggregator).await;
                assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_some());

                session.status.as_mut().unwrap().session_id = Some(format!("{step}-pending"));
                aggregator.replace_session_source(LocalSource::Durable, vec![session.clone()]).await;
                let old = aggregator.attach_refresh_tasks.join_next().await.unwrap().unwrap();
                *resolver.0.lock().await = Ok(vec![answer]);
                if change_host {
                    aggregator.local_host = HostName::new(format!("host-{step}"));
                } else {
                    session.status.as_mut().unwrap().session_id = Some(format!("{step}-current"));
                }
                // Removal invalidates even a completed, unapplied check. Re-add
                // with the same target to cover the ABA deletion race as well.
                aggregator.replace_session_source(LocalSource::Durable, vec![]).await;
                if recreate {
                    aggregator.replace_session_source(LocalSource::Durable, vec![session]).await;
                }
                aggregator.apply_attach_capability_resolution(old).await.unwrap();
                let pending = state.independents_result_set(&None).await;
                assert_eq!(pending.rows.len(), usize::from(recreate));
                assert!(pending.rows.as_independents().unwrap().iter().all(|row| row.attach.is_none()));
                settle_attach_refresh(&mut aggregator).await;
                let after = state.independents_result_set(&None).await;
                assert_eq!(after.rows.len(), usize::from(recreate));
                assert!(after.rows.as_independents().unwrap().iter().all(|row| row.attach.is_some() == answer));
            }
        });
    }

    type AttachReply = tokio::sync::oneshot::Sender<Result<Vec<bool>, String>>;

    // Provider I/O boundary: each individual lookup is explicitly completed by
    // the test, exposing cancellation, retry timing and concurrent interleavings.
    struct ControlledAttachResolver(mpsc::UnboundedSender<(String, HostName, AttachReply)>);

    #[async_trait]
    impl AttachCapabilityResolver for ControlledAttachResolver {
        async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
            assert_eq!(targets.len(), 1);
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.0.send((targets[0].0.clone(), targets[0].1.clone(), tx)).unwrap();
            rx.await.unwrap()
        }
    }

    async fn apply_next_attach(aggregator: &mut Aggregator) {
        loop {
            if let Ok(result) = aggregator.attach_refresh_tasks.join_next().await.unwrap() {
                aggregator.apply_attach_capability_resolution(result).await.unwrap();
                break;
            }
        }
    }

    // A target change retains the last verified action until a current negative
    // answer. Errors and malformed batches preserve it and retry with backoff.
    #[tokio::test(start_paused = true)]
    async fn attach_target_changes_retry_without_withdrawing_known_capability() {
        for mode in 0..4 {
            let state = AggregatorProjectionState::new();
            let (tx, _) = broadcast::channel(64);
            let (requests, mut rx) = mpsc::unbounded_channel();
            let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx)
                .with_attach_resolver(Arc::new(ControlledAttachResolver(requests)));
            let mut session = session_object("changing").await;
            aggregator.replace_session_source(LocalSource::Durable, vec![session.clone()]).await;
            rx.recv().await.unwrap().2.send(Ok(vec![true])).unwrap();
            apply_next_attach(&mut aggregator).await;
            match mode {
                0 => aggregator.local_host = HostName::new("new-host"),
                1 => session.status.as_mut().unwrap().session_id = Some("new-target".into()),
                2 => {
                    session.status.as_mut().unwrap().cleat_endpoint = Some(flotilla_protocol::result_set::CleatEndpoint {
                        runtime_root: "/run/new".into(),
                        daemon: "daemon".into(),
                        session: "new-target".into(),
                    })
                }
                _ => session.spec.pool = "new-pool".into(),
            }
            aggregator.replace_session_source(LocalSource::Durable, vec![session.clone()]).await;
            let (_, host, reply) = rx.recv().await.unwrap();
            if mode == 0 {
                assert_eq!(host, HostName::new("new-host"));
            }
            assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_some());
            let mut reply = reply;
            for (attempt, result) in [Err("unavailable".into()), Ok(vec![]), Ok(vec![true, false])].into_iter().enumerate() {
                let started = tokio::time::Instant::now();
                reply.send(result).unwrap();
                // Status churn must neither restart the retry nor clear capability.
                session.status.as_mut().unwrap().message = Some(format!("working-{attempt}"));
                aggregator.replace_session_source(LocalSource::Durable, vec![session.clone()]).await;
                reply = rx.recv().await.unwrap().2;
                assert_eq!(started.elapsed(), Duration::from_millis(250 * (1 << attempt)));
                assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_some());
            }
            reply.send(Ok(vec![false])).unwrap();
            apply_next_attach(&mut aggregator).await;
            assert!(state.independents_result_set(&None).await.rows.as_independents().unwrap()[0].attach.is_none());
            session.status.as_mut().unwrap().phase = TerminalSessionPhase::Stopped;
            aggregator.replace_session_source(LocalSource::Durable, vec![session]).await;
            assert!(aggregator.attachable_sessions.is_empty());
            assert!(rx.try_recv().is_err());
        }
    }

    // Churning one target must not cancel an unrelated in-flight lookup. A
    // superseded success must not overwrite the newer target's negative answer.
    #[tokio::test(start_paused = true)]
    async fn attach_checks_are_isolated_and_superseded_results_are_ignored() {
        let state = AggregatorProjectionState::new();
        let (tx, _) = broadcast::channel(64);
        let (requests, mut rx) = mpsc::unbounded_channel();
        let mut aggregator =
            Aggregator::new(state.clone(), HostName::new("local"), tx).with_attach_resolver(Arc::new(ControlledAttachResolver(requests)));
        let mut busy = session_object("busy").await;
        let quiet = session_object("quiet").await;
        aggregator.replace_session_source(LocalSource::Durable, vec![busy.clone(), quiet.clone()]).await;
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        let (busy_reply, quiet_reply) = if first.0 == "busy" { (first.2, second.2) } else { (second.2, first.2) };
        busy_reply.send(Ok(vec![true])).unwrap();
        let old = aggregator.attach_refresh_tasks.join_next().await.unwrap().unwrap();
        for step in 0..3 {
            busy.status.as_mut().unwrap().session_id = Some(format!("target-{step}"));
            aggregator.replace_session_source(LocalSource::Durable, vec![busy.clone(), quiet.clone()]).await;
            let request = rx.recv().await.unwrap();
            assert_eq!(request.0, "busy");
            if step == 2 {
                request.2.send(Err("busy target unavailable".into())).unwrap();
            }
        }
        quiet_reply.send(Ok(vec![true])).unwrap();
        apply_next_attach(&mut aggregator).await;
        let pending = state.independents_result_set(&None).await;
        assert!(pending.rows.as_independents().unwrap().iter().find(|row| row.resource.name == "quiet").unwrap().attach.is_some());
        let retry = rx.recv().await.unwrap();
        assert_eq!(retry.0, "busy");
        retry.2.send(Ok(vec![false])).unwrap();
        settle_attach_refresh(&mut aggregator).await;
        aggregator.apply_attach_capability_resolution(old).await.unwrap();
        let rows = state.independents_result_set(&None).await;
        for row in rows.rows.as_independents().unwrap() {
            assert_eq!(row.attach.is_some(), row.resource.name == "quiet");
        }
    }

    // A held provider check belongs to the aggregator lifetime, including an
    // aborted run. Dropping the aggregator must release the resolver task.
    #[tokio::test]
    async fn dropping_aggregator_cancels_held_capability_check() {
        let (entered, mut entered_rx) = mpsc::unbounded_channel();
        let resolver = Arc::new(HeldAttachResolver { entered, release: tokio::sync::Semaphore::new(0) });
        let state = AggregatorProjectionState::new();
        let (tx, _) = broadcast::channel(16);
        let mut aggregator = Aggregator::new(state, HostName::new("local"), tx).with_attach_resolver(resolver.clone());
        aggregator.replace_session_source(LocalSource::Durable, vec![session_object("held-session").await]).await;
        timeout(Duration::from_secs(1), entered_rx.recv()).await.expect("check starts").expect("signal");
        drop(aggregator);
        timeout(Duration::from_secs(1), async {
            while Arc::strong_count(&resolver) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("held provider check is canceled on drop");
    }

    fn resource_resolvers(d: &ResourceBackend) -> AggregatorResolvers {
        AggregatorResolvers::builder()
            .durable_convoys(d.including_replicas::<Convoy>("flotilla"))
            .durable_convoy_ensures(d.including_replicas::<ConvoyEnsure>("flotilla"))
            .durable_sessions(d.including_replicas::<TerminalSession>("flotilla"))
            .durable_projects(d.including_replicas::<Project>("flotilla"))
            .durable_repositories(d.including_replicas::<Repository>("flotilla"))
            .durable_vessels(d.including_replicas::<Vessel>("flotilla"))
            .durable_hosts(d.including_replicas::<flotilla_resources::Host>("flotilla"))
            .build()
    }

    // Boundary stand-in: terminal/provider capability lookup can await I/O.
    struct HeldAttachResolver {
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: tokio::sync::Semaphore,
    }

    #[async_trait]
    impl AttachCapabilityResolver for HeldAttachResolver {
        async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
            if !targets.is_empty() {
                self.entered.send(()).expect("probe observer");
                self.release.acquire().await.expect("probe release").forget();
            }
            Ok(vec![true; targets.len()])
        }
    }

    // Local row publication should remain prompt while an unrelated terminal
    // capability lookup is waiting. A provider gate makes the ordering exact.
    #[tokio::test]
    async fn local_convoy_publishes_while_attach_check_is_held() {
        for sqlite in [false, true] {
            let temp = tempfile::TempDir::new().expect("tempdir");
            let durable = if sqlite {
                ResourceBackend::new(flotilla_store_sqlite::SqliteBackend::open(temp.path().join("gate.sqlite")).expect("sqlite"))
            } else {
                ResourceBackend::new(InMemoryBackend::default())
            };
            let sessions = durable.using::<TerminalSession>("flotilla");
            let session = session_object("terminal-probe").await;
            let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
            let gate = Arc::new(HeldAttachResolver { entered: entered_tx, release: tokio::sync::Semaphore::new(0) });
            let state = AggregatorProjectionState::new();
            let (tx, mut rx) = broadcast::channel(4096);
            let aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx).with_attach_resolver(gate.clone());
            let resolvers = resource_resolvers(&durable);
            let task = tokio::spawn(aggregator.run(resolvers));
            timeout(Duration::from_secs(1), async {
                loop {
                    if let DaemonEvent::ResultSet(set) = rx.recv().await.expect("startup event") {
                        if set.query() == (QueryId::Convoys { scope: None }) {
                            break;
                        }
                    }
                }
            })
            .await
            .expect("startup");
            let created = sessions
                .create(&InputMeta::builder().name("terminal-probe".to_string()).build(), &session.spec)
                .await
                .expect("trigger probe");
            sessions
                .update_status(&created.metadata.name, &created.metadata.resource_version, session.status.as_ref().expect("status"))
                .await
                .expect("running terminal");
            timeout(Duration::from_secs(1), entered_rx.recv()).await.expect("probe entered").expect("probe signal");
            let convoys = durable.using::<Convoy>("flotilla");
            let mut watch = convoys.watch(WatchStart::Now).await.expect("watch");
            let start = std::time::Instant::now();
            convoys
                .create(
                    &InputMeta::builder().name("prompt-local".to_string()).build(),
                    &flotilla_resources::ConvoySpec::builder().workflow_ref("latency".to_string()).build(),
                )
                .await
                .expect("local write");
            let committed = std::time::Instant::now();
            watch.next().await.expect("watch event").expect("watch success");
            let watch_us = committed.elapsed().as_micros();
            let prompt = timeout(Duration::from_millis(200), async {
                loop {
                    if !state.local_result_set().await.rows.is_empty() {
                        break;
                    }
                    rx.recv().await.expect("projection event");
                }
            })
            .await
            .is_ok();
            let held_us = committed.elapsed().as_micros();
            gate.release.add_permits(16);
            let released = std::time::Instant::now();
            timeout(Duration::from_secs(1), async {
                loop {
                    if !state.local_result_set().await.rows.is_empty() {
                        break;
                    }
                    rx.recv().await.expect("projection event");
                }
            })
            .await
            .expect("projection catches up after probe release");
            eprintln!(
            "HELD_CHECK_PROJECTION sqlite={} write_us={} direct_watch_us={} held_us={} projected_before_probe_release={} projection_after_release_us={}",
                sqlite,
            committed.duration_since(start).as_micros(),
            watch_us,
            held_us,
            prompt,
            released.elapsed().as_micros()
        );
            timeout(Duration::from_secs(1), async {
                loop {
                    let rows = state.independents_result_set(&None).await;
                    if rows.rows.as_independents().expect("terminal rows").iter().any(|row| row.attach.is_some()) {
                        break;
                    }
                    rx.recv().await.expect("capability event");
                }
            })
            .await
            .expect("capabilities apply after release");
            task.abort();
            let _ = task.await;
            assert!(prompt, "local convoy projection was blocked by unrelated attach capability lookup");
        }
    }
}

#[cfg(test)]
mod project_parent_tests {
    use flotilla_resources::{FleetDesignation, FleetDesignationSpec, InputMeta, ProjectSpec};
    use flotilla_store::ResourceBackend;

    use super::*;

    // #2718: a fleet designation watch projects implicit parents, and an
    // explicit intermediate Project is published using the shared resolver.
    #[tokio::test]
    async fn designation_watch_rebuilds_project_parents() {
        let backend = ResourceBackend::new(InMemoryBackend::default());
        for (name, parent) in [("root", None), ("group", None), ("child", Some("group"))] {
            backend
                .definitions::<Project>("flotilla")
                .apply(
                    &InputMeta::builder().name(name.into()).build(),
                    &ProjectSpec::builder()
                        .display_name(name.into())
                        .default_workflow_ref("work".into())
                        .maybe_parent(parent.map(str::to_string))
                        .build(),
                )
                .await
                .expect("project");
        }
        backend
            .definitions::<FleetDesignation>("flotilla")
            .apply(
                &InputMeta::builder().name("fleet".into()).build(),
                &FleetDesignationSpec { fleet: None, project: "root".into(), image_cache: None },
            )
            .await
            .expect("designation");
        let state = AggregatorProjectionState::new();
        let (tx, _) = broadcast::channel(32);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);
        let _project_watch = aggregator.recover_project_watch(&backend.including_replicas::<Project>("flotilla")).await.expect("projects");
        let _fleet_watch = aggregator
            .recover_fleet_designation_watch(&backend.including_replicas::<FleetDesignation>("flotilla"))
            .await
            .expect("designation watch");
        let query = QueryId::ProjectRepositories { scope: None };
        let set = state.result_set_for(&query).await.expect("result set");
        let rows = set.rows.as_project_repositories().expect("rows");
        let root = rows.iter().find(|row| row.resource.name == "root").expect("root");
        let group = rows.iter().find(|row| row.resource.name == "group").expect("group");
        let child = rows.iter().find(|row| row.resource.name == "child").expect("child");
        assert!(root.parent.is_none());
        assert_eq!(group.parent.as_ref().map(|parent| parent.name.as_str()), Some("root"));
        assert_eq!(child.parent.as_ref().map(|parent| parent.name.as_str()), Some("group"));
    }
}

#[cfg(test)]
mod hold_attention_tests {
    use chrono::Utc;

    use super::*;

    fn refusal_delivery(attention: &flotilla_resources::ConvoyAttention, explicit_hold: bool) -> flotilla_resources::TurnDeliveryStatus {
        use flotilla_resources::{TurnDeliveryEpisode, TurnDeliveryOutcome, TurnDeliveryStatus};
        let now = attention.raised_at;
        TurnDeliveryStatus {
            hold: explicit_hold.then_some(attention.clone()),
            episodes: (0..3)
                .map(|revision| {
                    TurnDeliveryEpisode::builder()
                        .subject_revision(revision.to_string())
                        .evidence_at(now)
                        .judged_claim_at(now)
                        .outcome(TurnDeliveryOutcome::Refused {
                            reason: "three consecutive refusals".into(),
                            refused_at: now,
                            hold_executed: true,
                        })
                        .build()
                })
                .collect(),
            pending_supervisor_turn: Some(flotilla_resources::PendingSupervisorTurn {
                queued_order: 0,
                vessel: "work".into(),
                role: "governor".into(),
                message: flotilla_resources::TerminalCrewMessage {
                    id: "supervise".into(),
                    text: "Supervise".into(),
                    sender: Default::default(),
                    delivery: flotilla_resources::CrewMessageDelivery::Queued,
                    acknowledged: Default::default(),
                    following: Vec::new(),
                },
            }),
            ..Default::default()
        }
    }

    // #3079: terminal rows ignore delivery attention while preserving unrelated
    // attention and durable refusal history. Exhaust all terminal and non-terminal phases,
    // explicit/legacy holds, and shared delivery/unrelated/empty attention slots.
    #[tokio::test]
    async fn terminal_delivery_attention_projection() {
        use flotilla_resources::ConvoyAttention;
        for phase in [
            ResourceConvoyPhase::Landed,
            ResourceConvoyPhase::Failed,
            ResourceConvoyPhase::Abandoned,
            ResourceConvoyPhase::Pending,
            ResourceConvoyPhase::Active,
            ResourceConvoyPhase::Interrupted,
            ResourceConvoyPhase::Landing,
        ] {
            for explicit_hold in [false, true] {
                for source in [
                    None,
                    Some("actionable-review"),
                    Some(ConvoyAttention::QUEUED_TURN_SOURCE),
                    Some(ConvoyAttention::MISSING_TURN_HOOK_SOURCE),
                    Some(ConvoyAttention::SUPERVISOR_TURN_SOURCE),
                    Some("settlement"),
                ] {
                    let state = AggregatorProjectionState::new();
                    let (event_tx, _) = broadcast::channel(8);
                    let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
                    let mut convoy = super::tests::convoy_with_vessel("held").await;
                    let status = convoy.status.as_mut().expect("status");
                    status.phase = phase;
                    let now = Utc::now();
                    let attention =
                        ConvoyAttention { source: "actionable-review".into(), reason: "three consecutive refusals".into(), raised_at: now };
                    status.attention = source.map(|source| ConvoyAttention { source: source.into(), ..attention.clone() });
                    let delivery = refusal_delivery(&attention, explicit_hold);
                    status.turn_deliveries.insert("actionable-review".into(), delivery.clone());
                    aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
                    let result = state.result_set().await;
                    let row = &result.rows.as_convoys().expect("convoys")[0];
                    assert_eq!(
                        row.surface_state == SurfaceState::NeedsYou,
                        !phase.is_terminal() || source == Some("settlement"),
                        "{phase:?}, explicit={explicit_hold}, source={source:?}"
                    );
                    assert_eq!(row.phase, Some(convoy_phase(phase)));
                    if phase.is_terminal() && source != Some("settlement") {
                        assert!(row.message.is_none(), "stale delivery reason must not label the terminal row");
                    }
                    let stored = aggregator.convoys.values().next().expect("convoy");
                    assert_eq!(stored.object.status.as_ref().unwrap().turn_deliveries["actionable-review"], delivery);
                }
            }
        }
    }

    // #2758: persistent holds remain in attention views even if an unrelated
    // reconciler clears the shared attention slot; the reason survives too.
    #[tokio::test]
    async fn persisted_hold_survives_attention_slot_clear() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        let mut convoy = super::tests::convoy_with_vessel("held").await;
        let status = convoy.status.as_mut().expect("status");
        status.attention = None;
        status.turn_deliveries.insert(
            "checks-settled".into(),
            flotilla_resources::TurnDeliveryStatus {
                hold: Some(flotilla_resources::ConvoyAttention {
                    source: "checks-settled".into(),
                    reason: "episode limit".into(),
                    raised_at: Utc::now(),
                }),
                ..Default::default()
            },
        );
        aggregator.apply_convoy_event(WatchEvent::Added(convoy)).await;
        let result = state.result_set().await;
        let row = result.rows.as_convoys().expect("convoys").iter().find(|row| row.name == "held").expect("held row");
        assert_eq!(row.surface_state, SurfaceState::NeedsYou);
        assert_eq!(row.message.as_deref(), Some("episode limit"));
    }
}
