use flotilla_observation::convoy_branch_refresh::ConvoyBranchRefresh;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use flotilla_change_request::observation::ChangeRequestKey;
use flotilla_discovery::environment_manager::EnvironmentManager;
use flotilla_issue_tracker::IssueProvider;
use flotilla_projections::{
    ports::EnvironmentSummaries, read_projections::LeafSubscriptionRead, AttachCapabilityResolver, ConvoyChangeRequestResolver,
    IssueMaterializationResolver,
};
use flotilla_protocol::{EnvironmentInfo, HostName, IssueSource, NodeId, QueryScope, RepositoryKey, ToolInventory};
use flotilla_provider_runtime::ObservationError;
use flotilla_resources::{BoundChangeRequest, LeafFiringRecord, LeafSubscriptionRow, ResolvedIssueSourceBinding};

use super::InProcessDaemon;
use flotilla_crew::leaf_engine::LeafSubscriptionTable;

#[async_trait]
impl ConvoyChangeRequestResolver for InProcessDaemon {
    async fn refresh_branch(
        &self,
        repositories: &[RepositoryKey],
        branch: &str,
        binding: Option<&BoundChangeRequest>,
    ) -> ConvoyBranchRefresh {
        self.refresh_convoy_branch(repositories, branch, binding).await
    }

    async fn discover_branch_subjects(
        &self,
        namespace: &str,
        convoy: &str,
        branch: &str,
        resolution: ConvoyBranchRefresh,
    ) -> Result<(), String> {
        self.discover_convoy_branch_subjects_with_resolution(namespace, convoy, branch, Some(&resolution)).await
    }

    async fn change_request_changes(&self) -> Option<futures::stream::BoxStream<'static, ()>> {
        match self.observation_records.get()?.changes().await {
            Ok(changes) => Some(changes),
            Err(error) => {
                tracing::warn!(%error, "follow the change request records; convoys without a change request are not looked up again");
                None
            }
        }
    }
}

#[async_trait]
impl AttachCapabilityResolver for InProcessDaemon {
    async fn resolvable_attach_targets(&self, targets: &[(String, HostName)]) -> Result<Vec<bool>, String> {
        self.resolvable_attach_targets_internal(targets).await
    }

    async fn origin_host_names(&self, origins: &HashSet<NodeId>) -> HashMap<NodeId, HostName> {
        self.origin_host_names_internal(origins).await
    }
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

pub(super) struct ProjectionEnvironmentSummaries<'a>(pub &'a EnvironmentManager);

#[async_trait]
impl EnvironmentSummaries for ProjectionEnvironmentSummaries<'_> {
    fn local_inventory(&self) -> ToolInventory {
        crate::convert::inventory_from_bag(&self.0.local_environment_bag())
    }
    async fn host_summary_environments(&self) -> Vec<EnvironmentInfo> {
        self.0.host_summary_environments().await
    }
    async fn visible_environments(&self) -> Vec<EnvironmentInfo> {
        self.0.visible_environments().await
    }
}

/// Read projections' view of the crew service's shared leaf subscription table.
pub(super) struct LeafSubscriptionDiagnostics(pub LeafSubscriptionTable);

#[async_trait]
impl LeafSubscriptionRead for LeafSubscriptionDiagnostics {
    fn change_request_stale_after(&self) -> Duration {
        self.0.change_request_stale_after()
    }

    async fn change_request_observation_error(&self, subject: &ChangeRequestKey) -> Option<ObservationError> {
        self.0.change_request_observation_error(subject).await
    }

    async fn diagnostics(&self) -> Vec<(LeafSubscriptionRow, Vec<LeafFiringRecord>)> {
        self.0.diagnostics().await
    }
}

#[async_trait]
impl EnvironmentSummaries for InProcessDaemon {
    fn local_inventory(&self) -> ToolInventory {
        ProjectionEnvironmentSummaries(&self.environment_manager).local_inventory()
    }
    async fn host_summary_environments(&self) -> Vec<EnvironmentInfo> {
        ProjectionEnvironmentSummaries(&self.environment_manager).host_summary_environments().await
    }
    async fn visible_environments(&self) -> Vec<EnvironmentInfo> {
        ProjectionEnvironmentSummaries(&self.environment_manager).visible_environments().await
    }
}

/// Receiver evidence, and the Messages on the engine path when the
/// `[primitives]` switch is on.
pub(super) struct MessageReceivers(pub(super) flotilla_engine_messages::MessagePath);
#[async_trait]
impl flotilla_projections::read_projections::MessageReceiverLoader for MessageReceivers {
    async fn load(
        &self,
        backend: &flotilla_store::ResourceBackend,
        namespace: &str,
        convoys: Vec<flotilla_resources::ReadResourceObject<flotilla_resources::Convoy>>,
        terminals: Vec<flotilla_resources::ReadResourceObject<flotilla_resources::TerminalSession>>,
    ) -> Result<Box<dyn flotilla_projections::read_projections::MessageReceiverRead>, String> {
        let messages = match self.0.get() {
            // No message reads in the old shape until the fleet is designated.
            Some(primitives) if primitives.delivery_wait(namespace).await?.is_some() => Some(Vec::new()),
            Some(primitives) => Some(primitives.views(namespace).await.map_err(|e| e.to_string())?),
            None => None,
        };
        Ok(Box::new(MessageReceiverEvidence(
            flotilla_messaging::MessageReceiverSnapshot::load_from_observations(backend, namespace, convoys, terminals)
                .await
                .map_err(|e| e.to_string())?,
            messages,
        )))
    }
}
pub(super) struct MessageReceiverEvidence(
    pub flotilla_messaging::MessageReceiverSnapshot,
    pub Option<Vec<flotilla_resources::ReadResourceObject<flotilla_resources::Message>>>,
);
impl flotilla_projections::read_projections::MessageReceiverRead for MessageReceiverEvidence {
    fn terminals(&self) -> &[flotilla_resources::ReadResourceObject<flotilla_resources::TerminalSession>] {
        self.0.terminals()
    }
    fn may_address_convoy(&self, address: &str, project: &str, convoy: &str) -> bool {
        self.0.may_address_convoy(address, project, convoy)
    }
    fn resolve(
        &self,
        address: &str,
    ) -> Result<Option<flotilla_resources::ReadResourceObject<flotilla_resources::TerminalSession>>, String> {
        self.0.resolve(address).map_err(|e| e.to_string())
    }
    fn messages(&self) -> Option<&[flotilla_resources::ReadResourceObject<flotilla_resources::Message>]> {
        self.1.as_deref()
    }
}

/// The decided store's AgentSessions (#3108): this home's and every followed home's replicas,
/// followed through the store's commits.
#[async_trait]
impl flotilla_projections::ports::AgentSessions for InProcessDaemon {
    async fn follow(&self) -> Result<flotilla_projections::ports::AgentSessionFollow, String> {
        agent_sessions::follow(self.decided_store()?).await
    }
}

pub use agent_sessions::DecidedAgentSessions;

/// Provisioning readings whose AgentSessions are the daemon's own.
pub(super) struct WithAgentSessions {
    pub(super) reads: Arc<dyn flotilla_resources::lifecycle_reads::LifecycleReads>,
    pub(super) agent_sessions: DecidedAgentSessions,
}

impl flotilla_resources::lifecycle_reads::LifecycleReads for WithAgentSessions {
    fn is_disabled(&self) -> bool {
        self.reads.is_disabled()
    }

    fn try_reading(
        &self,
        kind: flotilla_resources::lifecycle_reads::ProvisioningKind,
        name: &str,
        origin: Option<&str>,
    ) -> Result<Option<flotilla_protocol::LifecycleReading>, String> {
        self.reads.try_reading(kind, name, origin)
    }

    fn environments(&self) -> Result<&dyn flotilla_resources::environment_records::EnvironmentRecords, String> {
        self.reads.environments()
    }

    fn clones(&self) -> Result<&dyn flotilla_resources::clone_records::CloneRecords, String> {
        self.reads.clones()
    }

    fn checkouts(&self) -> Result<&dyn flotilla_resources::checkout_records::CheckoutRecords, String> {
        self.reads.checkouts()
    }

    fn agent_sessions(&self) -> Result<&dyn flotilla_resources::agent_activity::AgentSessionReads, String> {
        Ok(&self.agent_sessions)
    }

    fn terminals(&self) -> Result<&dyn flotilla_resources::terminal_records::TerminalRecords, String> {
        self.reads.terminals()
    }
}

mod agent_sessions {
    use std::{collections::BTreeMap, sync::Arc};

    use chrono::{DateTime, Utc};
    use flotilla_engine_store::follow::Followed;
    use flotilla_engine_store::watch::EngineWatch;
    use flotilla_projections::ports::{AgentSessionFollow, AgentSessionKey, AgentSessionUpdate};
    use flotilla_protocol::{AgentActivity, AgentSessionEnd, AgentSessionReading, NodeId};
    use flotilla_resources::agent_activity::{home_current, AgentActivityView, AgentSessionReads, ProgressFacts};
    use flotilla_schema::Kind as StoredKind;
    use flotilla_schema_sqlite::{stream::Service, Read, StoreError};
    use flotilla_state_machines::{
        machines::agent_session::{Activity, AgentSession, EndReason},
        Resource,
    };
    use futures::StreamExt;

    const CODE: flotilla_schema::KindCode = <Resource<AgentSession> as StoredKind>::CODE;

    pub(super) async fn follow(service: Arc<Service>) -> Result<AgentSessionFollow, String> {
        let home = NodeId::new(service.home());
        // Started first: a write landing before the read below still reaches the stream.
        let watch = EngineWatch::start_hearing(Arc::clone(&service), [CODE]).map_err(|error| error.to_string())?;
        let homes = service.homes();
        let held = watch
            .read(move |read| {
                let mut held = Vec::new();
                for home in homes {
                    held.extend(
                        scan(read, &home)?.into_iter().map(|(name, reading)| (AgentSessionKey { home: NodeId::new(&home), name }, reading)),
                    );
                }
                Ok(held)
            })
            .await
            .map_err(|error| error.to_string())?;
        let heard = heard_from(&service);
        let held_heard = heard.iter().map(|(home, at)| (home.clone(), *at)).collect();
        // A commit that hears from another home wakes the watch, a keepalive-only round
        // included, so each home's heard time is read again then: one wake per keepalive, and no
        // timer.
        let updates = futures::stream::unfold((watch, service, heard), |(mut watch, service, mut last)| async move {
            loop {
                // A pass that fails is tried again at the next commit.
                let mut found = match watch.next(updates).await {
                    Ok(found) => found,
                    Err(error) => {
                        tracing::warn!(%error, "the AgentSession follow could not read the log; retrying");
                        Vec::new()
                    }
                };
                for (home, at) in heard_from(&service) {
                    if last.get(&home) != Some(&at) {
                        last.insert(home.clone(), at);
                        found.push(AgentSessionUpdate::Heard { home, at });
                    }
                }
                if !found.is_empty() {
                    return Some((futures::stream::iter(found), (watch, service, last)));
                }
            }
        })
        .flatten()
        .boxed();
        Ok(AgentSessionFollow { home, held, heard: held_heard, updates })
    }

    /// When this store last heard from each other home it follows: the home time its latest
    /// applied keepalive carried. A home never heard from is left out.
    fn heard_from(service: &Service) -> BTreeMap<NodeId, DateTime<Utc>> {
        service
            .homes()
            .into_iter()
            .filter(|home| home != service.home())
            .filter_map(|home| {
                let at = service.watermark(&home).ok()?.applied_through.to_datetime()?;
                (at > DateTime::<Utc>::UNIX_EPOCH).then(|| (NodeId::new(&home), at))
            })
            .collect()
    }

    /// Keyed reads of the decided store's AgentSessions, with whether each one's home is current
    /// by the daemon's clock.
    pub struct DecidedAgentSessions {
        service: Arc<Service>,
        clock: Arc<dyn flotilla_resources::Clock>,
        progress: Arc<ProgressFacts>,
    }

    impl DecidedAgentSessions {
        /// Reads `service`'s AgentSessions, joined with what this daemon has seen of its own
        /// sessions' progress.
        pub fn new(service: Arc<Service>, clock: Arc<dyn flotilla_resources::Clock>, progress: Arc<ProgressFacts>) -> Self {
            Self { service, clock, progress }
        }
    }

    impl AgentSessionReads for DecidedAgentSessions {
        fn read(&self, name: &str, origin: Option<&str>) -> Result<Option<AgentActivityView>, String> {
            let own = self.service.home();
            let home = origin.unwrap_or(own);
            let reading = self
                .service
                .with_store(|store| load(&mut store.read()?, name, home))
                .map_err(|error| format!("AgentSession/{name}: {error}"))?;
            let current = home == own || {
                let heard = self.service.watermark(home).map_err(|error| error.to_string())?.applied_through.to_datetime();
                home_current(heard.filter(|at| *at > DateTime::<Utc>::UNIX_EPOCH), self.clock.now())
            };
            // Progress is kept by session as hooks name it; a launch reads its session's, which an
            // earlier launch's can only precede.
            let progress = (home == own).then(|| self.progress.get(flotilla_resources::terminal_records::launched(name)));
            Ok(reading.map(|reading| AgentActivityView { reading, current, progress }))
        }
    }

    /// What one pass over the log found, in log order: written sessions as they are now, and
    /// a home's whole set where it must be reread.
    fn updates(read: &mut Read<'_>, followed: Vec<Followed>) -> Result<Vec<AgentSessionUpdate>, StoreError> {
        let mut updates = Vec::new();
        let mut changed = Vec::new();
        for followed in followed {
            match followed {
                Followed::Changed(identity) => {
                    let reading = load(read, &identity.name, &identity.home)?;
                    changed.push((AgentSessionKey { home: NodeId::new(&identity.home), name: identity.name }, reading));
                }
                Followed::Rescan(home) => {
                    if !changed.is_empty() {
                        updates.push(AgentSessionUpdate::Changed(std::mem::take(&mut changed)));
                    }
                    let sessions = scan(read, &home)?;
                    updates.push(AgentSessionUpdate::Home { home: NodeId::new(&home), sessions });
                }
            }
        }
        if !changed.is_empty() {
            updates.push(AgentSessionUpdate::Changed(changed));
        }
        Ok(updates)
    }

    fn load(read: &mut Read<'_>, name: &str, home: &str) -> Result<Option<AgentSessionReading>, StoreError> {
        let Some(id) = read.resolve(CODE, name, home)? else { return Ok(None) };
        Ok(read.load::<Resource<AgentSession>>(id)?.map(|loaded| reading(&loaded.value)))
    }

    fn scan(read: &mut Read<'_>, home: &str) -> Result<Vec<(String, AgentSessionReading)>, StoreError> {
        let mut sessions = Vec::new();
        let mut after = None;
        loop {
            let page = read.scan::<Resource<AgentSession>>(home, after, 64)?;
            for id in page.ids {
                if let Some(loaded) = read.load::<Resource<AgentSession>>(id)? {
                    sessions.push((loaded.value.name().to_owned(), reading(&loaded.value)));
                }
            }
            after = page.next;
            if after.is_none() {
                return Ok(sessions);
            }
        }
    }

    fn reading(session: &Resource<AgentSession>) -> AgentSessionReading {
        let state = session.state();
        let observation = state.observation();
        AgentSessionReading {
            ended: state.end_reason().map(|reason| match reason {
                EndReason::Completed => AgentSessionEnd::Completed,
                EndReason::Lost => AgentSessionEnd::Lost,
            }),
            activity: observation.as_ref().map(|observation| match observation.activity {
                Activity::Working => AgentActivity::Working,
                Activity::NeedsInput => AgentActivity::NeedsInput,
                Activity::Idle => AgentActivity::Idle,
                Activity::Interrupted => AgentActivity::Interrupted,
            }),
            activity_since: observation.as_ref().and_then(|observation| observation.activity_since.to_datetime()),
            activity_cause: observation.map(|observation| observation.activity_cause),
            stale: false,
        }
    }
}
