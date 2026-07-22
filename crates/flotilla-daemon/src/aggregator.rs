//! Resource-store and fleet-replica Aggregator maintaining named-query result sets.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use flotilla_core::{aggregator_projection::AggregatorProjectionState, in_process::InProcessDaemon};
use flotilla_protocol::{
    result_set::{
        CheckoutRow, ConvoyChangeRequest, ConvoyPhase, ConvoyRow, CrewMemberSummary, IndependentRow, QueryChanges, QueryId, QueryScope,
        ResultDelta, Rows, SessionPhase, VesselRow, WorkPhase,
    },
    Change, DaemonEvent, EntryOp, FleetReplicaSnapshot, HostName, LifecycleAuthority, ProviderData, RepoDelta, RepoIdentity, RepoSnapshot,
    RepositoryKey, ResourceRef,
};
use flotilla_resources::{
    api_version, Checkout, CheckoutSpec, Convoy, ConvoyPhase as ResourceConvoyPhase, ConvoyStatus, CrewSource, Environment, Presentation,
    Project, Repository, Resource, ResourceError, ResourceList, ResourceObject, TerminalAttentionState, TerminalSession,
    TerminalSessionPhase, TypedResolver, VesselRequirement, WatchEvent, WatchStart, WatchStream, WorkPhase as ResourceWorkPhase, WorkState,
    CONVOY_LABEL, REPO_KEY_LABEL, REPO_LABEL, VESSEL_LABEL,
};
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc};

use crate::issue_materializer::{IssueMaterializationResolver, IssueMaterializer};

type PresentationKey = (String, String, String);
type SessionKey = (String, String);
type ChangeRequestFingerprint = HashMap<String, (String, String)>;

#[derive(bon::Builder)]
pub struct AggregatorResolvers {
    durable_convoys: TypedResolver<Convoy>,
    durable_environments: TypedResolver<Environment>,
    durable_presentations: TypedResolver<Presentation>,
    durable_sessions: TypedResolver<TerminalSession>,
    durable_projects: TypedResolver<Project>,
    durable_repositories: TypedResolver<Repository>,
    observed_convoys: TypedResolver<Convoy>,
    observed_presentations: TypedResolver<Presentation>,
    observed_sessions: TypedResolver<TerminalSession>,
    observed_checkouts: TypedResolver<Checkout>,
}

#[derive(bon::Builder)]
struct AggregatorSourceRefs<'a> {
    durable_convoys: &'a dyn AggregatorWatchSource<Convoy>,
    durable_environments: &'a dyn AggregatorWatchSource<Environment>,
    durable_presentations: &'a dyn AggregatorWatchSource<Presentation>,
    durable_sessions: &'a dyn AggregatorWatchSource<TerminalSession>,
    durable_projects: &'a dyn AggregatorWatchSource<Project>,
    durable_repositories: &'a dyn AggregatorWatchSource<Repository>,
    observed_convoys: &'a dyn AggregatorWatchSource<Convoy>,
    observed_presentations: &'a dyn AggregatorWatchSource<Presentation>,
    observed_sessions: &'a dyn AggregatorWatchSource<TerminalSession>,
    observed_checkouts: &'a dyn AggregatorWatchSource<Checkout>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum LocalSource {
    Durable,
    Observed,
}

const LOCAL_SOURCE_PRECEDENCE: [LocalSource; 2] = [LocalSource::Durable, LocalSource::Observed];

#[async_trait]
trait AggregatorWatchSource<T: Resource>: Send + Sync {
    async fn list(&self) -> Result<ResourceList<T>, ResourceError>;
    async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError>;
}

#[async_trait]
impl<T: Resource> AggregatorWatchSource<T> for TypedResolver<T> {
    async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
        self.list().await
    }

    async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        self.watch(start).await
    }
}

#[async_trait]
pub(crate) trait AttachCapabilityResolver: Send + Sync {
    async fn resolvable_attach_references(&self, references: &[String]) -> Result<HashSet<String>, String>;
}

#[async_trait]
pub(crate) trait ConvoyChangeRequestResolver: Send + Sync {
    async fn resolve_change_request(&self, repositories: &[RepositoryKey], branch: &str) -> Result<Option<ConvoyChangeRequest>, String>;
}

#[async_trait]
impl ConvoyChangeRequestResolver for InProcessDaemon {
    async fn resolve_change_request(&self, repositories: &[RepositoryKey], branch: &str) -> Result<Option<ConvoyChangeRequest>, String> {
        self.resolve_convoy_change_request(repositories, branch).await
    }
}

#[async_trait]
impl AttachCapabilityResolver for InProcessDaemon {
    async fn resolvable_attach_references(&self, references: &[String]) -> Result<HashSet<String>, String> {
        self.resolvable_attach_references_internal(references).await
    }
}

#[derive(bon::Builder)]
pub struct Aggregator {
    state: AggregatorProjectionState,
    local_host: HostName,
    #[builder(skip)]
    convoys_by_source: HashMap<LocalSource, HashMap<ResourceRef, ResourceObject<Convoy>>>,
    #[builder(skip)]
    presentations_by_source: HashMap<LocalSource, HashMap<ResourceRef, ResourceObject<Presentation>>>,
    #[builder(skip)]
    sessions_by_source: HashMap<LocalSource, HashMap<SessionKey, ResourceObject<TerminalSession>>>,
    presentation_workspaces: HashMap<PresentationKey, String>,
    terminal_sessions: HashMap<SessionKey, ResourceObject<TerminalSession>>,
    projects: HashMap<(String, String), ResourceObject<Project>>,
    repositories: HashMap<RepositoryKey, ResourceObject<Repository>>,
    observed_checkouts: HashMap<ResourceRef, ResourceObject<Checkout>>,
    bootstrapping: bool,
    emitted_queries: HashSet<QueryId>,
    #[builder(skip)]
    attach_resolver: Option<Arc<dyn AttachCapabilityResolver>>,
    #[builder(skip)]
    change_request_resolver: Option<Arc<dyn ConvoyChangeRequestResolver>>,
    #[builder(skip)]
    convoy_change_requests: HashMap<ResourceRef, ConvoyChangeRequest>,
    #[builder(skip)]
    change_request_refresh_generations: HashMap<ResourceRef, u64>,
    #[builder(skip)]
    change_request_refresh_tasks: HashMap<ResourceRef, tokio::task::JoinHandle<()>>,
    #[builder(skip)]
    change_request_refresh_queue: ChangeRequestRefreshQueue,
    #[builder(skip)]
    repo_change_requests: HashMap<RepoIdentity, ChangeRequestFingerprint>,
    #[builder(skip)]
    issue_materializer: Option<IssueMaterializer>,
    event_tx: broadcast::Sender<DaemonEvent>,
}

struct ChangeRequestResolution {
    reference: ResourceRef,
    generation: u64,
    branch: String,
    result: Result<Option<ConvoyChangeRequest>, String>,
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
    const WATCH_RESTART_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

    pub fn new(state: AggregatorProjectionState, local_host: HostName, event_tx: broadcast::Sender<DaemonEvent>) -> Self {
        Self {
            state,
            local_host,
            convoys_by_source: HashMap::new(),
            presentations_by_source: HashMap::new(),
            sessions_by_source: HashMap::new(),
            presentation_workspaces: HashMap::new(),
            terminal_sessions: HashMap::new(),
            projects: HashMap::new(),
            repositories: HashMap::new(),
            observed_checkouts: HashMap::new(),
            bootstrapping: false,
            emitted_queries: HashSet::new(),
            attach_resolver: None,
            change_request_resolver: None,
            convoy_change_requests: HashMap::new(),
            change_request_refresh_generations: HashMap::new(),
            change_request_refresh_tasks: HashMap::new(),
            change_request_refresh_queue: ChangeRequestRefreshQueue::default(),
            repo_change_requests: HashMap::new(),
            issue_materializer: None,
            event_tx,
        }
    }

    pub(crate) fn with_attach_resolver<R>(mut self, resolver: Arc<R>) -> Self
    where
        R: AttachCapabilityResolver + 'static,
    {
        self.attach_resolver = Some(resolver);
        self
    }

    pub(crate) fn with_change_request_resolver<R>(mut self, resolver: Arc<R>) -> Self
    where
        R: ConvoyChangeRequestResolver + 'static,
    {
        self.change_request_resolver = Some(resolver);
        self
    }

    pub(crate) fn with_issue_resolver<R>(mut self, resolver: Arc<R>) -> Self
    where
        R: IssueMaterializationResolver + 'static,
    {
        self.issue_materializer = Some(IssueMaterializer::new(self.state.clone(), resolver, self.event_tx.clone()));
        self
    }

    pub async fn run(
        self,
        resolvers: AggregatorResolvers,
        replica_rx: broadcast::Receiver<Vec<FleetReplicaSnapshot>>,
    ) -> Result<(), ResourceError> {
        let AggregatorResolvers {
            durable_convoys,
            durable_environments,
            durable_presentations,
            durable_sessions,
            durable_projects,
            durable_repositories,
            observed_convoys,
            observed_presentations,
            observed_sessions,
            observed_checkouts,
        } = resolvers;
        let sources = AggregatorSourceRefs::builder()
            .durable_convoys(&durable_convoys)
            .durable_environments(&durable_environments)
            .durable_presentations(&durable_presentations)
            .durable_sessions(&durable_sessions)
            .durable_projects(&durable_projects)
            .durable_repositories(&durable_repositories)
            .observed_convoys(&observed_convoys)
            .observed_presentations(&observed_presentations)
            .observed_sessions(&observed_sessions)
            .observed_checkouts(&observed_checkouts)
            .build();
        self.run_with_sources(sources, replica_rx).await
    }

    async fn run_with_sources(
        mut self,
        sources: AggregatorSourceRefs<'_>,
        mut replica_rx: broadcast::Receiver<Vec<FleetReplicaSnapshot>>,
    ) -> Result<(), ResourceError> {
        let AggregatorSourceRefs {
            durable_convoys,
            durable_environments,
            durable_presentations,
            durable_sessions,
            durable_projects,
            durable_repositories,
            observed_convoys,
            observed_presentations,
            observed_sessions,
            observed_checkouts,
        } = sources;
        // Subscribe before any source bootstrap awaits so demand arriving
        // during recovery remains observable. Also consume the initial value
        // for demand that predates Aggregator startup; #747's source-backed
        // reconciler plugs into this same initial/change path.
        let mut demand_rx = self.state.subscribe_demand();
        let mut fetch_more_rx = self.state.subscribe_fetch_more();
        let mut daemon_event_rx = self.event_tx.subscribe();
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
        let mut durable_convoy_stream = self.recover_convoy_watch(LocalSource::Durable, durable_convoys).await?;
        let mut durable_environment_stream = self.recover_environment_watch(durable_environments).await?;
        let mut durable_presentation_stream = self.recover_presentation_watch(LocalSource::Durable, durable_presentations).await?;
        let mut durable_session_stream = self.recover_session_watch(LocalSource::Durable, durable_sessions).await?;
        let mut durable_project_stream = self.recover_project_watch(durable_projects).await?;
        let mut durable_repository_stream = self.recover_repository_watch(durable_repositories).await?;
        let mut observed_convoy_stream = self.recover_convoy_watch(LocalSource::Observed, observed_convoys).await?;
        let mut observed_presentation_stream = self.recover_presentation_watch(LocalSource::Observed, observed_presentations).await?;
        let mut observed_session_stream = self.recover_session_watch(LocalSource::Observed, observed_sessions).await?;
        let mut observed_checkout_stream = self.recover_checkout_watch(observed_checkouts).await?;
        self.bootstrapping = false;
        self.emitted_queries.extend(QueryId::ALWAYS_MATERIALIZED.iter().cloned());
        let _ = self.event_tx.send(DaemonEvent::ResultSet(Box::new(self.state.result_set().await)));
        let _ = self.event_tx.send(DaemonEvent::ResultSet(Box::new(self.state.independents_result_set(&None).await)));

        loop {
            tokio::select! {
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
                    Ok(DaemonEvent::RepoRefreshCompleted { .. }) => self.refresh_all_change_requests().await,
                    Ok(DaemonEvent::RepoSnapshot(snapshot)) => {
                        if self.repo_snapshot_changed_change_requests(&snapshot) {
                            self.refresh_all_change_requests().await;
                        }
                    }
                    Ok(DaemonEvent::RepoDelta(delta)) => {
                        if self.repo_delta_changed_change_requests(&delta) {
                            self.refresh_all_change_requests().await;
                        }
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
                resolution = self.change_request_refresh_queue.rx.recv() => {
                    if let Some(resolution) = resolution {
                        self.apply_change_request_resolution(resolution).await;
                    }
                },
                event = durable_convoy_stream.next() => match event {
                    Some(Ok(event)) => self.apply_convoy_event_from(LocalSource::Durable, event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_convoy_stream = self.recover_convoy_watch(LocalSource::Durable, durable_convoys).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable convoy watch ended")),
                },
                event = durable_environment_stream.next() => match event {
                    Some(Ok(event)) => self.apply_environment_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_environment_stream = self.recover_environment_watch(durable_environments).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable environment watch ended")),
                },
                event = durable_presentation_stream.next() => match event {
                    Some(Ok(event)) => self.apply_presentation_event_from(LocalSource::Durable, event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_presentation_stream = self.recover_presentation_watch(LocalSource::Durable, durable_presentations).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable presentation watch ended")),
                },
                event = durable_session_stream.next() => match event {
                    Some(Ok(event)) => self.apply_session_event_from(LocalSource::Durable, event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_session_stream = self.recover_session_watch(LocalSource::Durable, durable_sessions).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable terminal session watch ended")),
                },
                event = durable_project_stream.next() => match event {
                    Some(Ok(event)) => self.apply_project_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_project_stream = self.recover_project_watch(durable_projects).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable project watch ended")),
                },
                event = durable_repository_stream.next() => match event {
                    Some(Ok(event)) => self.apply_repository_event(event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        durable_repository_stream = self.recover_repository_watch(durable_repositories).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator durable repository watch ended")),
                },
                event = observed_convoy_stream.next() => match event {
                    Some(Ok(event)) => self.apply_convoy_event_from(LocalSource::Observed, event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        observed_convoy_stream = self.recover_convoy_watch(LocalSource::Observed, observed_convoys).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator observed convoy watch ended")),
                },
                event = observed_presentation_stream.next() => match event {
                    Some(Ok(event)) => self.apply_presentation_event_from(LocalSource::Observed, event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        observed_presentation_stream = self.recover_presentation_watch(LocalSource::Observed, observed_presentations).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator observed presentation watch ended")),
                },
                event = observed_session_stream.next() => match event {
                    Some(Ok(event)) => self.apply_session_event_from(LocalSource::Observed, event).await,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        observed_session_stream = self.recover_session_watch(LocalSource::Observed, observed_sessions).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator observed terminal session watch ended")),
                },
                event = observed_checkout_stream.next() => match event {
                    Some(Ok(event)) => self.apply_checkout_event(event).await?,
                    Some(Err(ResourceError::WatchExpired { .. })) => {
                        observed_checkout_stream = self.recover_checkout_watch(observed_checkouts).await?;
                    }
                    Some(Err(err)) => return Err(err),
                    None => return Err(ResourceError::other("aggregator observed checkout watch ended")),
                },
                replica = replica_rx.recv() => match replica {
                    Ok(snapshots) => self.apply_replica_cache(snapshots).await,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "aggregator lagged behind fleet replica refreshes");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return Err(ResourceError::other("aggregator fleet replica channel closed"));
                    }
                },
            }
        }
    }

    async fn recover_convoy_watch(
        &mut self,
        source: LocalSource,
        resolver: &dyn AggregatorWatchSource<Convoy>,
    ) -> Result<WatchStream<Convoy>, ResourceError> {
        loop {
            match self.list_and_watch_convoys(source, resolver).await {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
    }

    async fn recover_presentation_watch(
        &mut self,
        source: LocalSource,
        resolver: &dyn AggregatorWatchSource<Presentation>,
    ) -> Result<WatchStream<Presentation>, ResourceError> {
        loop {
            match self.list_and_watch_presentations(source, resolver).await {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
    }

    async fn recover_environment_watch(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Environment>,
    ) -> Result<WatchStream<Environment>, ResourceError> {
        loop {
            match self.list_and_watch_environments(resolver).await {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
    }

    async fn recover_session_watch(
        &mut self,
        source: LocalSource,
        resolver: &dyn AggregatorWatchSource<TerminalSession>,
    ) -> Result<WatchStream<TerminalSession>, ResourceError> {
        loop {
            match self.list_and_watch_sessions(source, resolver).await {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
    }

    async fn recover_project_watch(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Project>,
    ) -> Result<WatchStream<Project>, ResourceError> {
        loop {
            match self.list_and_watch_projects(resolver).await {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
    }

    async fn recover_repository_watch(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Repository>,
    ) -> Result<WatchStream<Repository>, ResourceError> {
        loop {
            match self.list_and_watch_repositories(resolver).await {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
    }

    async fn recover_checkout_watch(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Checkout>,
    ) -> Result<WatchStream<Checkout>, ResourceError> {
        loop {
            match self.list_and_watch_checkouts(resolver).await {
                Err(ResourceError::WatchExpired { .. }) => tokio::time::sleep(Self::WATCH_RESTART_BACKOFF).await,
                result => return result,
            }
        }
    }

    async fn list_and_watch_convoys(
        &mut self,
        source: LocalSource,
        resolver: &dyn AggregatorWatchSource<Convoy>,
    ) -> Result<WatchStream<Convoy>, ResourceError> {
        let listed = resolver.list().await?;
        let start = WatchStart::resuming_from(&listed);
        let watch = resolver.watch(start).await?;
        self.replace_convoy_source(source, listed.items).await;
        Ok(watch)
    }

    async fn list_and_watch_presentations(
        &mut self,
        source: LocalSource,
        resolver: &dyn AggregatorWatchSource<Presentation>,
    ) -> Result<WatchStream<Presentation>, ResourceError> {
        let listed = resolver.list().await?;
        let start = WatchStart::resuming_from(&listed);
        let watch = resolver.watch(start).await?;
        self.replace_presentation_source(source, listed.items).await;
        Ok(watch)
    }

    async fn replace_convoy_source(&mut self, source: LocalSource, convoys: Vec<ResourceObject<Convoy>>) {
        let previous = self.effective_convoys();
        let replacement =
            convoys.into_iter().map(|convoy| (self.convoy_ref(&convoy.metadata.namespace, &convoy.metadata.name), convoy)).collect();
        self.convoys_by_source.insert(source, replacement);
        let current = self.effective_convoys();
        let references = previous.keys().chain(current.keys()).cloned().collect::<HashSet<_>>();
        self.convoy_change_requests.retain(|reference, _| current.contains_key(reference));
        for reference in references {
            self.handle_convoy_transition(&reference, previous.get(&reference), current.get(&reference));
        }
        self.rebuild_local_projection().await;
    }

    async fn list_and_watch_environments(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Environment>,
    ) -> Result<WatchStream<Environment>, ResourceError> {
        let listed = resolver.list().await?;
        let watch = resolver.watch(WatchStart::resuming_from(&listed)).await?;
        self.rebuild_independents_projection().await;
        Ok(watch)
    }

    async fn list_and_watch_sessions(
        &mut self,
        source: LocalSource,
        resolver: &dyn AggregatorWatchSource<TerminalSession>,
    ) -> Result<WatchStream<TerminalSession>, ResourceError> {
        let listed = resolver.list().await?;
        let start = WatchStart::resuming_from(&listed);
        let watch = resolver.watch(start).await?;
        self.replace_session_source(source, listed.items).await;
        Ok(watch)
    }

    async fn list_and_watch_projects(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Project>,
    ) -> Result<WatchStream<Project>, ResourceError> {
        let listed = resolver.list().await?;
        let watch = resolver.watch(WatchStart::resuming_from(&listed)).await?;
        self.projects = listed
            .items
            .into_iter()
            .map(|project| ((project.metadata.namespace.clone(), project.metadata.name.clone()), project))
            .collect();
        self.rebuild_store_catalog().await;
        Ok(watch)
    }

    async fn list_and_watch_repositories(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Repository>,
    ) -> Result<WatchStream<Repository>, ResourceError> {
        let listed = resolver.list().await?;
        let watch = resolver.watch(WatchStart::resuming_from(&listed)).await?;
        self.repositories = listed.items.into_iter().map(|repository| (repository.spec.key(), repository)).collect();
        self.rebuild_store_catalog().await;
        Ok(watch)
    }

    async fn list_and_watch_checkouts(
        &mut self,
        resolver: &dyn AggregatorWatchSource<Checkout>,
    ) -> Result<WatchStream<Checkout>, ResourceError> {
        let listed = resolver.list().await?;
        let watch = resolver.watch(WatchStart::resuming_from(&listed)).await?;
        self.observed_checkouts = listed
            .items
            .into_iter()
            .map(|checkout| (self.checkout_ref(&checkout.metadata.namespace, &checkout.metadata.name), checkout))
            .collect();
        self.rebuild_checkout_rows().await?;
        Ok(watch)
    }

    async fn replace_presentation_source(&mut self, source: LocalSource, presentations: Vec<ResourceObject<Presentation>>) {
        let replacement = presentations
            .into_iter()
            .map(|presentation| (self.presentation_ref(&presentation.metadata.namespace, &presentation.metadata.name), presentation))
            .collect();
        self.presentations_by_source.insert(source, replacement);
        self.rebuild_local_projection().await;
    }

    async fn apply_presentation_event_from(&mut self, source: LocalSource, event: WatchEvent<Presentation>) {
        match event {
            WatchEvent::Added(presentation) | WatchEvent::Modified(presentation) => {
                let reference = self.presentation_ref(&presentation.metadata.namespace, &presentation.metadata.name);
                self.presentations_by_source.entry(source).or_default().insert(reference, presentation);
            }
            WatchEvent::Deleted(presentation) => {
                let reference = self.presentation_ref(&presentation.metadata.namespace, &presentation.metadata.name);
                self.presentations_by_source.entry(source).or_default().remove(&reference);
            }
        }
        self.rebuild_local_projection().await;
    }

    async fn apply_convoy_event_from(&mut self, source: LocalSource, event: WatchEvent<Convoy>) {
        let convoy = match &event {
            WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) | WatchEvent::Deleted(convoy) => convoy,
        };
        let reference = self.convoy_ref(&convoy.metadata.namespace, &convoy.metadata.name);
        let previous = self.effective_convoy(&reference).cloned();
        match event {
            WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) => {
                self.convoys_by_source.entry(source).or_default().insert(reference.clone(), convoy);
            }
            WatchEvent::Deleted(_) => {
                self.convoys_by_source.entry(source).or_default().remove(&reference);
            }
        }
        let current = self.effective_convoy(&reference).cloned();
        self.handle_convoy_transition(&reference, previous.as_ref(), current.as_ref());
        self.rebuild_local_projection().await;
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
        if association_changed || phase_changed {
            if let Some(current) = current {
                self.schedule_change_request_refresh(current);
            }
        }
    }

    fn convoy_association_changed(previous: Option<&ResourceObject<Convoy>>, current: Option<&ResourceObject<Convoy>>) -> bool {
        previous.map(|convoy| (&convoy.spec.r#ref, &convoy.spec.repositories))
            != current.map(|convoy| (&convoy.spec.r#ref, &convoy.spec.repositories))
    }

    fn convoy_phase_changed(previous: Option<&ResourceObject<Convoy>>, current: Option<&ResourceObject<Convoy>>) -> bool {
        previous.and_then(|convoy| convoy.status.as_ref().map(|status| status.phase))
            != current.and_then(|convoy| convoy.status.as_ref().map(|status| status.phase))
    }

    fn invalidate_change_request(&mut self, reference: &ResourceRef) {
        let generation = self.change_request_refresh_generations.entry(reference.clone()).or_default();
        *generation = generation.saturating_add(1);
        if let Some(task) = self.change_request_refresh_tasks.remove(reference) {
            task.abort();
        }
        self.convoy_change_requests.remove(reference);
    }

    fn schedule_change_request_refresh(&mut self, convoy: &ResourceObject<Convoy>) {
        let reference = self.convoy_ref(&convoy.metadata.namespace, &convoy.metadata.name);
        let generation = self.change_request_refresh_generations.entry(reference.clone()).or_default();
        *generation = generation.saturating_add(1);
        let generation = *generation;
        if let Some(task) = self.change_request_refresh_tasks.remove(&reference) {
            task.abort();
        }
        let Some(branch) = convoy.spec.r#ref.clone() else {
            self.convoy_change_requests.remove(&reference);
            return;
        };
        let repositories = convoy.spec.repositories.iter().map(|repository| repository.repo_ref.clone()).collect::<Vec<_>>();
        if repositories.is_empty() {
            self.convoy_change_requests.remove(&reference);
            return;
        }
        let Some(resolver) = self.change_request_resolver.clone() else {
            return;
        };
        let refresh_tx = self.change_request_refresh_queue.tx.clone();
        let task_reference = reference.clone();
        let task = tokio::spawn(async move {
            let result = resolver.resolve_change_request(&repositories, &branch).await;
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
        match result {
            Ok(Some(change_request)) => {
                self.convoy_change_requests.insert(reference.clone(), change_request);
            }
            Ok(None) => {
                self.convoy_change_requests.remove(&reference);
            }
            Err(error) => {
                tracing::warn!(convoy = %reference.name, %branch, %error, "failed to refresh convoy change request");
            }
        }
        self.rebuild_local_projection().await;
    }

    async fn refresh_all_change_requests(&mut self) {
        let effective_convoys = self.effective_convoys();
        self.convoy_change_requests.retain(|reference, _| effective_convoys.contains_key(reference));
        for convoy in effective_convoys.values() {
            self.schedule_change_request_refresh(convoy);
        }
        self.rebuild_local_projection().await;
    }

    fn repo_snapshot_changed_change_requests(&mut self, snapshot: &RepoSnapshot) -> bool {
        let current = change_request_fingerprint(&snapshot.providers);
        match self.repo_change_requests.get(&snapshot.repo_identity) {
            Some(previous) if previous == &current => false,
            None if current.is_empty() => false,
            _ => {
                self.repo_change_requests.insert(snapshot.repo_identity.clone(), current);
                true
            }
        }
    }

    fn repo_delta_changed_change_requests(&mut self, delta: &RepoDelta) -> bool {
        let mut changed = false;
        let fingerprint = self.repo_change_requests.entry(delta.repo_identity.clone()).or_default();
        for change in &delta.changes {
            let Change::ChangeRequest { key, op } = change else { continue };
            changed = true;
            match op {
                EntryOp::Added(request) | EntryOp::Updated(request) => {
                    fingerprint.insert(key.clone(), (request.branch.clone(), request.status.to_string()));
                }
                EntryOp::Removed => {
                    fingerprint.remove(key);
                }
            }
        }
        changed
    }

    fn effective_convoys(&self) -> HashMap<ResourceRef, ResourceObject<Convoy>> {
        let mut effective_convoys = HashMap::new();
        for source in LOCAL_SOURCE_PRECEDENCE {
            let Some(convoys) = self.convoys_by_source.get(&source) else { continue };
            effective_convoys.extend(convoys.iter().map(|(reference, convoy)| (reference.clone(), convoy.clone())));
        }
        effective_convoys.retain(|_, convoy| convoy.metadata.deletion_timestamp.is_none());
        effective_convoys
    }

    fn effective_convoy(&self, reference: &ResourceRef) -> Option<&ResourceObject<Convoy>> {
        LOCAL_SOURCE_PRECEDENCE
            .iter()
            .rev()
            .find_map(|source| self.convoys_by_source.get(source)?.get(reference))
            .filter(|convoy| convoy.metadata.deletion_timestamp.is_none())
    }

    async fn rebuild_local_projection(&mut self) {
        let effective_convoys = self.effective_convoys();
        let presentation_keys = effective_convoys
            .values()
            .flat_map(|convoy| {
                convoy.status.as_ref().and_then(|status| status.workflow_snapshot.as_ref()).into_iter().flat_map(|snapshot| {
                    snapshot
                        .vessels
                        .iter()
                        .map(|vessel| (convoy.metadata.namespace.clone(), convoy.metadata.name.clone(), vessel.name.clone()))
                })
            })
            .collect::<HashSet<_>>();

        let mut presentation_workspaces = HashMap::new();
        for source in LOCAL_SOURCE_PRECEDENCE {
            let Some(presentations) = self.presentations_by_source.get(&source) else { continue };
            for presentation in presentations.values() {
                let Some(key) = presentation_key(presentation) else { continue };
                if !presentation_keys.contains(&key) {
                    continue;
                }
                if let Some(workspace_ref) = presentation.status.as_ref().and_then(|status| status.observed_workspace_ref.clone()) {
                    presentation_workspaces.insert(key, workspace_ref);
                } else {
                    // A higher-precedence source with no active workspace still
                    // masks an attach target from a lower-precedence source.
                    presentation_workspaces.remove(&key);
                }
            }
        }
        self.presentation_workspaces = presentation_workspaces;

        let replacement = effective_convoys
            .into_values()
            .map(|convoy| {
                let row = self.summarize(&convoy);
                (row.resource.clone(), row)
            })
            .collect::<HashMap<_, _>>();

        let (changed, removed, result_set) = {
            let mut view = self.state.write().await;
            let changed = replacement
                .iter()
                .filter(|(reference, row)| view.local_rows.get(*reference) != Some(*row))
                .map(|(_, row)| row.clone())
                .collect::<Vec<_>>();
            let removed = view.local_rows.keys().filter(|reference| !replacement.contains_key(*reference)).cloned().collect::<Vec<_>>();
            if changed.is_empty() && removed.is_empty() {
                return;
            }
            view.local_rows = replacement;
            view.seq = view.seq.saturating_add(1);
            (changed, removed, view.result_set())
        };

        if self.bootstrapping {
            return;
        }
        if self.emitted_queries.contains(&QueryId::Convoys) {
            self.emit_delta(changed, removed).await;
        } else {
            self.emitted_queries.insert(QueryId::Convoys);
            let _ = self.event_tx.send(DaemonEvent::ResultSet(Box::new(result_set)));
        }
        let represented = self.state.represented_issue_refs().await;
        for delta in self.state.suppress_issues(&represented) {
            let _ = self.event_tx.send(DaemonEvent::ResultDelta(Box::new(delta)));
        }
        if let Some(materializer) = &self.issue_materializer {
            // The direct delta hides represented rows immediately; refiltering
            // republishes loaded windows so rows can reappear when representation
            // changes again.
            materializer.refilter_active_queries();
        }
    }

    async fn replace_session_source(&mut self, source: LocalSource, sessions: Vec<ResourceObject<TerminalSession>>) {
        let replacement =
            sessions.into_iter().map(|session| ((session.metadata.namespace.clone(), session.metadata.name.clone()), session)).collect();
        self.sessions_by_source.insert(source, replacement);
        self.rebuild_independents_projection().await;
        self.rebuild_local_projection().await;
    }

    async fn apply_session_event_from(&mut self, source: LocalSource, event: WatchEvent<TerminalSession>) {
        match event {
            WatchEvent::Added(session) | WatchEvent::Modified(session) => {
                self.sessions_by_source
                    .entry(source)
                    .or_default()
                    .insert((session.metadata.namespace.clone(), session.metadata.name.clone()), session);
            }
            WatchEvent::Deleted(session) => {
                self.sessions_by_source
                    .entry(source)
                    .or_default()
                    .remove(&(session.metadata.namespace.clone(), session.metadata.name.clone()));
            }
        }
        self.rebuild_independents_projection().await;
        self.rebuild_local_projection().await;
    }

    async fn apply_environment_event(&mut self, _event: WatchEvent<Environment>) {
        self.rebuild_independents_projection().await;
    }

    async fn apply_project_event(&mut self, event: WatchEvent<Project>) {
        match event {
            WatchEvent::Added(project) | WatchEvent::Modified(project) => {
                self.projects.insert((project.metadata.namespace.clone(), project.metadata.name.clone()), project);
            }
            WatchEvent::Deleted(project) => {
                self.projects.remove(&(project.metadata.namespace, project.metadata.name));
            }
        }
        self.rebuild_store_catalog().await;
    }

    async fn apply_repository_event(&mut self, event: WatchEvent<Repository>) {
        match event {
            WatchEvent::Added(repository) | WatchEvent::Modified(repository) => {
                self.repositories.insert(repository.spec.key(), repository);
            }
            WatchEvent::Deleted(repository) => {
                self.repositories.remove(&repository.spec.key());
            }
        }
        self.rebuild_store_catalog().await;
    }

    async fn apply_checkout_event(&mut self, event: WatchEvent<Checkout>) -> Result<(), ResourceError> {
        match event {
            WatchEvent::Added(checkout) | WatchEvent::Modified(checkout) => {
                let reference = self.checkout_ref(&checkout.metadata.namespace, &checkout.metadata.name);
                self.observed_checkouts.insert(reference, checkout);
            }
            WatchEvent::Deleted(checkout) => {
                let reference = self.checkout_ref(&checkout.metadata.namespace, &checkout.metadata.name);
                self.observed_checkouts.remove(&reference);
            }
        }
        self.rebuild_checkout_rows().await
    }

    async fn rebuild_store_catalog(&self) {
        let repositories = self.repositories.keys().cloned().collect();
        let projects = self
            .projects
            .values()
            .map(|project| {
                let scope = QueryScope::new(project.metadata.namespace.clone(), project.metadata.name.clone());
                let repositories = project.spec.repositories.iter().map(|repository| repository.repo.clone()).collect();
                (scope, repositories)
            })
            .collect();
        let deltas = self.state.replace_store_catalog(repositories, projects).await;
        self.emit_store_deltas(deltas);
    }

    async fn rebuild_checkout_rows(&self) -> Result<(), ResourceError> {
        let rows = self
            .observed_checkouts
            .values()
            .filter_map(|checkout| {
                let CheckoutSpec::Observed(spec) = &checkout.spec else { return None };
                Some((checkout, spec))
            })
            .map(|(checkout, spec)| {
                let authority = checkout.metadata.lifecycle_authority()?.unwrap_or(LifecycleAuthority::Observed);
                Ok(CheckoutRow::builder()
                    .resource(self.checkout_ref(&checkout.metadata.namespace, &checkout.metadata.name))
                    .repo(spec.repo_ref.clone())
                    .path(spec.path.clone())
                    .branch(spec.r#ref.clone())
                    .host(self.local_host.clone())
                    .authority(authority)
                    .build())
            })
            .collect::<Result<Vec<_>, ResourceError>>()?;
        let deltas = self.state.replace_local_checkout_rows(rows).await;
        self.emit_store_deltas(deltas);
        Ok(())
    }

    fn emit_store_deltas(&self, deltas: Vec<ResultDelta>) {
        if self.bootstrapping {
            return;
        }
        for delta in deltas {
            let _ = self.event_tx.send(DaemonEvent::ResultDelta(Box::new(delta)));
        }
    }

    async fn rebuild_independents_projection(&mut self) {
        let mut effective_sessions = HashMap::new();
        for source in LOCAL_SOURCE_PRECEDENCE {
            let Some(sessions) = self.sessions_by_source.get(&source) else { continue };
            effective_sessions.extend(sessions.iter().map(|(key, session)| (key.clone(), session.clone())));
        }
        self.terminal_sessions = effective_sessions;

        let attachable_references = match &self.attach_resolver {
            Some(daemon) => {
                let references = self
                    .terminal_sessions
                    .values()
                    .filter(|session| is_independent_session(session))
                    .map(|session| session.metadata.name.clone())
                    .collect::<Vec<_>>();
                daemon.resolvable_attach_references(&references).await.unwrap_or_default()
            }
            None => HashSet::new(),
        };

        let replacement =
            self.terminal_sessions.values().filter_map(|session| self.summarize_independent(session, &attachable_references)).collect();
        let deltas = self.state.replace_local_independent_rows(replacement).await;
        self.emit_store_deltas(deltas);
    }

    pub async fn apply_replica_cache(&mut self, snapshots: Vec<FleetReplicaSnapshot>) {
        let mut convoy_replacements = HashMap::new();
        let mut independent_replacements = HashMap::new();
        let mut checkout_replacements = HashMap::new();
        for snapshot in snapshots {
            let host = snapshot.host;
            let mut convoy_rows = HashMap::new();
            let mut independent_rows = Vec::new();
            let mut checkout_rows = Vec::new();
            for result_set in snapshot.result_sets {
                match result_set.rows {
                    Rows::Convoys(convoys) => {
                        for mut row in convoys {
                            set_convoy_row_host(&mut row, &host);
                            convoy_rows.insert(row.resource.clone(), row);
                        }
                    }
                    Rows::Independents { scope: None, rows: independents } => {
                        for mut row in independents {
                            set_independent_row_host(&mut row, &host);
                            independent_rows.push(row);
                        }
                    }
                    Rows::Independents { scope: Some(_), .. } => {
                        tracing::warn!(host = %host, "ignoring derived project independents set in fleet replica snapshot");
                    }
                    Rows::Issues { .. } => {
                        tracing::warn!(host = %host, "ignoring demand-backed issues in fleet replica snapshot");
                    }
                    Rows::Checkouts { scope: None, rows } => {
                        for mut row in rows {
                            set_checkout_row_host(&mut row, &host);
                            checkout_rows.push(row);
                        }
                    }
                    Rows::Checkouts { scope: Some(_), .. } => {
                        tracing::warn!(host = %host, "ignoring derived project checkout set in fleet replica snapshot");
                    }
                }
            }
            convoy_replacements.insert(host.clone(), convoy_rows);
            independent_replacements.insert(host.clone(), independent_rows);
            checkout_replacements.insert(host, checkout_rows);
        }

        let convoy_change = {
            let mut view = self.state.write().await;
            view.replace_replica_rows(convoy_replacements)
        };
        if let Some((changed, removed)) = convoy_change {
            if self.emitted_queries.contains(&QueryId::Convoys) {
                self.emit_delta(changed, removed).await;
            } else {
                self.emitted_queries.insert(QueryId::Convoys);
                let _ = self.event_tx.send(DaemonEvent::ResultSet(Box::new(self.state.result_set().await)));
            }
            let represented = self.state.represented_issue_refs().await;
            for delta in self.state.suppress_issues(&represented) {
                let _ = self.event_tx.send(DaemonEvent::ResultDelta(Box::new(delta)));
            }
            if let Some(materializer) = &self.issue_materializer {
                // The direct delta hides represented rows immediately; refiltering
                // republishes loaded windows so rows can reappear when representation
                // changes again.
                materializer.refilter_active_queries();
            }
        }

        let independent_deltas = self.state.replace_independent_replica_rows(independent_replacements).await;
        self.emit_store_deltas(independent_deltas);

        let checkout_deltas = self.state.replace_checkout_replica_rows(checkout_replacements).await;
        self.emit_store_deltas(checkout_deltas);
    }

    async fn emit_delta(&self, changed: Vec<ConvoyRow>, removed: Vec<ResourceRef>) {
        let seq = self.state.seq().await;
        let changes = QueryChanges::Convoys { changed, removed };
        let _ = self.event_tx.send(DaemonEvent::ResultDelta(Box::new(ResultDelta { seq, changes, state: None })));
    }

    fn convoy_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(Convoy::API_PATHS), Convoy::API_PATHS.kind, namespace, name).on_host(self.local_host.clone())
    }

    fn presentation_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(Presentation::API_PATHS), Presentation::API_PATHS.kind, namespace, name)
            .on_host(self.local_host.clone())
    }

    fn session_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(TerminalSession::API_PATHS), TerminalSession::API_PATHS.kind, namespace, name)
            .on_host(self.local_host.clone())
    }

    fn checkout_ref(&self, namespace: &str, name: &str) -> ResourceRef {
        ResourceRef::new(api_version(Checkout::API_PATHS), Checkout::API_PATHS.kind, namespace, name).on_host(self.local_host.clone())
    }

    fn summarize_independent(
        &self,
        session: &ResourceObject<TerminalSession>,
        attachable_references: &HashSet<String>,
    ) -> Option<IndependentRow> {
        if !is_independent_session(session) {
            return None;
        }
        let name = &session.metadata.name;
        let attach = attachable_references.contains(name).then(|| name.clone());
        Some(
            IndependentRow::builder()
                .resource(self.session_ref(&session.metadata.namespace, name))
                .name(name)
                .maybe_repo(session.metadata.labels.get(REPO_LABEL).map(|repo| flotilla_protocol::RepoKey(repo.clone())))
                .maybe_repository_key(session.metadata.labels.get(REPO_KEY_LABEL).cloned().map(RepositoryKey))
                .host(self.local_host.clone())
                .maybe_attach(attach)
                .phase(SessionPhase::Running)
                .build(),
        )
    }
    fn vessel_attach(&self, namespace: &str, convoy: &str, vessel: &str) -> Option<String> {
        self.presentation_workspaces.get(&(namespace.to_string(), convoy.to_string(), vessel.to_string())).cloned()
    }

    fn summarize(&self, convoy: &ResourceObject<Convoy>) -> ConvoyRow {
        let namespace = &convoy.metadata.namespace;
        let name = &convoy.metadata.name;
        let resource = self.convoy_ref(namespace, name);
        let change_request = self.convoy_change_requests.get(&resource).cloned();
        let status = convoy.status.as_ref();
        let phase = status.map(|status| status.phase).unwrap_or_default();
        let vessels: Vec<VesselRow> = status
            .and_then(|status| status.workflow_snapshot.as_ref())
            .map(|snapshot| {
                snapshot
                    .vessels
                    .iter()
                    .map(|definition| {
                        self.summarize_vessel(&resource, definition, status.and_then(|status| status.work.get(&definition.name)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let needs_attention = vessels.iter().any(|vessel| vessel.needs_attention);
        ConvoyRow::builder()
            .resource(resource)
            .name(name)
            .workflow_ref(&convoy.spec.workflow_ref)
            .phase(convoy_phase(phase))
            .initializing(convoy_is_initializing(status))
            .maybe_message(status.and_then(|status| status.message.clone()))
            .maybe_repo(convoy.metadata.labels.get(flotilla_resources::REPO_LABEL).map(|repo| flotilla_protocol::RepoKey(repo.clone())))
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
            .maybe_change_request(change_request)
            .vessels(vessels)
            .needs_attention(needs_attention)
            .build()
    }

    fn summarize_vessel(&self, convoy_ref: &ResourceRef, definition: &VesselRequirement, state: Option<&WorkState>) -> VesselRow {
        let requested_stance = definition.stance.to_string();
        let effective_stance = state
            .and_then(|state| state.placement.as_ref())
            .and_then(|placement| placement.fields.get("effective_stance"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let crew = definition
            .crew
            .iter()
            .map(|process| {
                let command_preview = match &process.source {
                    CrewSource::Tool { command } => command.clone(),
                    CrewSource::Agent { selector, prompt } => prompt.clone().unwrap_or_else(|| selector.capability.clone()),
                };
                CrewMemberSummary {
                    role: process.role.clone(),
                    command_preview,
                    requested_stance: Some(requested_stance.clone()),
                    effective_stance: effective_stance.clone(),
                }
            })
            .collect();
        let work_unsettled = !state.is_some_and(|state| state.phase.is_terminal());
        let now = chrono::Utc::now();
        let needs_attention = self.terminal_sessions.values().any(|session| {
            session.metadata.namespace == convoy_ref.namespace
                && session.metadata.labels.get(CONVOY_LABEL) == Some(&convoy_ref.name)
                && session.metadata.labels.get(VESSEL_LABEL) == Some(&definition.name)
                && session.status.as_ref().is_some_and(|status| {
                    status.phase == TerminalSessionPhase::Running
                        && status
                            .attention
                            .as_ref()
                            .is_some_and(|attention| !attention.is_stale_at(now) && attention_needs_human(attention.state, work_unsettled))
                })
        });
        VesselRow::builder()
            .resource(convoy_ref.subresource(format!("vessels/{}", definition.name)))
            .name(&definition.name)
            .phase(work_phase(state.map(|state| state.phase).unwrap_or(ResourceWorkPhase::Pending)))
            .crew(crew)
            .maybe_ready_at(state.and_then(|state| state.ready_at))
            .maybe_started_at(state.and_then(|state| state.started_at))
            .maybe_finished_at(state.and_then(|state| state.finished_at))
            .maybe_message(state.and_then(|state| state.message.clone()))
            .requested_stance(requested_stance)
            .maybe_effective_stance(effective_stance)
            .depends_on(definition.depends_on.clone())
            .host(self.local_host.clone())
            .maybe_attach(self.vessel_attach(&convoy_ref.namespace, &convoy_ref.name, &definition.name))
            .complete_work(state.is_some_and(|state| !state.phase.is_terminal()))
            .needs_attention(needs_attention)
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

fn attention_needs_human(state: TerminalAttentionState, work_unsettled: bool) -> bool {
    state == TerminalAttentionState::NeedsInput || (state == TerminalAttentionState::Idle && work_unsettled)
}

fn change_request_fingerprint(providers: &ProviderData) -> ChangeRequestFingerprint {
    providers.change_requests.iter().map(|(key, request)| (key.clone(), (request.branch.clone(), request.status.to_string()))).collect()
}

fn presentation_key(presentation: &ResourceObject<Presentation>) -> Option<PresentationKey> {
    Some((
        presentation.metadata.namespace.clone(),
        presentation.metadata.labels.get(CONVOY_LABEL)?.clone(),
        presentation.metadata.labels.get(VESSEL_LABEL)?.clone(),
    ))
}

fn is_independent_session(session: &ResourceObject<TerminalSession>) -> bool {
    !session.metadata.labels.contains_key(CONVOY_LABEL)
        && session.status.as_ref().map(|status| status.phase) == Some(TerminalSessionPhase::Running)
}

fn set_convoy_row_host(row: &mut ConvoyRow, host: &HostName) {
    row.resource.host = Some(host.clone());
    for vessel in &mut row.vessels {
        vessel.resource.host = Some(host.clone());
        vessel.host = host.clone();
    }
}

fn set_independent_row_host(row: &mut IndependentRow, host: &HostName) {
    row.resource.host = Some(host.clone());
    row.host = host.clone();
}

fn set_checkout_row_host(row: &mut CheckoutRow, host: &HostName) {
    row.resource.host = Some(host.clone());
    row.host = host.clone();
}

fn convoy_phase(phase: ResourceConvoyPhase) -> ConvoyPhase {
    match phase {
        ResourceConvoyPhase::Pending => ConvoyPhase::Pending,
        ResourceConvoyPhase::Active => ConvoyPhase::Active,
        ResourceConvoyPhase::Completed => ConvoyPhase::Completed,
        ResourceConvoyPhase::Failed => ConvoyPhase::Failed,
        ResourceConvoyPhase::Cancelled => ConvoyPhase::Cancelled,
        ResourceConvoyPhase::Abandoned => ConvoyPhase::Abandoned,
    }
}

fn convoy_phase_is_terminal(phase: ResourceConvoyPhase) -> bool {
    matches!(
        phase,
        ResourceConvoyPhase::Completed | ResourceConvoyPhase::Failed | ResourceConvoyPhase::Cancelled | ResourceConvoyPhase::Abandoned
    )
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
        ResourceWorkPhase::Complete => WorkPhase::Complete,
        ResourceWorkPhase::Failed => WorkPhase::Failed,
        ResourceWorkPhase::Cancelled => WorkPhase::Cancelled,
        ResourceWorkPhase::Abandoned => WorkPhase::Abandoned,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use chrono::Utc;
    use flotilla_protocol::result_set::{ResultSet, ResultSetState};
    use flotilla_resources::{
        ConvoyRepositorySpec, ConvoySpec, CrewSpec, InMemoryBackend, InputMeta, ObjectMeta, PlacementStatus, PresentationPhase,
        PresentationSpec, PresentationStatus, ResourceBackend, Stance, TerminalAttention, TerminalAttentionSource, TerminalSessionSource,
        TerminalSessionSpec, TerminalSessionStatus, VesselRequirement, WorkflowSnapshot,
    };
    use futures::stream;
    use tokio::{sync::Mutex, time::timeout};

    use super::*;

    #[test]
    fn needs_attention_is_needs_input_or_idle_with_unsettled_work() {
        assert!(attention_needs_human(TerminalAttentionState::NeedsInput, false));
        assert!(attention_needs_human(TerminalAttentionState::Idle, true));
        assert!(!attention_needs_human(TerminalAttentionState::Idle, false));
        assert!(!attention_needs_human(TerminalAttentionState::Working, true));
        assert!(!attention_needs_human(TerminalAttentionState::Unobservable, true));
    }

    #[tokio::test]
    async fn attention_projection_is_namespace_scoped_and_tracks_idle_unsettled_sessions() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);
        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Added(convoy_with_vessel("convoy-a").await)).await;

        let mut session = session_object("terminal-convoy-a-implement").await;
        session.metadata.labels =
            BTreeMap::from([(CONVOY_LABEL.to_string(), "convoy-a".to_string()), (VESSEL_LABEL.to_string(), "implement".to_string())]);
        session.status.as_mut().expect("running status").attention =
            Some(TerminalAttention { state: TerminalAttentionState::Idle, as_of: Utc::now(), source: TerminalAttentionSource::Screen });

        let mut foreign_session = session.clone();
        foreign_session.metadata.namespace = "other".to_string();
        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(foreign_session)).await;
        let result_set = state.result_set().await;
        assert!(!result_set.rows.as_convoys().expect("convoy rows")[0].needs_attention);

        aggregator.apply_session_event_from(LocalSource::Durable, WatchEvent::Added(session)).await;

        let result_set = state.result_set().await;
        let convoy = result_set.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
        assert!(convoy.vessels.first().expect("vessel row").needs_attention);
        assert!(convoy.needs_attention);
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

    struct CountingAttachResolver {
        calls: AtomicUsize,
    }

    struct ScriptedChangeRequestResolver {
        results: Mutex<VecDeque<Result<Option<flotilla_protocol::ConvoyChangeRequest>, String>>>,
        branches: Mutex<Vec<String>>,
        calls: AtomicUsize,
    }

    struct BlockingChangeRequestResolver;

    #[async_trait]
    impl ConvoyChangeRequestResolver for BlockingChangeRequestResolver {
        async fn resolve_change_request(
            &self,
            _repositories: &[RepositoryKey],
            _branch: &str,
        ) -> Result<Option<flotilla_protocol::ConvoyChangeRequest>, String> {
            std::future::pending().await
        }
    }

    #[async_trait]
    impl ConvoyChangeRequestResolver for ScriptedChangeRequestResolver {
        async fn resolve_change_request(
            &self,
            repositories: &[RepositoryKey],
            branch: &str,
        ) -> Result<Option<flotilla_protocol::ConvoyChangeRequest>, String> {
            assert_eq!(repositories, [RepositoryKey("repo_flotilla".into())]);
            self.branches.lock().await.push(branch.to_string());
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.results.lock().await.pop_front().unwrap_or(Ok(None))
        }
    }

    #[async_trait]
    impl AttachCapabilityResolver for CountingAttachResolver {
        async fn resolvable_attach_references(&self, references: &[String]) -> Result<HashSet<String>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(references.iter().cloned().collect())
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
        durable_convoys: &dyn AggregatorWatchSource<Convoy>,
        durable_presentations: &dyn AggregatorWatchSource<Presentation>,
        observed_convoys: &dyn AggregatorWatchSource<Convoy>,
        observed_presentations: &dyn AggregatorWatchSource<Presentation>,
        replica_rx: broadcast::Receiver<Vec<FleetReplicaSnapshot>>,
    ) -> Result<(), ResourceError> {
        let durable_environments = ScriptedSource::<Environment>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_sessions = ScriptedSource::<TerminalSession>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_projects = ScriptedSource::<Project>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let durable_repositories = ScriptedSource::<Repository>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_sessions = ScriptedSource::<TerminalSession>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_checkouts = ScriptedSource::<Checkout>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let sources = AggregatorSourceRefs::builder()
            .durable_convoys(durable_convoys)
            .durable_environments(&durable_environments)
            .durable_presentations(durable_presentations)
            .durable_sessions(&durable_sessions)
            .durable_projects(&durable_projects)
            .durable_repositories(&durable_repositories)
            .observed_convoys(observed_convoys)
            .observed_presentations(observed_presentations)
            .observed_sessions(&observed_sessions)
            .observed_checkouts(&observed_checkouts)
            .build();
        aggregator.run_with_sources(sources, replica_rx).await
    }

    async fn convoy_object(name: &str) -> ResourceObject<Convoy> {
        let backend = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
        backend
            .using::<Convoy>("flotilla")
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
                &ConvoySpec::builder().workflow_ref("scratch".to_string()).build(),
            )
            .await
            .expect("create scripted convoy")
    }

    async fn convoy_with_branch(name: &str) -> ResourceObject<Convoy> {
        let backend = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
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
                        .base_ref("main".to_string())
                        .workspace_slug("flotilla".to_string())
                        .subpaths(Vec::new())
                        .build()])
                    .build(),
            )
            .await
            .expect("create branch-backed convoy")
    }

    async fn convoy_with_vessel(name: &str) -> ResourceObject<Convoy> {
        let backend = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
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
                vessels: vec![VesselRequirement::builder().name("implement".to_string()).crew(Vec::new()).build()],
            }),
            work: BTreeMap::from([("implement".to_string(), WorkState::builder().phase(ResourceWorkPhase::Ready).build())]),
            ..Default::default()
        };
        resolver.update_status(name, &created.metadata.resource_version, &status).await.expect("set convoy vessel status")
    }

    async fn presentation_object(name: &str, convoy: &str, vessel: &str, workspace: Option<&str>) -> ResourceObject<Presentation> {
        let backend = ResourceBackend::InMemory(flotilla_resources::InMemoryBackend::default());
        let resolver = backend.using::<Presentation>("flotilla");
        let created = resolver
            .create(
                &InputMeta::builder()
                    .name(name.to_string())
                    .labels(BTreeMap::from([
                        (CONVOY_LABEL.to_string(), convoy.to_string()),
                        (VESSEL_LABEL.to_string(), vessel.to_string()),
                    ]))
                    .build(),
                &PresentationSpec::builder()
                    .convoy_ref(convoy.to_string())
                    .presentation_policy_ref("default".to_string())
                    .name(name.to_string())
                    .build(),
            )
            .await
            .expect("create scripted presentation");
        resolver
            .update_status(name, &created.metadata.resource_version, &PresentationStatus {
                phase: PresentationPhase::Active,
                observed_workspace_ref: workspace.map(str::to_string),
                ..Default::default()
            })
            .await
            .expect("set scripted presentation status")
    }

    async fn session_object(name: &str) -> ResourceObject<TerminalSession> {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<TerminalSession>("flotilla");
        let created = resolver
            .create(
                &InputMeta::builder().name(name.to_string()).build(),
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
            .update_status(name, &created.metadata.resource_version, &TerminalSessionStatus {
                phase: TerminalSessionPhase::Running,
                session_id: Some(name.to_string()),
                ..Default::default()
            })
            .await
            .expect("set scripted terminal session status")
    }

    #[tokio::test]
    async fn independents_projection_resolves_attach_capabilities_once_per_rebuild() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _) = broadcast::channel(4);
        let resolver = Arc::new(CountingAttachResolver { calls: AtomicUsize::new(0) });
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx).with_attach_resolver(Arc::clone(&resolver));

        aggregator
            .replace_session_source(LocalSource::Durable, vec![session_object("session-a").await, session_object("session-b").await])
            .await;

        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
        let result_set = state.independents_result_set(&None).await;
        let rows = result_set.rows.as_independents().expect("session rows");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.attach.as_deref() == Some(row.name.as_str())));
    }

    #[tokio::test]
    async fn convoy_row_insert_and_removal_are_not_blocked_by_change_request_enrichment() {
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let convoy = convoy_with_branch("convoy-a").await;
        let mut deleting_convoy = convoy.clone();
        deleting_convoy.metadata.deletion_timestamp = Some(Utc::now());
        let durable_convoys = ScriptedSource::new(vec![empty_list()], vec![Ok(watch_events(vec![
            WatchEvent::Added(convoy),
            WatchEvent::Modified(deleting_convoy),
        ]))]);
        let durable_presentations = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_convoys = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_presentations = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let (_replica_tx, replica_rx) = broadcast::channel(1);
        let run = run_with_test_sources(
            Aggregator::new(state, HostName::new("local"), event_tx).with_change_request_resolver(Arc::new(BlockingChangeRequestResolver)),
            &durable_convoys,
            &durable_presentations,
            &observed_convoys,
            &observed_presentations,
            replica_rx,
        );
        tokio::pin!(run);

        tokio::select! {
            result = &mut run => panic!("aggregator stopped before row lifecycle events: {result:?}"),
            () = async {
                let initial = recv_query_event(&mut event_rx, QueryId::Convoys, "initial convoy result set").await;
                let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial result set") };
                assert!(initial.rows.is_empty());

                let inserted = recv_query_event(&mut event_rx, QueryId::Convoys, "convoy insert delta").await;
                let DaemonEvent::ResultDelta(inserted) = inserted else { panic!("expected insert delta") };
                let QueryChanges::Convoys { changed, removed } = &inserted.changes else { panic!("expected convoy changes") };
                assert_eq!(changed.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), vec!["convoy-a"]);
                assert!(removed.is_empty());

                let removed = recv_query_event(&mut event_rx, QueryId::Convoys, "convoy removal delta").await;
                let DaemonEvent::ResultDelta(removed) = removed else { panic!("expected removal delta") };
                let QueryChanges::Convoys { changed, removed } = &removed.changes else { panic!("expected convoy changes") };
                assert!(changed.is_empty());
                assert_eq!(
                    removed.iter().map(|resource| resource.name.as_str()).collect::<Vec<_>>(),
                    vec!["convoy-a"]
                );
            } => {}
        }
    }

    #[tokio::test]
    async fn convoy_phase_change_refreshes_its_change_request_reference() {
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
        let mut aggregator = Aggregator::new(state, HostName::new("local"), event_tx).with_change_request_resolver(Arc::clone(&resolver));
        let mut convoy = convoy_with_branch("convoy-a").await;

        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Added(convoy.clone())).await;
        let DaemonEvent::ResultSet(initial) = event_rx.recv().await.expect("initial result set") else {
            panic!("expected initial result set");
        };
        assert!(initial.rows.as_convoys().expect("convoy rows")[0].change_request.is_none());

        apply_next_change_request_resolution(&mut aggregator).await;
        let DaemonEvent::ResultDelta(initial_change_request) = event_rx.recv().await.expect("initial change request delta") else {
            panic!("expected initial change request delta");
        };
        assert_eq!(
            initial_change_request.changes.as_convoys().expect("convoy changes")[0].change_request.as_ref().expect("change request").status,
            flotilla_protocol::ChangeRequestStatus::Open
        );

        convoy.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Completed, ..Default::default() });
        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Modified(convoy)).await;
        let DaemonEvent::ResultDelta(phase_delta) = event_rx.recv().await.expect("phase delta") else {
            panic!("expected result delta");
        };
        let phase_row = &phase_delta.changes.as_convoys().expect("convoy changes")[0];
        assert_eq!(phase_row.phase, ConvoyPhase::Completed);
        assert_eq!(phase_row.change_request.as_ref().expect("cached change request").status, flotilla_protocol::ChangeRequestStatus::Open);

        apply_next_change_request_resolution(&mut aggregator).await;
        let DaemonEvent::ResultDelta(change_request_delta) = event_rx.recv().await.expect("change request delta") else {
            panic!("expected result delta");
        };
        assert_eq!(
            change_request_delta.changes.as_convoys().expect("convoy changes")[0].change_request.as_ref().expect("change request").status,
            flotilla_protocol::ChangeRequestStatus::Merged
        );
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
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

        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Added(convoy.clone())).await;
        assert!(matches!(event_rx.recv().await.expect("initial result set"), DaemonEvent::ResultSet(_)));
        apply_next_change_request_resolution(&mut aggregator).await;
        assert!(matches!(event_rx.recv().await.expect("initial change request delta"), DaemonEvent::ResultDelta(_)));

        convoy.spec.r#ref = Some("feat/rebased".into());
        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Modified(convoy)).await;
        let DaemonEvent::ResultDelta(delta) = event_rx.recv().await.expect("association removal delta") else {
            panic!("expected result delta");
        };
        assert!(delta.changes.as_convoys().expect("convoy changes")[0].change_request.is_none());
        apply_next_change_request_resolution(&mut aggregator).await;
        assert_eq!(resolver.branches.lock().await.as_slice(), ["feat/convoy", "feat/rebased"]);
    }

    #[tokio::test]
    async fn shadowed_convoy_source_cannot_replace_the_effective_change_request() {
        let state = AggregatorProjectionState::new();
        let (event_tx, _event_rx) = broadcast::channel(8);
        let resolver = Arc::new(ScriptedChangeRequestResolver {
            results: Mutex::new(VecDeque::from([
                Ok(Some(ConvoyChangeRequest {
                    id: "815".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
                Ok(Some(ConvoyChangeRequest {
                    id: "900".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
                Ok(Some(ConvoyChangeRequest {
                    id: "816".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Merged,
                    repository_key: RepositoryKey("repo_flotilla".into()),
                })),
            ])),
            branches: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let mut aggregator = Aggregator::new(state, HostName::new("local"), event_tx).with_change_request_resolver(Arc::clone(&resolver));
        let mut durable = convoy_with_branch("convoy-a").await;
        let mut observed = durable.clone();
        observed.spec.r#ref = Some("feat/observed".into());

        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Added(durable.clone())).await;
        apply_next_change_request_resolution(&mut aggregator).await;
        aggregator.apply_convoy_event_from(LocalSource::Observed, WatchEvent::Added(observed)).await;
        apply_next_change_request_resolution(&mut aggregator).await;

        durable.spec.r#ref = Some("feat/durable-updated".into());
        durable.status = Some(ConvoyStatus { phase: ResourceConvoyPhase::Completed, ..Default::default() });
        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Modified(durable)).await;

        let reference = aggregator.convoy_ref("flotilla", "convoy-a");
        assert_eq!(aggregator.convoy_change_requests.get(&reference).expect("effective change request").id, "900");
        assert_eq!(resolver.branches.lock().await.as_slice(), ["feat/convoy", "feat/observed"]);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn provider_refresh_cadence_refreshes_convoy_change_requests() {
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
        let durable_presentations = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_convoys = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_presentations = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let (_replica_tx, replica_rx) = broadcast::channel(1);
        let run = run_with_test_sources(
            Aggregator::new(state, HostName::new("local"), event_tx.clone()).with_change_request_resolver(Arc::clone(&resolver)),
            &durable_convoys,
            &durable_presentations,
            &observed_convoys,
            &observed_presentations,
            replica_rx,
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("aggregator stopped before refresh: {result:?}"),
            () = async {
                let initial = recv_query_event(&mut event_rx, QueryId::Convoys, "initial convoy result set").await;
                assert!(matches!(initial, DaemonEvent::ResultSet(_)));
                let initial_change_request = recv_query_event(&mut event_rx, QueryId::Convoys, "initial change request delta").await;
                assert!(matches!(initial_change_request, DaemonEvent::ResultDelta(_)));
                event_tx
                    .send(DaemonEvent::RepoRefreshCompleted {
                        repo_identity: flotilla_protocol::RepoIdentity {
                            authority: "github.com".into(),
                            path: "flotilla-org/flotilla".into(),
                        },
                        repo: Some(std::path::PathBuf::from("/repo")),
                    })
                    .expect("publish provider refresh");

                let refreshed = recv_query_event(&mut event_rx, QueryId::Convoys, "refreshed convoy delta").await;
                let DaemonEvent::ResultDelta(delta) = refreshed else { panic!("expected refreshed result delta") };
                assert_eq!(
                    delta.changes.as_convoys().expect("convoy changes")[0]
                        .change_request
                        .as_ref()
                        .expect("change request")
                        .status,
                    flotilla_protocol::ChangeRequestStatus::Closed
                );
            } => {}
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn repo_snapshot_refreshes_convoy_change_requests() {
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
        let durable_presentations = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_convoys = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_presentations = ScriptedSource::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let (_replica_tx, replica_rx) = broadcast::channel(1);
        let run = run_with_test_sources(
            Aggregator::new(state, HostName::new("local"), event_tx.clone()).with_change_request_resolver(Arc::clone(&resolver)),
            &durable_convoys,
            &durable_presentations,
            &observed_convoys,
            &observed_presentations,
            replica_rx,
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("aggregator stopped before repo snapshot: {result:?}"),
            () = async {
                let initial = recv_query_event(&mut event_rx, QueryId::Convoys, "initial convoy result set").await;
                assert!(matches!(initial, DaemonEvent::ResultSet(_)));
                let initial_change_request = recv_query_event(&mut event_rx, QueryId::Convoys, "initial change request delta").await;
                assert!(matches!(initial_change_request, DaemonEvent::ResultDelta(_)));
                let mut providers = flotilla_protocol::ProviderData::default();
                providers.change_requests.insert("815".into(), flotilla_protocol::ChangeRequest {
                    title: "Fix convoy PR refs".into(),
                    branch: "feat/convoy".into(),
                    status: flotilla_protocol::ChangeRequestStatus::Open,
                    body: None,
                    correlation_keys: Vec::new(),
                    association_keys: Vec::new(),
                    provider_name: "github".into(),
                    provider_display_name: "GitHub".into(),
                });
                event_tx
                    .send(DaemonEvent::RepoSnapshot(Box::new(flotilla_protocol::RepoSnapshot {
                        seq: 1,
                        repo_identity: flotilla_protocol::RepoIdentity {
                            authority: "github.com".into(),
                            path: "flotilla-org/flotilla".into(),
                        },
                        repo: Some(std::path::PathBuf::from("/virtual/kiwi/flotilla")),
                        node_id: flotilla_protocol::NodeId::new("kiwi"),
                        work_items: Vec::new(),
                        providers,
                        provider_health: HashMap::new(),
                        errors: Vec::new(),
                    })))
                    .expect("publish repo snapshot");

                let refreshed = recv_query_event(&mut event_rx, QueryId::Convoys, "refreshed convoy delta").await;
                let DaemonEvent::ResultDelta(delta) = refreshed else { panic!("expected refreshed result delta") };
                assert_eq!(
                    delta.changes.as_convoys().expect("convoy changes")[0]
                        .change_request
                        .as_ref()
                        .expect("change request")
                        .status,
                    flotilla_protocol::ChangeRequestStatus::Closed
                );
            } => {}
        }
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn unchanged_repo_snapshot_does_not_refresh_convoy_change_requests() {
        let (event_tx, _event_rx) = broadcast::channel(1);
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx);
        let repo_identity = flotilla_protocol::RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() };
        let empty = flotilla_protocol::RepoSnapshot {
            seq: 1,
            repo_identity: repo_identity.clone(),
            repo: Some(std::path::PathBuf::from("/repo")),
            node_id: flotilla_protocol::NodeId::new("kiwi"),
            work_items: Vec::new(),
            providers: flotilla_protocol::ProviderData::default(),
            provider_health: HashMap::new(),
            errors: Vec::new(),
        };
        assert!(!aggregator.repo_snapshot_changed_change_requests(&empty));

        let mut providers = flotilla_protocol::ProviderData::default();
        providers.change_requests.insert("815".into(), flotilla_protocol::ChangeRequest {
            title: "Fix convoy PR refs".into(),
            branch: "feat/convoy".into(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            body: None,
            correlation_keys: Vec::new(),
            association_keys: Vec::new(),
            provider_name: "github".into(),
            provider_display_name: "GitHub".into(),
        });
        let with_change_request = flotilla_protocol::RepoSnapshot { providers, seq: 2, ..empty };

        assert!(aggregator.repo_snapshot_changed_change_requests(&with_change_request));
        assert!(!aggregator.repo_snapshot_changed_change_requests(&with_change_request));
    }

    fn remote_snapshot(host: &str, generation: &str, name: &str) -> FleetReplicaSnapshot {
        let host = HostName::new(host);
        let convoy = ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", name).on_host(host.clone());
        let row = ConvoyRow::builder()
            .resource(convoy)
            .name(name)
            .workflow_ref("scratch")
            .phase(ConvoyPhase::Active)
            .project_ref("my-project")
            .build();
        FleetReplicaSnapshot {
            host,
            generation: Some(generation.to_string()),
            rows: Vec::new(),
            result_sets: vec![ResultSet { seq: 1, rows: Rows::Convoys(vec![row]), state: Default::default() }],
        }
    }

    fn remote_independent_snapshot(
        host: &str,
        generation: &str,
        name: &str,
        repository_key: Option<RepositoryKey>,
    ) -> FleetReplicaSnapshot {
        let host = HostName::new(host);
        let session = ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", name);
        let row = IndependentRow::builder()
            .resource(session)
            .name(name)
            .maybe_repository_key(repository_key)
            .host(HostName::new("incorrect-source-host"))
            .attach(name)
            .phase(SessionPhase::Running)
            .build();
        FleetReplicaSnapshot {
            host,
            generation: Some(generation.to_string()),
            rows: Vec::new(),
            result_sets: vec![ResultSet { seq: 1, rows: Rows::Independents { scope: None, rows: vec![row] }, state: Default::default() }],
        }
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
            },
            spec: ConvoySpec::builder().workflow_ref("scratch".to_string()).build(),
            status: Some(ConvoyStatus {
                phase: convoy_phase,
                workflow_snapshot: Some(WorkflowSnapshot { vessels: vec![definition] }),
                work,
                ..Default::default()
            }),
        }
    }

    async fn emitted_vessel(convoy: ResourceObject<Convoy>) -> VesselRow {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(1);
        let mut aggregator = Aggregator::new(state, HostName::new("local"), tx);

        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Added(convoy)).await;

        let DaemonEvent::ResultSet(result_set) = rx.recv().await.expect("initial result set") else {
            panic!("expected result set");
        };
        let rows = result_set.rows.as_convoys().expect("convoy rows");
        let convoy = rows.first().expect("convoy row");
        convoy.vessels.first().expect("vessel row").clone()
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
        definition.stance = Stance::WorkspaceWrite;
        definition
            .crew
            .push(CrewSpec::builder().role("coder".to_string()).source(CrewSource::Tool { command: "cargo test".to_string() }).build());
        status.work.get_mut("implement").expect("work state").placement = Some(PlacementStatus {
            fields: BTreeMap::from([
                ("requested_stance".to_string(), serde_json::json!("workspace-write")),
                ("effective_stance".to_string(), serde_json::json!("contained")),
            ]),
        });

        let vessel = emitted_vessel(convoy).await;

        assert_eq!(vessel.requested_stance.as_deref(), Some("workspace-write"));
        assert_eq!(vessel.effective_stance.as_deref(), Some("contained"));
        assert_eq!(vessel.crew[0].requested_stance.as_deref(), Some("workspace-write"));
        assert_eq!(vessel.crew[0].effective_stance.as_deref(), Some("contained"));
    }

    #[tokio::test]
    async fn non_terminal_work_is_completable() {
        for phase in [ResourceWorkPhase::Pending, ResourceWorkPhase::Ready, ResourceWorkPhase::Launching, ResourceWorkPhase::Running] {
            let vessel = emitted_vessel(convoy_with_work().convoy_phase(ResourceConvoyPhase::Active).work_phase(phase).call()).await;

            assert!(vessel.complete_work, "{phase:?} work must expose the completion override");
        }
    }

    #[tokio::test]
    async fn replica_replacement_emits_removed_and_changed_rows() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "old")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::ResultSet(_)));

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-2", "new")]).await;
        let DaemonEvent::ResultDelta(delta) = rx.recv().await.expect("replacement event") else { panic!("expected result delta") };
        assert_eq!(delta.changes.as_convoys().expect("convoy changes").iter().map(|row| row.name.as_str()).collect::<Vec<_>>(), vec![
            "new"
        ]);
        let removed = delta.changes.removed_resources().expect("convoy removals");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "old");

        let result_set = state.result_set().await;
        assert_eq!(convoy_names(&result_set.rows), vec!["new"]);
    }

    #[tokio::test]
    async fn replica_cache_preserves_project_ref() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "remote-convoy")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::ResultSet(_)));

        let result_set = state.result_set().await;
        let rows = result_set.rows.as_convoys().expect("convoy rows");
        let row = rows.first().expect("replica convoy row");
        assert_eq!(row.project_ref.as_deref(), Some("my-project"));
    }

    #[tokio::test]
    async fn replica_cache_merges_repository_checkout_rows_and_sets_origin_host() {
        let state = AggregatorProjectionState::new();
        let repo = RepositoryKey("repo-widgets".into());
        state.replace_store_catalog(HashSet::from([repo.clone()]), HashMap::new()).await;
        let scope = None;
        let row = CheckoutRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "remote-checkout"))
            .repo(repo)
            .path("/srv/widgets")
            .branch("main")
            .host(HostName::new("incorrect-source-host"))
            .authority(LifecycleAuthority::Observed)
            .build();
        let snapshot = FleetReplicaSnapshot {
            host: HostName::new("kiwi"),
            generation: None,
            rows: vec![],
            result_sets: vec![ResultSet {
                seq: 4,
                rows: Rows::Checkouts { scope: scope.clone(), rows: vec![row] },
                state: ResultSetState::default(),
            }],
        };
        let (event_tx, _) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), event_tx);

        aggregator.apply_replica_cache(vec![snapshot]).await;

        let set = state.result_set_for(&QueryId::Checkouts { scope }).await.expect("checkout result set");
        let rows = set.rows.as_checkouts().expect("checkout rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host, HostName::new("kiwi"));
        assert_eq!(rows[0].resource.host, Some(HostName::new("kiwi")));
    }

    #[tokio::test]
    async fn replica_cache_unions_independents_and_stamps_origin_host() {
        let state = AggregatorProjectionState::new();
        let repository = RepositoryKey("repo-flotilla".into());
        let scope = QueryScope::new("flotilla", "flotilla");
        state.replace_store_catalog(HashSet::from([repository.clone()]), HashMap::from([(scope.clone(), vec![repository.clone()])])).await;
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator
            .apply_replica_cache(vec![remote_independent_snapshot("feta", "generation-1", "terminal-yeoman", Some(repository.clone()))])
            .await;
        let event = rx.recv().await.expect("independents replica event");
        assert!(
            matches!(event, DaemonEvent::ResultDelta(ref delta) if delta.query() == (QueryId::Independents { scope: None })),
            "unexpected first replica event: {event:?}",
        );

        let result_set = state.independents_result_set(&Some(scope)).await;
        let rows = result_set.rows.as_independents().expect("independent rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].resource.host, Some(HostName::new("feta")));
        assert_eq!(rows[0].host, HostName::new("feta"));
    }

    #[tokio::test]
    async fn complete_replica_cache_removes_hosts_missing_from_next_refresh() {
        let state = AggregatorProjectionState::new();
        let (tx, mut rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(state.clone(), HostName::new("local"), tx);

        aggregator.apply_replica_cache(vec![remote_snapshot("feta", "generation-1", "feta-convoy")]).await;
        assert!(matches!(rx.recv().await.expect("initial event"), DaemonEvent::ResultSet(_)));

        aggregator.apply_replica_cache(Vec::new()).await;
        let DaemonEvent::ResultDelta(delta) = rx.recv().await.expect("removal event") else { panic!("expected result delta") };
        assert!(delta.changes.as_convoys().expect("convoy changes").is_empty());
        let removed = delta.changes.removed_resources().expect("convoy removals");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "feta-convoy");
        assert!(state.result_set().await.rows.is_empty());
    }

    #[tokio::test]
    async fn restart_relist_removes_local_rows_missing_from_stores() {
        let durable = ResourceBackend::InMemory(InMemoryBackend::default());
        let observed = ResourceBackend::InMemory(InMemoryBackend::observed());
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
        let (replica_tx, replica_rx) = broadcast::channel(1);
        drop(replica_tx);

        let result = Aggregator::new(state.clone(), HostName::new("local"), event_tx)
            .run(
                AggregatorResolvers::builder()
                    .durable_convoys(durable.clone().using::<Convoy>("flotilla"))
                    .durable_environments(durable.clone().using::<Environment>("flotilla"))
                    .durable_presentations(durable.clone().using::<Presentation>("flotilla"))
                    .durable_sessions(durable.using::<TerminalSession>("flotilla"))
                    .durable_projects(durable.using::<Project>("flotilla"))
                    .durable_repositories(durable.using::<Repository>("flotilla"))
                    .observed_convoys(observed.clone().using::<Convoy>("flotilla"))
                    .observed_presentations(observed.clone().using::<Presentation>("flotilla"))
                    .observed_sessions(observed.using::<TerminalSession>("flotilla"))
                    .observed_checkouts(observed.using::<Checkout>("flotilla"))
                    .build(),
                replica_rx,
            )
            .await;

        assert!(result.expect_err("closed channel should stop the run").to_string().contains("replica channel closed"));
        let DaemonEvent::ResultSet(result_set) = event_rx.recv().await.expect("relist snapshot") else {
            panic!("expected relist result set");
        };
        assert!(result_set.rows.is_empty());
        assert!(state.result_set().await.rows.is_empty());
    }

    #[tokio::test]
    async fn expired_convoy_watch_relists_its_source_and_removes_missed_deletion() {
        let stale = convoy_object("deleted-while-watch-expired").await;
        let durable_convoys = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![stale], resource_version: "1".to_string(), generation: None }, empty_list()],
            vec![Ok(expiring_watch()), Ok(pending_watch())],
        ));
        let durable_presentations = Arc::new(ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_convoys = Arc::new(ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_presentations = Arc::new(ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let (_replica_tx, replica_rx) = broadcast::channel(1);

        let run_durable_convoys = Arc::clone(&durable_convoys);
        let run_durable_presentations = Arc::clone(&durable_presentations);
        let run_observed_convoys = Arc::clone(&observed_convoys);
        let run_observed_presentations = Arc::clone(&observed_presentations);
        let run_state = state.clone();
        let task = tokio::spawn(async move {
            run_with_test_sources(
                Aggregator::new(run_state, HostName::new("local"), event_tx),
                run_durable_convoys.as_ref(),
                run_durable_presentations.as_ref(),
                run_observed_convoys.as_ref(),
                run_observed_presentations.as_ref(),
                replica_rx,
            )
            .await
        });

        let initial = recv_query_event(&mut event_rx, QueryId::Convoys, "initial result set timeout").await;
        let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial result set") };
        assert_eq!(convoy_names(&initial.rows), vec!["deleted-while-watch-expired"]);

        let removal = recv_query_event(&mut event_rx, QueryId::Convoys, "relist delta timeout").await;
        let DaemonEvent::ResultDelta(removal) = removal else { panic!("expected relist delta") };
        let removed = removal.changes.removed_resources().expect("convoy removals");
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].name, "deleted-while-watch-expired");
        assert!(state.result_set().await.rows.is_empty());
        assert_eq!(durable_convoys.list_calls.load(Ordering::SeqCst), 2);
        assert_eq!(durable_convoys.watch_calls.load(Ordering::SeqCst), 2);
        assert_eq!(observed_convoys.watch_calls.load(Ordering::SeqCst), 1, "healthy watch must not restart");
        assert!(!task.is_finished(), "aggregator should remain alive after in-place relist");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn expired_session_watch_relists_its_source_and_removes_missed_deletion() {
        let stale = session_object("deleted-while-watch-expired").await;
        let durable_convoys = Arc::new(ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_environments = Arc::new(ScriptedSource::<Environment>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_presentations = Arc::new(ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_sessions = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![stale], resource_version: "1".to_string(), generation: None }, empty_list()],
            vec![Ok(expiring_watch()), Ok(pending_watch())],
        ));
        let durable_projects = Arc::new(ScriptedSource::<Project>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let durable_repositories = Arc::new(ScriptedSource::<Repository>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_convoys = Arc::new(ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_presentations = Arc::new(ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_sessions = Arc::new(ScriptedSource::<TerminalSession>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_checkouts = Arc::new(ScriptedSource::<Checkout>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let (_replica_tx, replica_rx) = broadcast::channel(1);

        let run_state = state.clone();
        let run_durable_sessions = Arc::clone(&durable_sessions);
        let task = tokio::spawn(async move {
            let sources = AggregatorSourceRefs::builder()
                .durable_convoys(durable_convoys.as_ref())
                .durable_environments(durable_environments.as_ref())
                .durable_presentations(durable_presentations.as_ref())
                .durable_sessions(run_durable_sessions.as_ref())
                .durable_projects(durable_projects.as_ref())
                .durable_repositories(durable_repositories.as_ref())
                .observed_convoys(observed_convoys.as_ref())
                .observed_presentations(observed_presentations.as_ref())
                .observed_sessions(observed_sessions.as_ref())
                .observed_checkouts(observed_checkouts.as_ref())
                .build();
            Aggregator::new(run_state, HostName::new("local"), event_tx).run_with_sources(sources, replica_rx).await
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
        let durable_presentations = Arc::new(ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_convoys = Arc::new(ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_presentations = Arc::new(ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let (_replica_tx, replica_rx) = broadcast::channel(1);

        let run_durable_convoys = Arc::clone(&durable_convoys);
        let run_state = state.clone();
        let task = tokio::spawn(async move {
            run_with_test_sources(
                Aggregator::new(run_state, HostName::new("local"), event_tx),
                run_durable_convoys.as_ref(),
                durable_presentations.as_ref(),
                observed_convoys.as_ref(),
                observed_presentations.as_ref(),
                replica_rx,
            )
            .await
        });

        let initial = recv_query_event(&mut event_rx, QueryId::Convoys, "initial result set timeout").await;
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
    async fn expired_presentation_watch_relists_its_source_and_removes_stale_attach() {
        let convoy = convoy_with_vessel("convoy-a").await;
        let presentation = presentation_object("convoy-a-implement", "convoy-a", "implement", Some("workspace-1")).await;
        let durable_convoys = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![convoy], resource_version: "1".to_string(), generation: None }],
            vec![Ok(pending_watch())],
        ));
        let durable_presentations = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![presentation], resource_version: "1".to_string(), generation: None }, empty_list()],
            vec![Ok(expiring_watch()), Ok(pending_watch())],
        ));
        let observed_convoys = Arc::new(ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_presentations = Arc::new(ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let (_replica_tx, replica_rx) = broadcast::channel(1);

        let run_durable_convoys = Arc::clone(&durable_convoys);
        let run_durable_presentations = Arc::clone(&durable_presentations);
        let run_observed_convoys = Arc::clone(&observed_convoys);
        let run_observed_presentations = Arc::clone(&observed_presentations);
        let run_state = state.clone();
        let task = tokio::spawn(async move {
            run_with_test_sources(
                Aggregator::new(run_state, HostName::new("local"), event_tx),
                run_durable_convoys.as_ref(),
                run_durable_presentations.as_ref(),
                run_observed_convoys.as_ref(),
                run_observed_presentations.as_ref(),
                replica_rx,
            )
            .await
        });

        let initial = recv_query_event(&mut event_rx, QueryId::Convoys, "initial result set timeout").await;
        let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial result set") };
        let initial_row = initial.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
        assert_eq!(initial_row.vessels.first().expect("vessel row").attach.as_deref(), Some("workspace-1"));

        let update = recv_query_event(&mut event_rx, QueryId::Convoys, "relist delta timeout").await;
        let DaemonEvent::ResultDelta(update) = update else { panic!("expected relist delta") };
        let changed = update.changes.as_convoys().expect("changed convoy rows").first().expect("changed convoy row");
        assert_eq!(changed.vessels.first().expect("changed vessel row").attach, None);
        assert!(update.changes.removed_resources().expect("convoy removals").is_empty());
        assert_eq!(durable_presentations.list_calls.load(Ordering::SeqCst), 2);
        assert_eq!(durable_presentations.watch_calls.load(Ordering::SeqCst), 2);
        assert_eq!(durable_convoys.watch_calls.load(Ordering::SeqCst), 1, "healthy watch must not restart");
        assert!(!task.is_finished(), "aggregator should remain alive after presentation relist");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn convoy_deletion_removes_its_presentation_workspace_from_the_join() {
        let convoy = convoy_with_vessel("convoy-a").await;
        let presentation = presentation_object("convoy-a-implement", "convoy-a", "implement", Some("workspace-1")).await;
        let (event_tx, _event_rx) = broadcast::channel(8);
        let mut aggregator = Aggregator::new(AggregatorProjectionState::new(), HostName::new("local"), event_tx);
        let key = ("flotilla".to_string(), "convoy-a".to_string(), "implement".to_string());

        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Added(convoy.clone())).await;
        aggregator.apply_presentation_event_from(LocalSource::Durable, WatchEvent::Added(presentation)).await;
        assert_eq!(aggregator.presentation_workspaces.get(&key).map(String::as_str), Some("workspace-1"));

        aggregator.apply_convoy_event_from(LocalSource::Durable, WatchEvent::Deleted(convoy)).await;

        assert!(!aggregator.presentation_workspaces.contains_key(&key));
    }

    #[tokio::test]
    async fn observed_presentation_without_workspace_masks_durable_attach() {
        let convoy = convoy_with_vessel("convoy-a").await;
        let durable_presentation = presentation_object("convoy-a-implement", "convoy-a", "implement", Some("stale-workspace")).await;
        let observed_presentation = presentation_object("convoy-a-implement", "convoy-a", "implement", None).await;
        let durable_convoys = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![convoy], resource_version: "1".to_string(), generation: None }],
            vec![Ok(pending_watch())],
        ));
        let durable_presentations = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![durable_presentation], resource_version: "1".to_string(), generation: None }],
            vec![Ok(pending_watch())],
        ));
        let observed_convoys = Arc::new(ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]));
        let observed_presentations = Arc::new(ScriptedSource::new(
            vec![ResourceList { items: vec![observed_presentation], resource_version: "1".to_string(), generation: None }],
            vec![Ok(pending_watch())],
        ));
        let state = AggregatorProjectionState::new();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        let (_replica_tx, replica_rx) = broadcast::channel(1);

        let run_state = state.clone();
        let task = tokio::spawn(async move {
            run_with_test_sources(
                Aggregator::new(run_state, HostName::new("local"), event_tx),
                durable_convoys.as_ref(),
                durable_presentations.as_ref(),
                observed_convoys.as_ref(),
                observed_presentations.as_ref(),
                replica_rx,
            )
            .await
        });

        let initial = recv_query_event(&mut event_rx, QueryId::Convoys, "initial result set timeout").await;
        let DaemonEvent::ResultSet(initial) = initial else { panic!("expected initial result set") };
        let row = initial.rows.as_convoys().expect("convoy rows").first().expect("convoy row");
        assert_eq!(row.vessels.first().expect("vessel row").attach, None);
        assert!(state.result_set().await.rows.as_convoys().expect("convoy rows")[0].vessels[0].attach.is_none());
        assert!(!task.is_finished(), "aggregator should remain alive");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn non_expiry_watch_error_still_exits_aggregator() {
        let durable_convoys = ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(failing_watch("convoy watch failed"))]);
        let durable_presentations = ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_convoys = ScriptedSource::<Convoy>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let observed_presentations = ScriptedSource::<Presentation>::new(vec![empty_list()], vec![Ok(pending_watch())]);
        let state = AggregatorProjectionState::new();
        let (event_tx, _event_rx) = broadcast::channel(8);
        let (_replica_tx, replica_rx) = broadcast::channel(1);

        let result = timeout(
            Duration::from_secs(1),
            run_with_test_sources(
                Aggregator::new(state, HostName::new("local"), event_tx),
                &durable_convoys,
                &durable_presentations,
                &observed_convoys,
                &observed_presentations,
                replica_rx,
            ),
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
}
