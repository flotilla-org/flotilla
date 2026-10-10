use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use flotilla_core::{
    dispatch_missions::{MissionBoard, MissionBoardIndex, MissionSourceSnapshot},
    in_process::InProcessDaemon,
};
use flotilla_protocol::{
    issue_query::{IssueQuery, READY_ISSUE_LABEL},
    DispatchIssueFacts, Issue, IssueRef, IssueState,
};
use flotilla_resources::{
    content_hash, pinned_workflow_ref, Clock, Convoy, ConvoyPhase, DispatchDeployment, DispatchHold, DispatchHoldStatusPatch,
    DispatchObservation, DispatchObservationSpec, DispatchPolicy, DispatchQueueAttention, DispatchQueueEntry, HoldClearWhen, InputMeta,
    Project, ProjectStatusPatch, ReadWatchEvent, ResolvedIssueSourceBinding, ResourceError, ResourceObject, SystemClock, WorkflowTemplate,
    DISPATCH_RECONCILER_PROVENANCE,
};
use flotilla_store::{apply_status_patch, ResourceBackend};
use futures::{stream::BoxStream, FutureExt, StreamExt};
use tracing::{info, warn};

const ISSUE_PAGE_SIZE: usize = 100;

pub(crate) struct ProjectBoardInput {
    readiness: Result<Vec<Issue>, String>,
    boards: Vec<MissionSourceSnapshot>,
}
type ProjectBoards = BTreeMap<String, Result<ProjectBoardInput, String>>;

#[derive(Default)]
struct MissionState {
    boards: MissionBoardIndex,
    convoys: BTreeMap<String, Arc<ResourceObject<Convoy>>>,
    watch: Option<BoxStream<'static, Result<ReadWatchEvent<Convoy>, ResourceError>>>,
    occupancy: BTreeMap<String, Occupancy>,
    occupancy_rebuilds: usize,
    convoy_lists: usize,
}

struct Occupancy {
    board: Arc<MissionBoard>,
    policy: DispatchPolicy,
    project_active: usize,
    missions: BTreeMap<String, usize>,
}

#[derive(Default)]
struct PassIssueReads {
    facts: BTreeMap<IssueRef, Result<DispatchIssueFacts, String>>,
    issues: BTreeMap<IssueRef, Result<Issue, String>>,
}

// An unavailable shared inventory conservatively pauses enabled Projects.
// Publish per-Project errors without discarding queues; disabled scopes still clear.
struct PassInventories {
    error: Option<String>,
    project_convoys: BTreeMap<String, Vec<Arc<ResourceObject<Convoy>>>>,
    landed_issues: HashSet<IssueRef>,
    holds: Vec<ResourceObject<DispatchHold>>,
    deployments: Vec<ResourceObject<DispatchDeployment>>,
    workflows: HashSet<String>,
    observations: HashSet<String>,
}

#[async_trait]
trait DispatchResourceSource: Send + Sync {
    async fn holds(&self) -> Result<Vec<ResourceObject<DispatchHold>>, String>;
    async fn deployments(&self) -> Result<Vec<ResourceObject<DispatchDeployment>>, String>;
    async fn workflows(&self) -> Result<HashSet<String>, String>;
    async fn observations(&self) -> Result<HashSet<String>, String>;
}

struct BackendDispatchResources {
    backend: ResourceBackend,
    namespace: String,
}

#[async_trait]
impl DispatchResourceSource for BackendDispatchResources {
    async fn holds(&self) -> Result<Vec<ResourceObject<DispatchHold>>, String> {
        self.backend.definitions::<DispatchHold>(&self.namespace).list().await.map_err(|e| e.to_string())
    }
    async fn deployments(&self) -> Result<Vec<ResourceObject<DispatchDeployment>>, String> {
        Ok(self
            .backend
            .including_replicas::<DispatchDeployment>(&self.namespace)
            .list()
            .await
            .map_err(|e| e.to_string())?
            .items
            .into_iter()
            .map(|record| record.object)
            .collect())
    }
    async fn workflows(&self) -> Result<HashSet<String>, String> {
        Ok(self
            .backend
            .definitions::<WorkflowTemplate>(&self.namespace)
            .list()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|object| object.metadata.name)
            .collect())
    }
    async fn observations(&self) -> Result<HashSet<String>, String> {
        Ok(self
            .backend
            .using::<DispatchObservation>(&self.namespace)
            .list()
            .await
            .map_err(|e| e.to_string())?
            .items
            .into_iter()
            .map(|object| object.metadata.name)
            .collect())
    }
}

#[async_trait]
pub(crate) trait DispatchIssueSource: Send + Sync {
    async fn collect_boards(&self, projects: &[ResourceObject<Project>]) -> Result<ProjectBoards, String>;
    async fn fetch_issue(&self, reference: &IssueRef) -> Result<Issue, String>;
    async fn dispatch_facts(&self, reference: &IssueRef) -> Result<DispatchIssueFacts, String>;
}

pub(crate) struct DaemonDispatchIssueSource {
    daemon: Arc<InProcessDaemon>,
}

impl DaemonDispatchIssueSource {
    pub(crate) fn new(daemon: Arc<InProcessDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait]
impl DispatchIssueSource for DaemonDispatchIssueSource {
    async fn collect_boards(&self, projects: &[ResourceObject<Project>]) -> Result<ProjectBoards, String> {
        let snapshot = self.daemon.collect_dispatch_board_inputs(projects).await?;
        let mut queries = BTreeMap::new();
        let mut providers = BTreeMap::new();
        let mut result = BTreeMap::new();
        for project in projects {
            let input = match &snapshot[&project.metadata.name] {
                Err(error) => Err(error.clone()),
                Ok((bindings, boards)) => {
                    let mut readiness = Ok(Vec::new());
                    if project.spec.dispatch_policy.as_ref().is_some_and(|policy| policy.enabled) {
                        for binding in bindings {
                            let query = ready_issue_query(binding);
                            let serialized = match serde_json::to_string(&query) {
                                Ok(serialized) => serialized,
                                Err(error) => {
                                    readiness = Err(error.to_string());
                                    break;
                                }
                            };
                            let key = (binding.source.clone(), serialized);
                            if !queries.contains_key(&key) {
                                if !providers.contains_key(&binding.source) {
                                    providers.insert(binding.source.clone(), self.daemon.issue_provider_for_source(&binding.source).await);
                                }
                                let items = match &providers[&binding.source] {
                                    Err(error) => Err(error.clone()),
                                    Ok(provider) => {
                                        let mut items = Vec::new();
                                        let mut page = 1;
                                        loop {
                                            match provider.query(&binding.source, &query, page, ISSUE_PAGE_SIZE).await {
                                                Err(error) => break Err(error),
                                                Ok(result) => {
                                                    items.extend(result.items);
                                                    if !result.has_more {
                                                        break Ok(items);
                                                    }
                                                    page += 1;
                                                }
                                            }
                                        }
                                    }
                                };
                                queries.insert(key.clone(), items);
                            }
                            match (&mut readiness, &queries[&key]) {
                                (Ok(items), Ok(observed)) => items.extend(observed.iter().cloned()),
                                (_, Err(error)) => {
                                    readiness = Err(error.clone());
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                    Ok(ProjectBoardInput { readiness, boards: boards.clone() })
                }
            };
            result.insert(project.metadata.name.clone(), input);
        }
        Ok(result)
    }
    async fn fetch_issue(&self, reference: &IssueRef) -> Result<Issue, String> {
        self.daemon.fetch_issue_by_ref(reference).await
    }
    async fn dispatch_facts(&self, reference: &IssueRef) -> Result<DispatchIssueFacts, String> {
        self.daemon.issue_provider_for_source(&reference.source).await?.dispatch_facts(reference).await
    }
}

fn ready_issue_query(binding: &ResolvedIssueSourceBinding) -> IssueQuery {
    IssueQuery {
        search: None,
        label: Some(READY_ISSUE_LABEL.to_string()),
        match_fields: binding.filter.match_fields.iter().map(|(field, value)| (field.clone(), value.to_values())).collect(),
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReconcilePass {
    pub queued: usize,
    pub blocked: usize,
    pub observations_recorded: usize,
    pub project_errors: usize,
}

pub(crate) struct DispatchReconciler {
    backend: ResourceBackend,
    namespace: String,
    issues: Arc<dyn DispatchIssueSource>,
    clock: Arc<dyn Clock>,
    mission: tokio::sync::Mutex<MissionState>,
    resources: Arc<dyn DispatchResourceSource>,
}

impl DispatchReconciler {
    pub(crate) fn new(backend: ResourceBackend, namespace: impl Into<String>, issues: Arc<dyn DispatchIssueSource>) -> Self {
        let namespace = namespace.into();
        let resources = Arc::new(BackendDispatchResources { backend: backend.clone(), namespace: namespace.clone() });
        Self { backend, namespace, issues, clock: Arc::new(SystemClock), mission: Default::default(), resources }
    }

    #[cfg(test)]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) async fn reconcile_once(&self) -> Result<ReconcilePass, String> {
        let projects = self.backend.clone().using::<Project>(&self.namespace).list().await.map_err(|error| error.to_string())?;
        let mut mission = self.mission.lock().await;
        let before = (mission.boards.rebuilds(), mission.boards.updated_issues(), mission.occupancy_rebuilds, mission.convoy_lists);
        let convoy_error = self.refresh_convoys(&mut mission).await.err();
        let mut project_convoys = BTreeMap::<String, Vec<Arc<ResourceObject<Convoy>>>>::new();
        let mut landed_issues = HashSet::new();
        for convoy in mission.convoys.values() {
            if convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed) {
                landed_issues.extend(convoy.spec.issues.iter().map(|issue| issue.reference.clone()));
            }
            if let Some(project) = &convoy.spec.project_ref {
                project_convoys.entry(project.clone()).or_default().push(convoy.clone());
            }
        }
        let holds = self.resources.holds().await;
        let deployments = self.resources.deployments().await;
        let workflows = self.resources.workflows().await;
        let observations = self.resources.observations().await;
        let error = convoy_error
            .or_else(|| holds.as_ref().err().cloned())
            .or_else(|| deployments.as_ref().err().cloned())
            .or_else(|| workflows.as_ref().err().cloned())
            .or_else(|| observations.as_ref().err().cloned());
        let inventories = PassInventories {
            error,
            project_convoys,
            landed_issues,
            holds: holds.unwrap_or_default(),
            deployments: deployments.unwrap_or_default(),
            workflows: workflows.unwrap_or_default(),
            observations: observations.unwrap_or_default(),
        };
        let local = self.backend.local_root().map_err(|e| e.to_string())?;
        let mut homes = BTreeMap::new();
        for record in
            self.backend.including_replicas::<Project>(&self.namespace).list_replica_sources().await.map_err(|e| e.to_string())?.items
        {
            let root = match record.provenance {
                flotilla_resources::ResourceProvenance::Local => local.clone(),
                flotilla_resources::ResourceProvenance::Replica { origin_root, .. } => origin_root,
            };
            let candidate = (record.object.metadata.creation_timestamp, root);
            let entry = homes.entry(record.object.metadata.name).or_insert_with(|| candidate.clone());
            *entry = candidate.min(entry.clone());
        }
        let projects = projects
            .items
            .into_iter()
            .filter(|project| homes.get(&project.metadata.name).is_some_and(|(_, root)| root == &local))
            .collect::<Vec<_>>();
        let enabled = projects
            .iter()
            .filter(|project| project.spec.dispatch_policy.as_ref().is_some_and(|policy| policy.enabled))
            .cloned()
            .collect::<Vec<_>>();
        let boards = self.issues.collect_boards(&enabled).await.unwrap_or_else(|error| {
            enabled
                .iter()
                .map(|project| (project.metadata.name.clone(), Err(format!("dispatch source inventory unavailable: {error}"))))
                .collect()
        });
        let scopes = enabled.iter().map(|project| project.metadata.name.clone()).collect::<BTreeSet<_>>();
        let mut sources = BTreeSet::new();
        for input in boards.values().filter_map(|input| input.as_ref().ok()) {
            for snapshot in &input.boards {
                sources.insert(snapshot.1.source.clone());
                mission.boards.apply_source(snapshot.1.source.clone(), Some(snapshot.clone()));
            }
        }
        // Unavailable scopes retain last-good evidence until they recover.
        if boards.values().all(Result::is_ok) {
            mission.boards.retain(&scopes, &sources);
        }
        mission.occupancy.retain(|name, _| scopes.contains(name));
        let mut total = ReconcilePass::default();
        let mut reads = PassIssueReads::default();
        for project in projects {
            match self.reconcile_project(&project, self.clock.now(), &boards, &inventories, &mut mission, &mut reads).await {
                Ok(outcome) => {
                    total.queued += outcome.queued;
                    total.blocked += outcome.blocked;
                    total.observations_recorded += outcome.observations_recorded;
                }
                Err(error) => {
                    warn!(project = %project.metadata.name, %error, "dispatch reconciliation failed for project; continuing pass");
                    // Retain readiness history and its attention clock while readers
                    // mark this scope unavailable; recovery must not reset aging.
                    if let Err(write_error) = self.set_queue_error(&project, Some(error)).await {
                        warn!(project = %project.metadata.name, %write_error, "could not publish dispatch source error; continuing pass");
                    }
                    total.project_errors += 1;
                }
            }
        }
        tracing::debug!(
            projects = scopes.len(),
            sources = sources.len(),
            graph_rebuilds = mission.boards.rebuilds() - before.0,
            mission_issue_updates = mission.boards.updated_issues() - before.1,
            occupancy_rebuilds = mission.occupancy_rebuilds - before.2,
            convoy_inventory_reads = mission.convoy_lists - before.3,
            "dispatch reconciliation work"
        );
        Ok(total)
    }

    async fn set_queue_error(&self, project: &ResourceObject<Project>, message: Option<String>) -> Result<(), String> {
        if project.status.as_ref().and_then(|status| status.dispatch_queue_error.as_ref()) == message.as_ref() {
            return Ok(());
        }
        apply_status_patch(
            &self.backend.clone().using::<Project>(&self.namespace),
            &project.metadata.name,
            &ProjectStatusPatch::DispatchQueueError { message },
        )
        .await
        .map(|_| ())
        .map_err(|error| error.to_string())
    }

    async fn reconcile_project(
        &self,
        project: &ResourceObject<Project>,
        now: DateTime<Utc>,
        boards: &ProjectBoards,
        inventories: &PassInventories,
        mission: &mut MissionState,
        reads: &mut PassIssueReads,
    ) -> Result<ReconcilePass, String> {
        let Some(policy) = project.spec.dispatch_policy.as_ref().filter(|policy| policy.enabled) else {
            self.replace_queue(project, Vec::new(), None).await?;
            self.set_queue_error(project, None).await?;
            return Ok(ReconcilePass::default());
        };

        if let Some(error) = &inventories.error {
            return Err(format!("dispatch pass inventory unavailable: {error}"));
        }
        let existing = inventories.project_convoys.get(&project.metadata.name).map(Vec::as_slice).unwrap_or_default();
        let held = self.active_holds(project, now, inventories, reads).await?;
        let previous_queue = project.status.as_ref().map(|status| status.dispatch_queue.as_slice()).unwrap_or_default();
        let observations_recorded = self.observe_dispatches(project, previous_queue, existing, now, inventories).await?;
        let dispatched = existing
            .iter()
            .filter(|convoy| !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()))
            .flat_map(|convoy| convoy.spec.issues.iter().map(|issue| issue.reference.clone()))
            .collect::<HashSet<_>>();
        let previous_by_issue = previous_queue.iter().map(|entry| (entry.issue.clone(), entry)).collect::<BTreeMap<_, _>>();

        let input = boards[&project.metadata.name].as_ref().map_err(Clone::clone)?;
        let board =
            mission.boards.scope(&project.metadata.name, input.boards.iter().map(|snapshot| snapshot.1.source.clone()).collect())?;
        let rebuild = mission
            .occupancy
            .get(&project.metadata.name)
            .is_none_or(|cached| !Arc::ptr_eq(&cached.board, &board) || cached.policy != *policy);
        if rebuild {
            let live = existing.iter().filter(|convoy| !convoy.status.as_ref().is_some_and(|s| s.phase.is_terminal())).collect::<Vec<_>>();
            let project_active = live.iter().map(|c| active_crew_count(c.status.as_ref())).sum();
            let mut missions = BTreeMap::<String, usize>::new();
            for convoy in live {
                let mut served_missions = HashSet::new();
                for served in &convoy.spec.issues {
                    let issue = Issue::builder()
                        .reference(served.reference.clone())
                        .title(served.snapshot.title.clone())
                        .labels(served.snapshot.labels.clone())
                        .state(served.snapshot.state)
                        .as_of(served.snapshot.as_of)
                        .provider_name(String::new())
                        .provider_display_name(String::new())
                        .build();
                    served_missions.insert(board.score(&issue, policy)?.mission);
                }
                for name in served_missions {
                    *missions.entry(name).or_default() += active_crew_count(convoy.status.as_ref());
                }
            }
            mission.occupancy_rebuilds += 1;
            mission.occupancy.insert(
                project.metadata.name.clone(),
                Occupancy { board: board.clone(), policy: policy.clone(), project_active, missions },
            );
        }
        let occupancy = &mission.occupancy[&project.metadata.name];
        let mut ready = input.readiness.clone()?;
        ready.retain(|issue| issue.state == IssueState::Open && issue.labels.iter().any(|label| label == READY_ISSUE_LABEL));
        ready.sort_by(|left, right| left.reference.cmp(&right.reference).then_with(|| right.as_of.cmp(&left.as_of)));
        ready.dedup_by(|left, right| left.reference == right.reference);

        let mut queue = Vec::new();
        let mut blocked = 0;
        for issue in ready {
            if dispatched.contains(&issue.reference) {
                continue;
            }
            if held.contains(&issue.reference) {
                blocked += 1;
                continue;
            }
            let facts = self.cached_facts(&issue.reference, reads).await?;
            let is_ideation = |kind: &str| {
                matches!(kind.rsplit(':').next().unwrap_or(kind).to_ascii_lowercase().as_str(), "grill" | "grilling" | "map" | "brainstorm")
            };
            let ideation = facts.issue_type.iter().chain(issue.labels.iter()).any(|kind| is_ideation(kind));
            if issue.labels.iter().any(|label| label.eq_ignore_ascii_case("map"))
                && !facts.issue_type.iter().any(|kind| is_ideation(kind))
                && !issue.labels.iter().any(|label| !label.eq_ignore_ascii_case("map") && is_ideation(label))
            {
                warn!(project = %project.metadata.name, issue = %issue.reference.id, "dispatch excluded by reserved plain map label");
            }
            if ideation || facts.has_open_pull_request {
                blocked += 1;
                continue;
            }
            let blockers = facts.blockers;
            let previous = previous_by_issue.get(&issue.reference).copied();
            let mut is_blocked = false;
            let mut blockers_unknown = false;
            for blocker in blockers {
                if !reads.issues.contains_key(&blocker) {
                    reads.issues.insert(blocker.clone(), self.issues.fetch_issue(&blocker).await);
                }
                match reads.issues[&blocker].clone() {
                    Ok(blocker) if blocker.state == IssueState::Closed => {}
                    Ok(_) => is_blocked = true,
                    Err(error) => {
                        warn!(project = %project.metadata.name, issue = %issue.reference.id, blocker = %blocker.id, %error, "blocker could not be observed; treating issue as unavailable");
                        blockers_unknown = true;
                    }
                }
            }
            if is_blocked {
                blocked += 1;
                continue;
            }
            if blockers_unknown {
                blocked += 1;
                continue;
            }

            let ready_observed_at = previous.map_or(now, |entry| entry.ready_observed_at);
            let provenance = previous.map_or_else(
                || format!("{DISPATCH_RECONCILER_PROVENANCE}, issue #{} ready+unblocked at {}", issue.reference.id, now.to_rfc3339()),
                |entry| entry.provenance.clone(),
            );
            let observed_at = previous
                .filter(|entry| entry.issue_as_of == issue.as_of && entry.title == issue.title)
                .map_or(now, |entry| entry.observed_at);
            let mut score = board.score(&issue, policy)?;
            score.project_active_crews = occupancy.project_active;
            score.mission_active_crews = occupancy.missions.get(&score.mission).copied().unwrap_or(0);
            queue.push(DispatchQueueEntry {
                score: Some(score),
                issue: issue.reference,
                title: issue.title,
                issue_as_of: issue.as_of,
                ready_observed_at,
                observed_at,
                provenance,
            });
        }
        queue.sort_by(|left, right| left.ready_observed_at.cmp(&right.ready_observed_at).then_with(|| left.issue.cmp(&right.issue)));
        let previous_attention = project.status.as_ref().and_then(|status| status.dispatch_queue_attention.as_ref());
        let attention = dispatch_queue_attention(&queue, policy, previous_attention, now);
        self.replace_queue(project, queue.clone(), attention).await?;
        if project.status.as_ref().is_some_and(|status| status.dispatch_queue_error.is_some()) {
            apply_status_patch(
                &self.backend.clone().using::<Project>(&self.namespace),
                &project.metadata.name,
                &ProjectStatusPatch::DispatchQueueError { message: None },
            )
            .await
            .map_err(|error| error.to_string())?;
        }

        Ok(ReconcilePass { queued: queue.len(), blocked, observations_recorded, ..ReconcilePass::default() })
    }

    async fn cached_facts(&self, reference: &IssueRef, reads: &mut PassIssueReads) -> Result<DispatchIssueFacts, String> {
        if !reads.facts.contains_key(reference) {
            reads.facts.insert(reference.clone(), self.issues.dispatch_facts(reference).await);
        }
        reads.facts[reference].clone()
    }

    async fn active_holds(
        &self,
        project: &ResourceObject<Project>,
        now: DateTime<Utc>,
        inventories: &PassInventories,
        reads: &mut PassIssueReads,
    ) -> Result<HashSet<IssueRef>, String> {
        let mut active = HashSet::new();
        for hold in &inventories.holds {
            if hold.spec.project_ref != project.metadata.name || hold.status.as_ref().is_some_and(|status| status.cleared_at.is_some()) {
                continue;
            }
            let cleared = match &hold.spec.clear_when {
                HoldClearWhen::Landed => {
                    let convoy_landed = inventories.landed_issues.contains(&hold.spec.land_after);
                    convoy_landed || self.cached_facts(&hold.spec.land_after, reads).await.map(|facts| facts.landed).unwrap_or(false)
                }
                HoldClearWhen::Deployed { installation } => inventories
                    .deployments
                    .iter()
                    .any(|deployment| deployment.spec.issue == hold.spec.land_after && deployment.spec.installation == *installation),
            };
            if cleared {
                // Clear only on the authoring store; replicas can project the satisfied
                // relationship immediately and receive its durable latch on replication.
                let local = self.backend.clone().using::<DispatchHold>(&project.metadata.namespace);
                match local.get(&hold.metadata.name).await {
                    Ok(_) => {
                        apply_status_patch(&local, &hold.metadata.name, &DispatchHoldStatusPatch::Clear { at: now })
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
            } else {
                active.insert(hold.spec.issue.clone());
            }
        }
        Ok(active)
    }

    async fn refresh_convoys(&self, state: &mut MissionState) -> Result<(), String> {
        loop {
            if state.watch.is_none() {
                let resolver = self.backend.including_replicas::<Convoy>(&self.namespace);
                // Subscribe before listing: racing writes appear in either or both.
                let watch = resolver.watch().await.map_err(|e| e.to_string())?;
                let listed = resolver.list().await.map_err(|e| e.to_string())?;
                state.convoy_lists += 1;
                state.convoys.clear();
                for record in listed.items {
                    state.convoys.entry(record.object.metadata.name.clone()).or_insert_with(|| Arc::new(record.object));
                }
                state.occupancy.clear();
                state.watch = Some(watch);
            }
            match state.watch.as_mut().expect("subscribed").next().now_or_never() {
                None => return Ok(()),
                Some(Some(Ok(event))) => {
                    let name = match event {
                        ReadWatchEvent::Added(record) | ReadWatchEvent::Modified(record) | ReadWatchEvent::Deleted(record) => {
                            record.object.metadata.name
                        }
                        ReadWatchEvent::DeletedByName { tombstone, .. } => tombstone.name,
                    };
                    // Re-read only the changed name to preserve local-over-replica
                    // precedence and reveal a replica when its local copy is removed.
                    let object = match self.backend.including_replicas::<Convoy>(&self.namespace).get(&name).await {
                        Ok(record) => Some(record.object),
                        Err(ResourceError::NotFound { .. }) => None,
                        Err(error) => {
                            state.watch = None;
                            return Err(error.to_string());
                        }
                    };
                    if state.convoys.get(&name).map(|previous| (&previous.spec, &previous.status))
                        == object.as_ref().map(|next| (&next.spec, &next.status))
                    {
                        continue;
                    }
                    if let Some(previous) = state.convoys.remove(&name) {
                        if let Some(project) = &previous.spec.project_ref {
                            state.occupancy.remove(project);
                        }
                    }
                    if let Some(object) = object {
                        if let Some(project) = &object.spec.project_ref {
                            state.occupancy.remove(project);
                        }
                        state.convoys.insert(name, Arc::new(object));
                    }
                }
                Some(_) => {
                    state.watch = None;
                    // Recover a gap from a fresh watch/list on the next pass.
                    return Err("convoy watch interrupted; retry reconciliation".into());
                }
            }
        }
    }

    async fn observe_dispatches(
        &self,
        project: &ResourceObject<Project>,
        previous_queue: &[DispatchQueueEntry],
        convoys: &[Arc<ResourceObject<Convoy>>],
        now: DateTime<Utc>,
        inventories: &PassInventories,
    ) -> Result<usize, String> {
        let queued = previous_queue.iter().map(|entry| (&entry.issue, entry)).collect::<BTreeMap<_, _>>();
        let observations = self.backend.clone().using::<DispatchObservation>(&project.metadata.namespace);
        let mut recorded = 0;
        let mut seen = HashSet::new();
        for convoy in convoys {
            for issue in &convoy.spec.issues {
                let Some(queue_entry) = queued.get(&issue.reference) else { continue };
                let identity = serde_json::json!({
                    "project": project.metadata.name,
                    "convoy": convoy.metadata.name,
                    "issue": issue.reference,
                });
                let name = format!("dispatch-{}", content_hash(&identity).map_err(|error| error.to_string())?);
                if inventories.observations.contains(&name) || !seen.insert(name.clone()) {
                    continue;
                }
                if !inventories.workflows.contains(pinned_workflow_ref(convoy)) {
                    warn!(
                        project = %project.metadata.name,
                        convoy = %convoy.metadata.name,
                        issue = ?issue.reference,
                        workflow = %pinned_workflow_ref(convoy),
                        reason = "absent from pass inventory",
                        "cannot record dispatch observation without its workflow"
                    );
                    continue;
                }
                let dispatched_at = convoy.metadata.creation_timestamp;
                let time_from_ready_seconds =
                    dispatched_at.signed_duration_since(queue_entry.ready_observed_at).num_seconds().max(0) as u64;
                let spec = DispatchObservationSpec::builder()
                    .project_ref(project.metadata.name.clone())
                    .convoy_ref(convoy.metadata.name.clone())
                    .issue(issue.reference.clone())
                    .workflow_ref(convoy.spec.workflow_ref.clone())
                    .maybe_placement_policy(convoy.spec.placement_policy.clone())
                    .ready_observed_at(queue_entry.ready_observed_at)
                    .dispatched_at(dispatched_at)
                    .time_from_ready_seconds(time_from_ready_seconds)
                    .observed_at(now)
                    .provenance(DISPATCH_RECONCILER_PROVENANCE.to_string())
                    .build();
                observations
                    .create(&InputMeta::builder().name(name).build(), &spec)
                    .await
                    .map_err(|error| format!("record dispatch observation for convoy {}: {error}", convoy.metadata.name))?;
                info!(project = %project.metadata.name, convoy = %convoy.metadata.name, issue = %issue.reference.id, "recorded dispatch observation");
                recorded += 1;
            }
        }
        Ok(recorded)
    }

    async fn replace_queue(
        &self,
        project: &ResourceObject<Project>,
        queue: Vec<DispatchQueueEntry>,
        attention: Option<DispatchQueueAttention>,
    ) -> Result<(), String> {
        let current = project.status.clone().unwrap_or_default();
        if current.dispatch_queue == queue && current.dispatch_queue_attention == attention {
            return Ok(());
        }
        apply_status_patch(
            &self.backend.clone().using::<Project>(&project.metadata.namespace),
            &project.metadata.name,
            &ProjectStatusPatch::ReplaceDispatchQueue { queue, attention },
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }
}

/// A settled role releases mission share even while its convoy awaits landing.
/// Before publication of any role state, reserve one provisional crew.
fn active_crew_count(status: Option<&flotilla_resources::ConvoyStatus>) -> usize {
    if status.is_some_and(|status| status.phase.is_terminal()) {
        return 0;
    }
    let Some(status) = status.filter(|status| status.crew_work.values().any(|crew| !crew.is_empty())) else {
        return 1;
    };
    status
        .crew_work
        .values()
        .flat_map(|crew| crew.values())
        .filter(|state| {
            matches!(
                state.phase,
                flotilla_resources::CrewWorkPhase::Pending
                    | flotilla_resources::CrewWorkPhase::Working
                    | flotilla_resources::CrewWorkPhase::Interrupted
                    | flotilla_resources::CrewWorkPhase::Stalled
            )
        })
        .count()
}

fn dispatch_queue_attention(
    queue: &[DispatchQueueEntry],
    policy: &DispatchPolicy,
    previous: Option<&DispatchQueueAttention>,
    now: DateTime<Utc>,
) -> Option<DispatchQueueAttention> {
    let oldest = queue.iter().map(|entry| entry.ready_observed_at).min()?;
    let threshold = i64::try_from(policy.stale_after_seconds).unwrap_or(i64::MAX);
    let stale_since = oldest.checked_add_signed(Duration::seconds(threshold)).unwrap_or(DateTime::<Utc>::MAX_UTC);
    (now >= stale_since).then(|| {
        let observed_at = previous
            .filter(|attention| {
                attention.count == queue.len() && attention.oldest_ready_observed_at == oldest && attention.stale_since == stale_since
            })
            .map_or(now, |attention| attention.observed_at);
        DispatchQueueAttention { count: queue.len(), oldest_ready_observed_at: oldest, stale_since, observed_at }
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use flotilla_protocol::IssueSource;
    use flotilla_resources::single_agent_workflow_spec;
    use flotilla_resources::ConvoyIssue;
    use flotilla_resources::ConvoySpec;
    use flotilla_resources::InputValue;
    use flotilla_resources::IssueSnapshot;
    use flotilla_resources::ProjectSpec;
    use flotilla_resources::RepositoryKey;
    use flotilla_store_testkit::VirtualClock;

    use super::*;

    const NAMESPACE: &str = "flotilla";

    // Stands in for the external tracker; storage/reconciliation remain real.
    struct FakeIssues {
        board_override: Mutex<Option<Vec<flotilla_protocol::DispatchBoardRepository>>>,
        ready: Mutex<Vec<Issue>>,
        by_ref: Mutex<HashMap<IssueRef, Issue>>,
        ready_calls: Mutex<usize>,
        facts_calls: Mutex<usize>,
        facts: Mutex<HashMap<IssueRef, DispatchIssueFacts>>,
        failing_projects: Mutex<HashSet<String>>,
        failing_collection: Mutex<bool>,
        failing_refs: Mutex<HashSet<IssueRef>>,
    }

    impl FakeIssues {
        async fn boards(&self, _project: &ResourceObject<Project>) -> Result<Vec<flotilla_protocol::DispatchBoardRepository>, String> {
            if let Some(boards) = &*self.board_override.lock().expect("board") {
                return Ok(boards.clone());
            }
            let facts = self.facts.lock().expect("facts");
            let issues = self
                .ready
                .lock()
                .expect("ready")
                .iter()
                .chain(self.by_ref.lock().expect("by_ref").values())
                .map(|issue| {
                    flotilla_protocol::DispatchBoardIssue::builder()
                        .id(issue.reference.id.clone())
                        .title(issue.title.clone())
                        .state(issue.state)
                        .url(format!("https://github.com/{}/issues/{}", issue.reference.source.scope, issue.reference.id))
                        .updated_at(issue.as_of.to_rfc3339())
                        .labels(issue.labels.clone())
                        .blocked_by(
                            facts
                                .get(&issue.reference)
                                .into_iter()
                                .flat_map(|f| &f.blockers)
                                .map(|b| flotilla_protocol::DispatchBoardDependency {
                                    url: format!("https://github.com/{}/issues/{}", b.source.scope, b.id),
                                    state: IssueState::Open,
                                })
                                .collect(),
                        )
                        .pull_requests(vec![])
                        .build()
                })
                .collect();
            Ok(vec![flotilla_protocol::DispatchBoardRepository {
                source: source(),
                issues,
                pull_requests: vec![],
                observed_at: Utc::now(),
                age_seconds: 0,
                refresh_error: None,
            }])
        }
        async fn ready_issues(&self, project: &ResourceObject<Project>) -> Result<Vec<Issue>, String> {
            if self.failing_projects.lock().expect("failing projects lock").contains(&project.metadata.name) {
                return Err("issue source unavailable".to_string());
            }
            *self.ready_calls.lock().expect("ready calls lock") += 1;
            Ok(self.ready.lock().expect("ready lock").clone())
        }
    }

    #[async_trait]
    impl DispatchIssueSource for FakeIssues {
        async fn collect_boards(&self, projects: &[ResourceObject<Project>]) -> Result<ProjectBoards, String> {
            if *self.failing_collection.lock().expect("collection failure") {
                return Err("injected source inventory outage".into());
            }
            let mut result = BTreeMap::new();
            for project in projects {
                result.insert(
                    project.metadata.name.clone(),
                    self.boards(project).await.map(|boards| ProjectBoardInput {
                        readiness: Ok(vec![]),
                        boards: boards
                            .into_iter()
                            .map(|board| {
                                let revision =
                                    flotilla_resources::content_hash(&serde_json::to_value(&board.issues).expect("issue values"))
                                        .expect("issue hash");
                                // The injected tracker has no cache cursor; hash its delivered facts.
                                let revision = u64::from_str_radix(&revision, 16).expect("hash prefix");
                                (revision, Arc::new(board))
                            })
                            .collect(),
                    }),
                );
            }
            for project in projects {
                if project.spec.dispatch_policy.as_ref().is_some_and(|policy| policy.enabled) {
                    if let Some(Ok(input)) = result.get_mut(&project.metadata.name) {
                        input.readiness = self.ready_issues(project).await;
                    }
                }
            }
            Ok(result)
        }
        async fn fetch_issue(&self, reference: &IssueRef) -> Result<Issue, String> {
            if self.failing_refs.lock().expect("failing refs lock").contains(reference) {
                return Err("issue fetch unavailable".to_string());
            }
            self.by_ref.lock().expect("issues lock").get(reference).cloned().ok_or_else(|| "missing issue".to_string())
        }
        async fn dispatch_facts(&self, reference: &IssueRef) -> Result<DispatchIssueFacts, String> {
            *self.facts_calls.lock().expect("fact counts") += 1;
            Ok(self.facts.lock().expect("facts lock").get(reference).cloned().unwrap_or_default())
        }
    }

    fn source() -> IssueSource {
        IssueSource { service: "https://github.com".to_string(), scope: "acme/widgets".to_string() }
    }

    fn issue(id: &str, labels: &[&str], body: Option<&str>, state: IssueState) -> Issue {
        Issue {
            reference: IssueRef { source: source(), id: id.to_string() },
            title: format!("Issue {id}"),
            body: body.map(str::to_string),
            state,
            labels: labels.iter().map(|label| (*label).to_string()).collect(),
            assignees: vec![],
            as_of: "2026-08-04T12:00:00Z".parse().expect("timestamp"),
            observed_at: Some("2026-08-04T12:01:00Z".parse().expect("timestamp")),
            provider_name: "fake".to_string(),
            provider_display_name: "Fake".to_string(),
        }
    }

    #[test]
    fn ready_issue_queries_preserve_each_bindings_tracker_fields() {
        let binding = ResolvedIssueSourceBinding {
            source: source(),
            alias: "widgets".to_string(),
            filter: flotilla_resources::IssueFilter {
                match_fields: BTreeMap::from([("component".to_string(), flotilla_resources::IssueFieldValue::One("terminal".to_string()))]),
            },
            create_with: BTreeMap::new(),
            creatable: true,
        };

        assert_eq!(
            ready_issue_query(&binding),
            IssueQuery {
                search: None,
                label: Some(READY_ISSUE_LABEL.to_string()),
                match_fields: BTreeMap::from([("component".to_string(), vec!["terminal".to_string()])]),
            }
        );
    }

    async fn harness(
        ready: Vec<Issue>,
        blockers: Vec<Issue>,
        policy: DispatchPolicy,
    ) -> (ResourceBackend, Arc<FakeIssues>, Arc<VirtualClock>, DispatchReconciler) {
        let backend = ResourceBackend::InMemory(Default::default());
        backend
            .clone()
            .using::<Project>(NAMESPACE)
            .create(
                &InputMeta::builder().name("widgets".to_string()).build(),
                &ProjectSpec {
                    charter: None,
                    role_definitions: BTreeMap::new(),
                    charter_prose: BTreeMap::new(),
                    parent: None,
                    platform_matrix: Vec::new(),
                    role_needs: Default::default(),
                    skills: BTreeMap::new(),
                    display_name: "Widgets".to_string(),
                    default_workflow_ref: "implement".to_string(),
                    supervision: None,
                    issue_source_bindings: vec![flotilla_resources::IssueSourceBindingSpec::builder()
                        .source(source())
                        .alias("widgets".to_string())
                        .build()],
                    repositories: vec![flotilla_resources::ProjectRepositorySpec {
                        charter_store: None,
                        repo: RepositoryKey("acme/widgets".to_string()),
                        alias: None,
                        roles: Default::default(),
                        subpath: None,
                        default_branch: None,
                    }],
                    dispatch_policy: Some(policy),
                },
            )
            .await
            .expect("project");
        let issues = Arc::new(FakeIssues {
            board_override: Mutex::new(None),
            ready: Mutex::new(ready),
            by_ref: Mutex::new(blockers.into_iter().map(|issue| (issue.reference.clone(), issue)).collect()),
            ready_calls: Mutex::new(0),
            facts_calls: Mutex::new(0),
            facts: Mutex::new(HashMap::new()),
            failing_projects: Mutex::new(Default::default()),
            failing_collection: Mutex::new(false),
            failing_refs: Mutex::new(Default::default()),
        });
        let clock = Arc::new(VirtualClock::new("2026-08-04T12:00:00Z".parse().expect("clock timestamp")));
        let reconciler = DispatchReconciler::new(backend.clone(), NAMESPACE, Arc::clone(&issues) as Arc<dyn DispatchIssueSource>)
            .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        (backend, issues, clock, reconciler)
    }

    struct CountedInventories {
        real: BackendDispatchResources,
        calls: Mutex<[usize; 4]>,
        fail_holds: Mutex<bool>,
    }

    #[async_trait]
    impl DispatchResourceSource for CountedInventories {
        async fn holds(&self) -> Result<Vec<ResourceObject<DispatchHold>>, String> {
            self.calls.lock().expect("counts")[0] += 1;
            if *self.fail_holds.lock().expect("failure") {
                return Err("injected hold inventory outage".into());
            }
            self.real.holds().await
        }
        async fn deployments(&self) -> Result<Vec<ResourceObject<DispatchDeployment>>, String> {
            self.calls.lock().expect("counts")[1] += 1;
            self.real.deployments().await
        }
        async fn workflows(&self) -> Result<HashSet<String>, String> {
            self.calls.lock().expect("counts")[2] += 1;
            self.real.workflows().await
        }
        async fn observations(&self) -> Result<HashSet<String>, String> {
            self.calls.lock().expect("counts")[3] += 1;
            self.real.observations().await
        }
    }

    // #2860/#2859: overlapping Projects share inventories; unchanged passes do
    // no graph/occupancy work. Convoy add/update/remove and watch recovery affect
    // occupancy independently of graph topology and preserve readiness aging.
    #[tokio::test]
    async fn pass_inventories_and_convoy_deltas_have_bounded_work() {
        use flotilla_resources::ConvoyStatus;
        let (backend, issues, clock, mut reconciler) =
            harness(vec![issue("1", &["ready"], None, IssueState::Open)], vec![], policy(60)).await;
        let spec = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").spec;
        for id in 0..12 {
            backend
                .using::<Project>(NAMESPACE)
                .create(&InputMeta::builder().name(format!("overlap-{id}")).build(), &spec)
                .await
                .expect("overlap");
        }
        let counted = Arc::new(CountedInventories {
            real: BackendDispatchResources { backend: backend.clone(), namespace: NAMESPACE.into() },
            calls: Mutex::new([0; 4]),
            fail_holds: Mutex::new(false),
        });
        reconciler.resources = counted.clone();
        assert_eq!(reconciler.reconcile_once().await.expect("initial pass").queued, 13);
        let initial = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status").dispatch_queue[0]
            .ready_observed_at;
        clock.advance(Duration::seconds(120));
        assert_eq!(reconciler.reconcile_once().await.expect("unchanged pass").queued, 13);
        assert_eq!(*counted.calls.lock().expect("counts"), [2; 4]);
        assert_eq!(*issues.facts_calls.lock().expect("fact counts"), 2, "one unique issue read per pass across 13 Projects");
        {
            let state = reconciler.mission.lock().await;
            assert_eq!(state.boards.rebuilds(), 13);
            assert_eq!(state.boards.updated_issues(), 0);
            assert_eq!(state.occupancy_rebuilds, 13);
            assert_eq!(state.convoy_lists, 1);
        }
        let convoys = backend.using::<Convoy>(NAMESPACE);
        convoys
            .create(
                &InputMeta::builder().name("watched".into()).build(),
                &ConvoySpec::builder().workflow_ref("implement".into()).project_ref("widgets".into()).build(),
            )
            .await
            .expect("convoy add");
        reconciler.reconcile_once().await.expect("add pass");
        assert_eq!(reconciler.mission.lock().await.occupancy_rebuilds, 14);
        let widgets = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("widgets").status.expect("status");
        assert_eq!(widgets.dispatch_queue[0].score.as_ref().expect("score").project_active_crews, 1);
        let mut moved = convoys.get("watched").await.expect("convoy");
        moved.spec.project_ref = Some("overlap-0".into());
        convoys.update(&InputMeta::from(&moved.metadata), &moved.metadata.resource_version, &moved.spec).await.expect("move convoy");
        reconciler.reconcile_once().await.expect("move pass");
        assert_eq!(reconciler.mission.lock().await.occupancy_rebuilds, 16, "old and new scopes each update once");
        let widgets = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("widgets").status.expect("status");
        let moved_project = backend.using::<Project>(NAMESPACE).get("overlap-0").await.expect("destination").status.expect("status");
        assert_eq!(widgets.dispatch_queue[0].score.as_ref().expect("score").project_active_crews, 0);
        assert_eq!(moved_project.dispatch_queue[0].score.as_ref().expect("score").project_active_crews, 1);
        let current = convoys.get("watched").await.expect("convoy");
        convoys
            .update_status(
                "watched",
                &current.metadata.resource_version,
                &ConvoyStatus { phase: ConvoyPhase::Landed, ..Default::default() },
            )
            .await
            .expect("convoy update");
        reconciler.reconcile_once().await.expect("update pass");
        assert_eq!(reconciler.mission.lock().await.occupancy_rebuilds, 17);
        convoys.delete("watched").await.expect("convoy remove");
        reconciler.reconcile_once().await.expect("remove pass");
        {
            let state = reconciler.mission.lock().await;
            assert_eq!(state.occupancy_rebuilds, 18);
            assert_eq!(state.boards.rebuilds(), 13);
            assert_eq!(state.convoy_lists, 1);
        }
        // Inject a transport gap at the actual watch seam, then recover from
        // real in-memory resource inventory; no live fleet is involved.
        reconciler.mission.lock().await.watch = Some(futures::stream::empty().boxed());
        assert_eq!(reconciler.reconcile_once().await.expect("scoped watch error").project_errors, 13);
        reconciler.reconcile_once().await.expect("watch recovery");
        assert_eq!(reconciler.mission.lock().await.convoy_lists, 2);
        *counted.fail_holds.lock().expect("failure") = true;
        assert_eq!(reconciler.reconcile_once().await.expect("scoped inventory error").project_errors, 13);
        let unavailable = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        assert!(unavailable.dispatch_queue_error.is_some());
        assert_eq!(unavailable.dispatch_queue[0].ready_observed_at, initial);
        *counted.fail_holds.lock().expect("failure") = false;
        *issues.failing_collection.lock().expect("collection failure") = true;
        assert_eq!(reconciler.reconcile_once().await.expect("source inventory outage").project_errors, 13);
        let unavailable = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        assert!(unavailable.dispatch_queue_error.as_ref().expect("error").contains("source inventory unavailable"));
        assert_eq!(unavailable.dispatch_queue[0].ready_observed_at, initial);
        *issues.failing_collection.lock().expect("collection failure") = false;
        issues.failing_projects.lock().expect("failures").insert("overlap-0".into());
        let failed = reconciler.reconcile_once().await.expect("isolated error");
        assert_eq!(failed.project_errors, 1);
        assert_eq!(failed.queued, 12);
        issues.failing_projects.lock().expect("failures").clear();
        reconciler.reconcile_once().await.expect("source recovery");
        let status = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        assert_eq!(status.dispatch_queue[0].ready_observed_at, initial);
        assert!(status.dispatch_queue_attention.is_some());
        let recovered = backend.using::<Project>(NAMESPACE).get("overlap-0").await.expect("recovered project").status.expect("status");
        assert_eq!(recovered.dispatch_queue[0].ready_observed_at, initial);
        assert!(recovered.dispatch_queue_error.is_none());
    }

    // #2868: local charter copies must not turn every daemon into a queue
    // reconciler. The original Project home alone reads dispatch sources.
    #[tokio::test]
    async fn project_home_is_the_only_dispatch_reconciler() {
        let (home, issues, _, reconciler) = harness(vec![issue("1", &["ready"], None, IssueState::Open)], Vec::new(), policy(3600)).await;
        let project = home.using::<Project>(NAMESPACE).get("widgets").await.unwrap();
        for root in ["second-reconciler", "third-reconciler"] {
            let other = ResourceBackend::InMemory(Default::default()).with_local_root(flotilla_protocol::NodeId::new(root));
            other.using::<Project>(NAMESPACE).create(&InputMeta::builder().name("widgets".into()).build(), &project.spec).await.unwrap();
            other
                .replica_writer::<Project>(home.local_root().unwrap(), NAMESPACE)
                .replace(&home.using::<Project>(NAMESPACE).list().await.unwrap(), Utc::now())
                .await
                .unwrap();
            let pass = DispatchReconciler::new(other, NAMESPACE, issues.clone()).reconcile_once().await.unwrap();
            assert_eq!(pass, ReconcilePass::default());
            assert_eq!(*issues.ready_calls.lock().unwrap(), 0);
        }
        reconciler.reconcile_once().await.unwrap();
        assert_eq!(*issues.ready_calls.lock().unwrap(), 1);
    }

    fn native_edge(issues: &FakeIssues, issue_id: &str, blocker_id: &str) {
        issues
            .facts
            .lock()
            .expect("native facts")
            .entry(IssueRef { source: source(), id: issue_id.into() })
            .or_default()
            .blockers
            .push(IssueRef { source: source(), id: blocker_id.into() });
    }

    fn policy(stale_after_seconds: u64) -> DispatchPolicy {
        DispatchPolicy::builder().stale_after_seconds(stale_after_seconds).build()
    }

    // #2783 admission inputs count unsettled roles, not completed obligations.
    // Exhaustive finite enum mapping: every phase, unknown publication, terminal
    // convoy, and mixed roles. No process boundary or random interleaving exists.
    #[test]
    fn settled_roles_release_share_before_the_convoy_lands() {
        use flotilla_resources::{ConvoyStatus, CrewWorkPhase, CrewWorkState};
        assert_eq!(active_crew_count(None), 1);
        assert_eq!(active_crew_count(Some(&ConvoyStatus::default())), 1);
        for (phase, unsettled) in [
            (CrewWorkPhase::Pending, true),
            (CrewWorkPhase::Working, true),
            (CrewWorkPhase::Interrupted, true),
            (CrewWorkPhase::Stalled, true),
            (CrewWorkPhase::Done, false),
            (CrewWorkPhase::HandedBack, false),
            (CrewWorkPhase::Failed, false),
        ] {
            let mut status = ConvoyStatus::default();
            status.crew_work.insert(
                "work".into(),
                BTreeMap::from([
                    ("coder".into(), CrewWorkState::builder().phase(phase).build()),
                    ("reviewer".into(), CrewWorkState::builder().phase(CrewWorkPhase::Working).build()),
                ]),
            );
            assert_eq!(active_crew_count(Some(&status)), 1 + usize::from(unsettled));
            status.crew_work.get_mut("work").expect("work").remove("reviewer");
            assert_eq!(active_crew_count(Some(&status)), usize::from(unsettled));
            status.phase = ConvoyPhase::Landed;
            assert_eq!(active_crew_count(Some(&status)), 0);
        }
    }

    // #2783: a mission score survives real in-memory status writes; changing
    // a field re-ranks a proposal without resetting readiness age or attention.
    #[tokio::test]
    async fn mission_scores_reconcile_and_retain_readiness_age() {
        use flotilla_resources::DispatchMission;
        let mut policy = policy(60);
        policy.project_share = 3;
        policy.missions =
            vec![DispatchMission::builder().name("routine".into()).issue(IssueRef { source: source(), id: "99".into() }).build()];
        let (backend, issues, clock, reconciler) =
            harness(vec![issue("1", &[READY_ISSUE_LABEL], None, IssueState::Open)], vec![], policy).await;
        let tracking = flotilla_protocol::DispatchBoardIssue::builder()
            .id("99".into())
            .title("Routine".into())
            .state(IssueState::Open)
            .url("https://github.com/acme/widgets/issues/99".into())
            .updated_at(String::new())
            .labels(vec!["value:2".into(), "crew-limit:0".into()])
            .blocked_by(vec![])
            .pull_requests(vec![])
            .build();
        let mut boards = issues.boards(&backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project")).await.expect("board");
        boards[0].issues.push(tracking);
        *issues.board_override.lock().expect("board") = Some(boards.clone());
        reconciler.reconcile_once().await.expect("first pass");
        let first = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        let score = first.dispatch_queue[0].score.as_ref().expect("score");
        assert_eq!(f64::from(score.attributes.value), 2.0);
        assert_eq!(score.attributes.crew_limit, Some(0));
        assert_eq!(score.project_share, 3);
        assert_eq!(score.project_active_crews, 0);
        boards[0].issues.last_mut().expect("tracking").mission_fields.value = Some(7.5.try_into().expect("value"));
        *issues.board_override.lock().expect("board") = Some(boards);
        clock.advance(Duration::seconds(60));
        reconciler.reconcile_once().await.expect("re-rank pass");
        let second = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        assert_eq!(second.dispatch_queue[0].ready_observed_at, first.dispatch_queue[0].ready_observed_at);
        let score = second.dispatch_queue[0].score.as_ref().expect("score");
        assert_eq!(f64::from(score.attributes.value), 7.5);
        assert_eq!(score.attribute_sources["value"], "issue_field");
        assert!(second.dispatch_queue_attention.is_some());
    }

    #[tokio::test]
    async fn ready_unblocked_issues_are_proposed_without_creating_any_convoys() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let unlabeled = issue("1", &[], None, IssueState::Open);
        let blocked = issue("3", &[READY_ISSUE_LABEL], Some("## Blocked by\n\n#9"), IssueState::Open);
        let blocker = issue("9", &[], None, IssueState::Open);
        let (backend, issues, _, reconciler) = harness(vec![unlabeled, blocked, ready.clone()], vec![blocker], policy(300)).await;
        native_edge(&issues, "3", "9");

        let outcome = reconciler.reconcile_once().await.expect("reconcile");

        assert_eq!(outcome.queued, 1);
        assert_eq!(outcome.blocked, 1);
        let project = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project");
        assert_eq!(project.status.expect("status").dispatch_queue[0].issue, ready.reference);
        assert!(backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys").items.is_empty(), "proposer must never admit convoys");
    }

    #[tokio::test]
    async fn closing_a_blocker_adds_the_dependent_to_the_queue_on_the_next_pass() {
        let dependent = issue("2", &[READY_ISSUE_LABEL], Some("## Blocked by\n#9"), IssueState::Open);
        let blocker = issue("9", &[], None, IssueState::Open);
        let (backend, issues, _, reconciler) = harness(vec![dependent], vec![blocker], policy(300)).await;
        native_edge(&issues, "2", "9");
        assert_eq!(reconciler.reconcile_once().await.expect("blocked pass").queued, 0);

        issues
            .by_ref
            .lock()
            .expect("issues lock")
            .insert(IssueRef { source: source(), id: "9".to_string() }, issue("9", &[], None, IssueState::Closed));

        assert_eq!(reconciler.reconcile_once().await.expect("unblocked pass").queued, 1);
        assert_eq!(
            backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status").dispatch_queue.len(),
            1
        );
    }

    #[tokio::test]
    async fn unknown_native_dependencies_are_never_proof_of_readiness() {
        let dependent = issue("2", &[READY_ISSUE_LABEL], Some("## Blocked by\n#9"), IssueState::Open);
        let newcomer = issue("3", &[READY_ISSUE_LABEL], Some("## Blocked by\n#9"), IssueState::Open);
        let blocker = issue("9", &[], None, IssueState::Closed);
        let blocker_ref = blocker.reference.clone();
        let (backend, issues, clock, reconciler) = harness(vec![dependent.clone()], vec![blocker], policy(60)).await;
        native_edge(&issues, "2", "9");
        reconciler.reconcile_once().await.expect("verified pass");
        issues
            .facts
            .lock()
            .expect("facts lock")
            .insert(newcomer.reference.clone(), DispatchIssueFacts { blockers: vec![blocker_ref.clone()], ..Default::default() });
        issues.ready.lock().expect("ready lock").push(newcomer);
        issues.failing_refs.lock().expect("failing refs lock").insert(blocker_ref);
        clock.advance(Duration::seconds(60));

        let outcome = reconciler.reconcile_once().await.expect("transient failure pass");

        // Native dependency availability is required even for previously ready issues.
        assert_eq!(outcome.queued, 0);
        assert_eq!(outcome.blocked, 2);
        let status = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        assert!(status.dispatch_queue.is_empty());
        assert!(status.dispatch_queue_attention.is_none());
    }

    #[tokio::test]
    async fn manual_dispatch_of_a_queued_issue_records_the_a_decision_and_drops_it_from_the_queue() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let (backend, _, clock, reconciler) = harness(vec![ready.clone()], vec![], policy(300)).await;
        reconciler.reconcile_once().await.expect("queue pass");
        let workflow_root = flotilla_protocol::NodeId::new("workflow-authority");
        let workflow_authority = ResourceBackend::InMemory(Default::default()).with_local_root(workflow_root.clone());
        workflow_authority
            .definitions::<WorkflowTemplate>(NAMESPACE)
            .apply(&InputMeta::builder().name("review-and-fix".to_string()).build(), &single_agent_workflow_spec())
            .await
            .expect("workflow");
        backend
            .replica_writer::<WorkflowTemplate>(workflow_root, NAMESPACE)
            .replace(&workflow_authority.using::<WorkflowTemplate>(NAMESPACE).list().await.expect("workflow authority log"), Utc::now())
            .await
            .expect("replicate workflow");
        assert!(backend.using::<WorkflowTemplate>(NAMESPACE).list().await.expect("local workflows").items.is_empty());
        backend
            .clone()
            .using::<Convoy>(NAMESPACE)
            .create(
                &InputMeta::builder().name("human-dispatch".to_string()).build(),
                &ConvoySpec {
                    continuation: None,
                    subjects: Vec::new(),
                    role: String::new(),
                    generation: 1,
                    workflow_ref: "review-and-fix".to_string(),
                    dispatching_principal_ref: Default::default(),
                    inputs: BTreeMap::<String, InputValue>::new(),
                    placement_policy: Some("docker-local".to_string()),
                    repositories: Vec::new(),
                    r#ref: Some("fix-2".to_string()),
                    project_ref: Some("widgets".to_string()),
                    adopted_checkout_refs: Default::default(),
                    issues: vec![ConvoyIssue {
                        reference: ready.reference.clone(),
                        repository_ref: None,
                        snapshot: IssueSnapshot {
                            title: ready.title,
                            body: ready.body,
                            state: ready.state,
                            labels: ready.labels,
                            as_of: ready.as_of,
                        },
                    }],
                    change_request: None,
                    instruction: None,
                },
            )
            .await
            .expect("manual convoy");
        clock.advance(Duration::seconds(90));

        let outcome = reconciler.reconcile_once().await.expect("observation pass");

        assert_eq!(outcome.observations_recorded, 1);
        assert!(backend
            .using::<Project>(NAMESPACE)
            .get("widgets")
            .await
            .expect("project")
            .status
            .expect("status")
            .dispatch_queue
            .is_empty());
        let observations = backend.using::<DispatchObservation>(NAMESPACE).list().await.expect("observations");
        assert_eq!(observations.items.len(), 1);
        let observation = &observations.items[0].spec;
        assert_eq!(observation.issue, ready.reference);
        assert_eq!(observation.workflow_ref, "review-and-fix");
        assert_eq!(observation.placement_policy.as_deref(), Some("docker-local"));
        assert_eq!(
            observation.time_from_ready_seconds,
            observation.dispatched_at.signed_duration_since(observation.ready_observed_at).num_seconds().max(0) as u64
        );
        // Only live convoys serve an issue; terminal history cannot suppress a
        // reopened/retriaged issue forever. Every lifecycle phase is covered.
        use flotilla_resources::{ConvoyPhase, ConvoyStatus};
        let convoys = backend.clone().using::<Convoy>(NAMESPACE);
        for phase in [
            ConvoyPhase::Pending,
            ConvoyPhase::Active,
            ConvoyPhase::Interrupted,
            ConvoyPhase::Landing,
            ConvoyPhase::Landed,
            ConvoyPhase::Failed,
            ConvoyPhase::Abandoned,
        ] {
            let current = convoys.get("human-dispatch").await.expect("convoy");
            convoys
                .update_status("human-dispatch", &current.metadata.resource_version, &ConvoyStatus { phase, ..Default::default() })
                .await
                .expect("phase");
            assert_eq!(reconciler.reconcile_once().await.expect("phase pass").queued, usize::from(phase.is_terminal()));
        }
    }

    #[tokio::test]
    async fn a_missing_observation_workflow_does_not_freeze_the_queue_or_staleness_attention() {
        let dispatched = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let waiting = issue("3", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let (backend, _, clock, reconciler) = harness(vec![dispatched.clone(), waiting.clone()], vec![], policy(60)).await;
        reconciler.reconcile_once().await.expect("queue pass");
        backend
            .clone()
            .using::<Convoy>(NAMESPACE)
            .create(
                &InputMeta::builder().name("missing-workflow-dispatch".to_string()).build(),
                &ConvoySpec {
                    continuation: None,
                    subjects: Vec::new(),
                    role: String::new(),
                    generation: 1,
                    workflow_ref: "deleted-workflow".to_string(),
                    dispatching_principal_ref: Default::default(),
                    inputs: BTreeMap::<String, InputValue>::new(),
                    placement_policy: None,
                    repositories: Vec::new(),
                    r#ref: Some("fix-2".to_string()),
                    project_ref: Some("widgets".to_string()),
                    adopted_checkout_refs: Default::default(),
                    issues: vec![ConvoyIssue {
                        reference: dispatched.reference,
                        repository_ref: None,
                        snapshot: IssueSnapshot {
                            title: dispatched.title,
                            body: dispatched.body,
                            state: dispatched.state,
                            labels: dispatched.labels,
                            as_of: dispatched.as_of,
                        },
                    }],
                    change_request: None,
                    instruction: None,
                },
            )
            .await
            .expect("manual convoy");
        clock.advance(Duration::seconds(60));

        let outcome = reconciler.reconcile_once().await.expect("resilient observation pass");

        assert_eq!(outcome.queued, 1);
        assert_eq!(outcome.observations_recorded, 0);
        let status = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        assert_eq!(status.dispatch_queue.iter().map(|entry| &entry.issue).collect::<Vec<_>>(), vec![&waiting.reference]);
        assert_eq!(status.dispatch_queue[0].score.as_ref().expect("score").project_active_crews, 1);
        assert_eq!(status.dispatch_queue[0].score.as_ref().expect("score").mission_active_crews, 1);
        assert_eq!(status.dispatch_queue_attention.expect("stale attention").count, 1);
    }

    #[tokio::test]
    async fn a_non_empty_queue_raises_attention_after_the_policy_threshold() {
        let (backend, _, clock, reconciler) =
            harness(vec![issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open)], vec![], policy(60)).await;
        reconciler.reconcile_once().await.expect("fresh pass");
        assert!(backend
            .using::<Project>(NAMESPACE)
            .get("widgets")
            .await
            .expect("project")
            .status
            .expect("status")
            .dispatch_queue_attention
            .is_none());

        clock.advance(Duration::seconds(60));
        reconciler.reconcile_once().await.expect("stale pass");

        let attention = backend
            .using::<Project>(NAMESPACE)
            .get("widgets")
            .await
            .expect("project")
            .status
            .expect("status")
            .dispatch_queue_attention
            .expect("attention");
        assert_eq!(attention.count, 1);
    }

    #[tokio::test]
    async fn an_unchanged_queue_does_not_rewrite_project_status() {
        let (backend, _, _, reconciler) =
            harness(vec![issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open)], vec![], policy(300)).await;
        reconciler.reconcile_once().await.expect("first pass");
        let first = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project");

        reconciler.reconcile_once().await.expect("identical pass");

        let second = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project");
        assert_eq!(second.metadata.resource_version, first.metadata.resource_version);
    }

    #[tokio::test]
    async fn malformed_mission_publishes_error_preserves_queue_and_recovers() {
        use flotilla_resources::DispatchMission;
        let mut dispatch_policy = policy(60);
        dispatch_policy.missions =
            vec![DispatchMission::builder().name("routine".into()).issue(IssueRef { source: source(), id: "2".into() }).build()];
        let (backend, issues, _, reconciler) =
            harness(vec![issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open)], vec![], dispatch_policy).await;
        let projects = backend.using::<Project>(NAMESPACE);
        reconciler.reconcile_once().await.expect("initial pass");
        let before = projects.get("widgets").await.expect("project").status.expect("status");
        let mut boards = issues.boards(&projects.get("widgets").await.expect("project")).await.expect("board");
        boards[0].issues[0].labels.push("value:abc".into());
        *issues.board_override.lock().expect("board") = Some(boards);
        assert_eq!(reconciler.reconcile_once().await.expect("invalid pass").project_errors, 1);
        let failed = projects.get("widgets").await.expect("project").status.expect("status");
        assert_eq!(failed.dispatch_queue, before.dispatch_queue);
        assert!(failed.dispatch_queue_error.as_deref().expect("visible error").contains("value"));
        *issues.board_override.lock().expect("board") = None;
        reconciler.reconcile_once().await.expect("recovery");
        let recovered = projects.get("widgets").await.expect("project").status.expect("status");
        assert_eq!(recovered.dispatch_queue, before.dispatch_queue);
        assert!(recovered.dispatch_queue_error.is_none());
    }

    #[tokio::test]
    async fn attention_uses_oldest_entry_even_when_it_is_not_first() {
        let (backend, _, _, reconciler) = harness(
            vec![issue("1", &[READY_ISSUE_LABEL], None, IssueState::Open), issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open)],
            vec![],
            policy(60),
        )
        .await;
        reconciler.reconcile_once().await.expect("queue");
        let mut queue = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status").dispatch_queue;
        queue[1].ready_observed_at -= Duration::seconds(120);
        let oldest = queue[1].ready_observed_at;
        let attention = dispatch_queue_attention(&queue, &policy(60), None, queue[0].ready_observed_at).expect("stale");
        assert_eq!(attention.oldest_ready_observed_at, oldest);
        assert_eq!(attention.count, 2);
    }

    #[tokio::test]
    async fn source_outage_preserves_aging_and_deduplicates_errors_then_recovers() {
        let (backend, issues, clock, reconciler) =
            harness(vec![issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open)], vec![], policy(60)).await;
        let projects = backend.using::<Project>(NAMESPACE);
        reconciler.reconcile_once().await.expect("initial pass");
        clock.advance(Duration::seconds(120));
        reconciler.reconcile_once().await.expect("attention pass");
        let before = projects.get("widgets").await.expect("project").status.expect("status");
        issues.failing_projects.lock().expect("failures").insert("widgets".into());
        assert_eq!(reconciler.reconcile_once().await.expect("outage").project_errors, 1);
        let failed = projects.get("widgets").await.expect("project");
        let status = failed.status.as_ref().expect("status");
        assert_eq!(status.dispatch_queue, before.dispatch_queue);
        assert_eq!(status.dispatch_queue_attention, before.dispatch_queue_attention);
        assert!(status.dispatch_queue_error.is_some());
        clock.advance(Duration::seconds(120));
        reconciler.reconcile_once().await.expect("persistent outage");
        assert_eq!(projects.get("widgets").await.expect("project").metadata.resource_version, failed.metadata.resource_version);
        issues.failing_projects.lock().expect("failures").clear();
        reconciler.reconcile_once().await.expect("recovery");
        let recovered = projects.get("widgets").await.expect("project").status.expect("status");
        assert_eq!(recovered.dispatch_queue, before.dispatch_queue);
        assert_eq!(recovered.dispatch_queue_attention, before.dispatch_queue_attention);
        assert!(recovered.dispatch_queue_error.is_none());
    }

    #[tokio::test]
    async fn one_broken_project_does_not_starve_a_healthy_projects_queue() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let (backend, issues, _, reconciler) = harness(vec![ready], vec![], policy(300)).await;
        let widgets = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("widgets project");
        backend
            .clone()
            .using::<Project>(NAMESPACE)
            .create(&InputMeta::builder().name("broken".to_string()).build(), &widgets.spec)
            .await
            .expect("broken project");
        issues.failing_projects.lock().expect("failing projects lock").insert("broken".to_string());

        let outcome = reconciler.reconcile_once().await.expect("isolated pass");

        assert_eq!(outcome.project_errors, 1);
        assert_eq!(outcome.queued, 1);
        assert_eq!(
            backend.using::<Project>(NAMESPACE).get("widgets").await.expect("widgets project").status.expect("status").dispatch_queue.len(),
            1
        );
    }

    #[tokio::test]
    async fn disabled_policy_clears_queue_immediately_without_observing_or_querying() {
        let (backend, issues, _, reconciler) =
            harness(vec![issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open)], vec![], policy(60)).await;
        reconciler.reconcile_once().await.expect("enabled pass");
        let project_resolver = backend.clone().using::<Project>(NAMESPACE);
        let current = project_resolver.get("widgets").await.expect("project");
        let mut spec = current.spec;
        spec.dispatch_policy.as_mut().expect("policy").enabled = false;
        project_resolver
            .update(&InputMeta::from(&current.metadata), &current.metadata.resource_version, &spec)
            .await
            .expect("disable policy");
        let calls_before = *issues.ready_calls.lock().expect("ready calls lock");

        assert_eq!(reconciler.reconcile_once().await.expect("disabled pass"), ReconcilePass::default());
        assert_eq!(*issues.ready_calls.lock().expect("ready calls lock"), calls_before);
        let status = project_resolver.get("widgets").await.expect("project").status.expect("status");
        assert!(status.dispatch_queue.is_empty());
        assert!(status.dispatch_queue_attention.is_none());
    }
    // Each predicate input is pinned independently so a missing gate cannot
    // hide behind another false conjunct in a generated combination.
    #[tokio::test]
    async fn every_predicate_input_has_an_independent_gate() {
        let mut candidates = (1..=14)
            .filter(|id| *id != 12)
            .map(|id| issue(&id.to_string(), &[READY_ISSUE_LABEL], None, IssueState::Open))
            .collect::<Vec<_>>();
        candidates.iter_mut().find(|issue| issue.reference.id == "2").expect("closed").state = IssueState::Closed;
        candidates.iter_mut().find(|issue| issue.reference.id == "3").expect("unlabelled").labels.clear();
        for (id, label) in [("4", "grilling"), ("5", "wayfinder:map"), ("6", "brainstorm")] {
            candidates.iter_mut().find(|issue| issue.reference.id == id).expect("ideation").labels.push(label.into());
        }
        candidates.iter_mut().find(|issue| issue.reference.id == "13").expect("body").body = Some("## Blocked by\n#12".into());
        let duplicates = candidates.iter().find(|issue| issue.reference.id == "1").expect("valid").clone();
        candidates.extend([duplicates.clone(), duplicates]);
        let (backend, issues, _, reconciler) =
            harness(candidates, vec![issue("12", &[], None, IssueState::Open), issue("15", &[], None, IssueState::Closed)], policy(60))
                .await;
        for (id, kind) in [("7", "Grill"), ("8", "Map"), ("9", "Brainstorm")] {
            issues.facts.lock().expect("facts").insert(
                IssueRef { source: source(), id: id.into() },
                DispatchIssueFacts { issue_type: Some(kind.into()), ..Default::default() },
            );
        }
        issues.facts.lock().expect("facts").insert(
            IssueRef { source: source(), id: "10".into() },
            DispatchIssueFacts { has_open_pull_request: true, ..Default::default() },
        );
        native_edge(&issues, "11", "12");
        native_edge(&issues, "14", "15");
        reconciler.reconcile_once().await.expect("pass");
        let queue = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status").dispatch_queue;
        assert_eq!(queue.iter().map(|entry| entry.issue.id.as_str()).collect::<Vec<_>>(), ["1", "13", "14"]);
    }

    // Contract #2782: every required input independently gates readiness;
    // body prose cannot create a native dependency.
    #[hegel::test]
    fn predicate_inputs_are_conjunctive(tc: hegel::TestCase) {
        use hegel::generators as gs;
        let open = tc.draw(gs::booleans());
        let ready_label = tc.draw(gs::booleans());
        let native_blocked = tc.draw(gs::booleans());
        let serving_pr = tc.draw(gs::booleans());
        let ideation = tc.draw(gs::integers::<usize>().min_value(0).max_value(6));
        let kinds = [None, Some("grill"), Some("map"), Some("brainstorm"), Some("Grilling"), Some("Map"), Some("Brainstorm")];
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        runtime.block_on(async {
            let mut candidate = issue(
                "2",
                if ready_label { &[READY_ISSUE_LABEL] } else { &[] },
                Some("## Blocked by\nUnparseable old convention #99"),
                if open { IssueState::Open } else { IssueState::Closed },
            );
            if ideation > 0 && ideation < 4 {
                candidate.labels.push(kinds[ideation].expect("kind").into());
            }
            let reference = candidate.reference.clone();
            let (backend, issues, _, reconciler) =
                harness(vec![candidate], vec![issue("9", &[], None, IssueState::Open)], policy(300)).await;
            issues.facts.lock().expect("facts").insert(
                reference,
                DispatchIssueFacts {
                    issue_type: if ideation >= 4 { kinds[ideation].map(str::to_string) } else { None },
                    blockers: if native_blocked { vec![IssueRef { source: source(), id: "9".into() }] } else { vec![] },
                    has_open_pull_request: serving_pr,
                    landed: false,
                },
            );
            assert_eq!(
                reconciler.reconcile_once().await.expect("pass").queued,
                usize::from(open && ready_label && !native_blocked && !serving_pr && ideation == 0)
            );
            assert!(backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys").items.is_empty());
        });
    }

    // Land-after is a separate relationship, retained as a cleared audit record.
    // Deploy-dependent holds require positive evidence for their installation;
    // neither issue closure, merge, nor another installation's receipt suffices.
    #[tokio::test]
    async fn holds_clear_on_landing_or_matching_deployment_and_stay_cleared() {
        use flotilla_resources::{DispatchDeploymentSpec, DispatchHoldSpec};
        for deploy in [false, true] {
            let candidate = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
            let target = issue("9", &[], None, IssueState::Closed);
            let reference = candidate.reference.clone();
            let after = target.reference.clone();
            let (backend, issues, clock, reconciler) = harness(vec![candidate], vec![target], policy(300)).await;
            let spec = DispatchHoldSpec::builder()
                .project_ref("widgets".into())
                .issue(reference)
                .land_after(after.clone())
                .reason("shared interface".into())
                .author("governor".into())
                .clear_when(if deploy { HoldClearWhen::Deployed { installation: "lab".into() } } else { HoldClearWhen::Landed })
                .build();
            let holds = backend.clone().using::<DispatchHold>(NAMESPACE);
            holds.create(&InputMeta::builder().name("serialise".into()).build(), &spec).await.expect("hold");
            assert_eq!(reconciler.reconcile_once().await.expect("closed only").queued, 0);
            issues.facts.lock().expect("facts").insert(after.clone(), DispatchIssueFacts { landed: true, ..Default::default() });
            if deploy {
                assert_eq!(reconciler.reconcile_once().await.expect("merged only").queued, 0);
                let receipts = backend.clone().using::<DispatchDeployment>(NAMESPACE);
                for installation in ["other", "lab"] {
                    receipts
                        .create(
                            &InputMeta::builder().name(installation.into()).build(),
                            &DispatchDeploymentSpec::builder()
                                .issue(after.clone())
                                .installation(installation.into())
                                .revision("abc123".into())
                                .deployed_at(clock.now())
                                .build(),
                        )
                        .await
                        .expect("receipt");
                    assert_eq!(reconciler.reconcile_once().await.expect("deployment").queued, usize::from(installation == "lab"));
                }
            } else {
                assert_eq!(reconciler.reconcile_once().await.expect("landed").queued, 1);
            }
            assert!(holds.get("serialise").await.expect("hold").status.expect("status").cleared_at.is_some());
            issues.facts.lock().expect("facts").clear();
            assert_eq!(reconciler.reconcile_once().await.expect("latched clear").queued, 1);
        }
    }
}
