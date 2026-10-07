use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use flotilla_core::{
    dispatch_footprints::{ConflictBoard, ConflictMeasurement},
    dispatch_missions::MissionBoard,
    in_process::InProcessDaemon,
};
use flotilla_protocol::{
    issue_query::{IssueQuery, READY_ISSUE_LABEL},
    DispatchIssueFacts, Issue, IssueRef, IssueState, QueryScope,
};
use flotilla_resources::{
    apply_status_patch, content_hash, pinned_workflow_ref, Clock, Convoy, ConvoyPhase, DispatchDeployment, DispatchHold, DispatchHoldSpec,
    DispatchHoldStatusPatch, DispatchObservation, DispatchObservationSpec, DispatchOverlap, DispatchOverlapSpec, DispatchPolicy,
    DispatchQueueAttention, DispatchQueueEntry, HoldClearWhen, InputMeta, Project, ProjectStatusPatch, ResolvedIssueSourceBinding,
    ResourceBackend, ResourceError, ResourceObject, SystemClock, WorkflowTemplate, DISPATCH_RECONCILER_PROVENANCE,
};
use tracing::{info, warn};

const ISSUE_PAGE_SIZE: usize = 100;
const OVERLAP_RETENTION_DAYS: i64 = 30;
const OVERLAP_RECORD_LIMIT: usize = 2048;

#[async_trait]
pub(crate) trait DispatchIssueSource: Send + Sync {
    async fn boards(&self, project: &ResourceObject<Project>) -> Result<Vec<flotilla_protocol::DispatchBoardRepository>, String>;
    async fn ready_issues(&self, project: &ResourceObject<Project>) -> Result<Vec<Issue>, String>;
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
    async fn boards(&self, project: &ResourceObject<Project>) -> Result<Vec<flotilla_protocol::DispatchBoardRepository>, String> {
        self.daemon.dispatch_board_repositories_internal(Some(&project.metadata.name)).await
    }
    async fn ready_issues(&self, project: &ResourceObject<Project>) -> Result<Vec<Issue>, String> {
        let scope = QueryScope::new(&project.metadata.namespace, &project.metadata.name);
        let bindings = self.daemon.resolve_issue_source_bindings(&scope).await?;
        let mut issues = Vec::new();
        for binding in bindings {
            let provider = self.daemon.issue_provider_for_source(&binding.source).await?;
            let query = ready_issue_query(&binding);
            let mut page = 1;
            loop {
                let result = provider.query(&binding.source, &query, page, ISSUE_PAGE_SIZE).await?;
                issues.extend(result.items);
                if !result.has_more {
                    break;
                }
                page += 1;
            }
        }
        Ok(issues)
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
    overlap_backend: ResourceBackend,
    namespace: String,
    issues: Arc<dyn DispatchIssueSource>,
    clock: Arc<dyn Clock>,
}

impl DispatchReconciler {
    pub(crate) fn new(backend: ResourceBackend, namespace: impl Into<String>, issues: Arc<dyn DispatchIssueSource>) -> Self {
        Self { overlap_backend: backend.clone(), backend, namespace: namespace.into(), issues, clock: Arc::new(SystemClock) }
    }

    #[cfg(test)]
    fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub(crate) async fn reconcile_once(&self) -> Result<ReconcilePass, String> {
        let projects = self.backend.clone().using::<Project>(&self.namespace).list().await.map_err(|error| error.to_string())?;
        let mut total = ReconcilePass::default();
        for project in projects.items {
            match self.reconcile_project(&project, self.clock.now()).await {
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

    async fn reconcile_project(&self, project: &ResourceObject<Project>, now: DateTime<Utc>) -> Result<ReconcilePass, String> {
        let Some(policy) = project.spec.dispatch_policy.as_ref().filter(|policy| policy.enabled) else {
            self.record_overlaps_best_effort(project, Vec::new(), now).await;
            self.replace_queue(project, Vec::new(), None).await?;
            self.set_queue_error(project, None).await?;
            return Ok(ReconcilePass::default());
        };

        let convoys = self.namespace_convoys(project).await?;
        let existing =
            convoys.iter().filter(|convoy| convoy.spec.project_ref.as_deref() == Some(&project.metadata.name)).cloned().collect::<Vec<_>>();
        let previous_queue = project.status.as_ref().map(|status| status.dispatch_queue.as_slice()).unwrap_or_default();
        let observations_recorded = self.observe_dispatches(project, previous_queue, &existing, now).await?;
        let boards = self.issues.boards(project).await?;
        let held = self.active_holds(project, &convoys, &boards, now).await?;
        let repository_sources = if policy.overlap_policy.is_some() {
            let repositories = self
                .backend
                .clone()
                .including_replicas::<flotilla_resources::Repository>(&project.metadata.namespace)
                .list()
                .await
                .map_err(|e| e.to_string())?;
            flotilla_core::dispatch_footprints::repository_sources(repositories.items.into_iter().map(|item| item.object))
        } else {
            BTreeMap::new()
        };
        let conflicts = ConflictBoard::new(&boards, &convoys, &repository_sources);
        let dispatched = existing
            .iter()
            .filter(|convoy| !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()))
            .flat_map(|convoy| convoy.spec.issues.iter().map(|issue| issue.reference.clone()))
            .collect::<HashSet<_>>();
        let previous_by_issue = previous_queue.iter().map(|entry| (entry.issue.clone(), entry)).collect::<BTreeMap<_, _>>();

        let board = MissionBoard::new(&boards)?;
        if policy.overlap_policy.is_some() && !boards.iter().any(|b| b.footprints.is_some()) {
            return Err("awaiting a complete footprint observation".into());
        }
        let live = existing.iter().filter(|convoy| !convoy.status.as_ref().is_some_and(|s| s.phase.is_terminal())).collect::<Vec<_>>();
        let project_active_crews = live.iter().map(|c| active_crew_count(c.status.as_ref())).sum();
        let mut mission_active = BTreeMap::<String, usize>::new();
        let mut overlap_measurements = Vec::new();
        for convoy in &live {
            let mut missions = HashSet::new();
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
                if policy.overlap_policy.is_some() {
                    let mut observed_issue = issue.clone();
                    observed_issue.body = served.snapshot.body.clone();
                    let mut measurements = conflicts.measurements(&observed_issue, Some(&convoy.metadata.name));
                    measurements.extend(conflicts.outcomes(&convoy.metadata.name));
                    overlap_measurements
                        .extend(measurements.into_iter().map(|measurement| (observed_issue.reference.clone(), measurement)));
                }
                missions.insert(board.score(&issue, policy)?.mission);
            }
            for mission in missions {
                *mission_active.entry(mission).or_default() += active_crew_count(convoy.status.as_ref());
            }
        }
        let mut ready = self.issues.ready_issues(project).await?;
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
            let facts = self.issues.dispatch_facts(&issue.reference).await?;
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
                match self.issues.fetch_issue(&blocker).await {
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
            if let Some(overlap_policy) = &policy.overlap_policy {
                let measurements = conflicts.measurements(&issue, None);
                overlap_measurements.extend(measurements.iter().cloned().map(|measurement| (issue.reference.clone(), measurement)));
                let holds = self.backend.clone().using::<DispatchHold>(&project.metadata.namespace);
                let existing_holds = holds
                    .list()
                    .await
                    .map_err(|e| e.to_string())?
                    .items
                    .into_iter()
                    .map(|h| (h.metadata.name.clone(), h))
                    .collect::<BTreeMap<_, _>>();
                let mut desired = BTreeMap::new();
                for measurement in &measurements {
                    if measurement.weight >= overlap_policy.land_after_threshold {
                        let identity = serde_json::json!([
                            project.metadata.name,
                            issue.reference,
                            measurement.source,
                            measurement.target,
                            measurement.revision,
                            measurement.weight,
                            measurement.files
                        ]);
                        let mut name = format!("overlap-{}", content_hash(&identity).map_err(|e| e.to_string())?);
                        // Cleared relationships remain immutable evidence. A recurring
                        // overlap receives a fresh relationship, even after a revert.
                        while let Some(cleared_at) = existing_holds.get(&name).and_then(|h| h.status.as_ref()).and_then(|s| s.cleared_at) {
                            name = format!("overlap-{}", content_hash(&serde_json::json!([name, cleared_at])).map_err(|e| e.to_string())?);
                        }
                        desired.insert(name, measurement);
                    } else {
                        score.conflict_penalty = score.conflict_penalty.saturating_add(measurement.weight);
                    }
                }
                for hold in existing_holds.values() {
                    if hold.spec.author == "dispatch-footprints"
                        && hold.spec.project_ref == project.metadata.name
                        && hold.spec.issue == issue.reference
                        && !desired.contains_key(&hold.metadata.name)
                        && !hold.status.as_ref().is_some_and(|s| s.cleared_at.is_some())
                    {
                        apply_status_patch(&holds, &hold.metadata.name, &DispatchHoldStatusPatch::Clear { at: now })
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
                let mut automatically_held = false;
                for (name, measurement) in desired {
                    match holds.get(&name).await {
                        Ok(hold) => {
                            automatically_held |= !hold.status.as_ref().is_some_and(|s| s.cleared_at.is_some());
                        }
                        Err(ResourceError::NotFound { .. }) => {
                            holds
                                .create(
                                    &InputMeta::builder().name(name).build(),
                                    &DispatchHoldSpec::builder()
                                        .project_ref(project.metadata.name.clone())
                                        .issue(issue.reference.clone())
                                        .land_after(issue.reference.clone())
                                        .land_after_work(measurement.target.clone())
                                        .reason(format!("rarity-weighted overlap {}: {}", measurement.weight, measurement.files.join(", ")))
                                        .author("dispatch-footprints".into())
                                        .clear_when(HoldClearWhen::Landed)
                                        .build(),
                                )
                                .await
                                .map_err(|e| e.to_string())?;
                            automatically_held = true;
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                }
                if automatically_held {
                    blocked += 1;
                    continue;
                }
            }
            score.project_active_crews = project_active_crews;
            score.mission_active_crews = mission_active.get(&score.mission).copied().unwrap_or(0);
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
        self.record_overlaps_best_effort(project, overlap_measurements, now).await;
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

    async fn record_overlaps_best_effort(
        &self,
        project: &ResourceObject<Project>,
        measurements: Vec<(IssueRef, ConflictMeasurement)>,
        now: DateTime<Utc>,
    ) {
        if let Err(error) = self.record_overlaps(project, measurements, now).await {
            warn!(project = %project.metadata.name, %error, "overlap telemetry unavailable; dispatch control continues");
        }
    }

    async fn record_overlaps(
        &self,
        project: &ResourceObject<Project>,
        measurements: Vec<(IssueRef, ConflictMeasurement)>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let observations = self.overlap_backend.clone().using::<DispatchOverlap>(&project.metadata.namespace);
        // One inventory read per Project pass, never one get per issue/work pair.
        let mut records = observations
            .list()
            .await
            .map_err(|e| e.to_string())?
            .items
            .into_iter()
            .filter(|r| r.spec.project_ref == project.metadata.name)
            .collect::<Vec<_>>();
        records.sort_by(|a, b| a.spec.observed_at.cmp(&b.spec.observed_at).then(a.metadata.name.cmp(&b.metadata.name)));
        let cutoff = now - Duration::days(OVERLAP_RETENTION_DAYS);
        for record in &records {
            if record.spec.observed_at < cutoff {
                observations.delete(&record.metadata.name).await.map_err(|e| e.to_string())?;
            }
        }
        records.retain(|record| record.spec.observed_at >= cutoff);
        let mut latest = BTreeMap::new();
        for record in &records {
            if record.spec.observed_at >= cutoff {
                latest.insert(
                    (record.spec.issue.clone(), record.spec.source.clone(), record.spec.target.clone(), record.spec.outcome),
                    record.spec.clone(),
                );
            }
        }
        let mut names = records.iter().map(|r| r.metadata.name.clone()).collect::<HashSet<_>>();
        for (issue, measurement) in measurements {
            let key = (issue.clone(), measurement.source.clone(), measurement.target.clone(), measurement.outcome);
            // Retain a zero-weight transition out of a previously overlapping pair,
            // but do not persist the otherwise quadratic set of disjoint pairs.
            if !measurement.outcome
                && measurement.weight == 0
                && measurement.conflicts != Some(true)
                && !latest.get(&key).is_some_and(|previous| previous.weight > 0)
            {
                continue;
            }
            let spec = DispatchOverlapSpec {
                outcome: measurement.outcome,
                project_ref: project.metadata.name.clone(),
                source: measurement.source,
                issue,
                target: measurement.target,
                candidate_actual: measurement.candidate_actual,
                target_actual: measurement.target_actual,
                revision: measurement.revision,
                weight: measurement.weight,
                files: measurement.files,
                conflicts: measurement.conflicts,
                observed_at: now,
            };
            if latest.get(&key).is_some_and(|previous| {
                let mut previous = previous.clone();
                previous.observed_at = now;
                previous == spec
            }) {
                continue;
            }
            let mut identity = serde_json::to_value(&spec).map_err(|e| e.to_string())?;
            identity.as_object_mut().expect("overlap object").remove("observed_at");
            let name = format!("overlap-{}", content_hash(&identity).map_err(|e| e.to_string())?);
            if names.insert(name.clone()) {
                let record = observations.create(&InputMeta::builder().name(name).build(), &spec).await.map_err(|e| e.to_string())?;
                records.push(record);
            }
            latest.insert(key, spec);
        }
        records.sort_by(|a, b| a.spec.observed_at.cmp(&b.spec.observed_at).then(a.metadata.name.cmp(&b.metadata.name)));
        let surplus = records.len().saturating_sub(OVERLAP_RECORD_LIMIT);
        for (index, record) in records.iter().enumerate() {
            if index < surplus {
                observations.delete(&record.metadata.name).await.map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    async fn active_holds(
        &self,
        project: &ResourceObject<Project>,
        convoys: &[ResourceObject<Convoy>],
        boards: &[flotilla_protocol::DispatchBoardRepository],
        now: DateTime<Utc>,
    ) -> Result<HashSet<IssueRef>, String> {
        let holds =
            self.backend.definitions::<DispatchHold>(&project.metadata.namespace).list().await.map_err(|error| error.to_string())?;
        let deployments = self
            .backend
            .clone()
            .including_replicas::<DispatchDeployment>(&project.metadata.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?;
        let mut active = HashSet::new();
        for hold in holds {
            if hold.spec.project_ref != project.metadata.name || hold.status.as_ref().is_some_and(|status| status.cleared_at.is_some()) {
                continue;
            }
            let cleared = if let Some(target) = &hold.spec.land_after_work {
                match target {
                    flotilla_protocol::FootprintTarget::Convoy { name } => convoys
                        .iter()
                        .any(|c| &c.metadata.name == name && c.status.as_ref().is_some_and(|s| s.phase == ConvoyPhase::Landed)),
                    flotilla_protocol::FootprintTarget::PullRequest { url } => {
                        boards.iter().flat_map(|b| &b.pull_requests).any(|pr| &pr.url == url && pr.merged_at.is_some())
                    }
                }
            } else {
                match &hold.spec.clear_when {
                    HoldClearWhen::Landed => {
                        let convoy_landed = convoys.iter().any(|convoy| {
                            convoy.status.as_ref().is_some_and(|status| status.phase == ConvoyPhase::Landed)
                                && convoy.spec.issues.iter().any(|issue| issue.reference == hold.spec.land_after)
                        });
                        convoy_landed || self.issues.dispatch_facts(&hold.spec.land_after).await.map(|facts| facts.landed).unwrap_or(false)
                    }
                    HoldClearWhen::Deployed { installation } => deployments.items.iter().any(|deployment| {
                        deployment.object.spec.issue == hold.spec.land_after && deployment.object.spec.installation == *installation
                    }),
                }
            };
            let automatic_disabled = hold.spec.author == "dispatch-footprints"
                && project.spec.dispatch_policy.as_ref().is_none_or(|p| p.overlap_policy.is_none());
            if cleared || automatic_disabled {
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
            } else if hold.spec.author != "dispatch-footprints" {
                active.insert(hold.spec.issue);
            }
        }
        Ok(active)
    }

    async fn namespace_convoys(&self, project: &ResourceObject<Project>) -> Result<Vec<ResourceObject<Convoy>>, String> {
        let listed = self
            .backend
            .clone()
            .including_replicas::<Convoy>(&project.metadata.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?;
        let mut by_name = BTreeMap::new();
        for convoy in listed.items.into_iter().map(|item| item.object) {
            by_name.entry(convoy.metadata.name.clone()).or_insert(convoy);
        }
        Ok(by_name.into_values().collect())
    }

    async fn observe_dispatches(
        &self,
        project: &ResourceObject<Project>,
        previous_queue: &[DispatchQueueEntry],
        convoys: &[ResourceObject<Convoy>],
        now: DateTime<Utc>,
    ) -> Result<usize, String> {
        let queued = previous_queue.iter().map(|entry| (&entry.issue, entry)).collect::<BTreeMap<_, _>>();
        let observations = self.backend.clone().using::<DispatchObservation>(&project.metadata.namespace);
        let workflows = self.backend.definitions::<WorkflowTemplate>(&project.metadata.namespace);
        let mut recorded = 0;
        for convoy in convoys {
            for issue in &convoy.spec.issues {
                let Some(queue_entry) = queued.get(&issue.reference) else { continue };
                let identity = serde_json::json!({
                    "project": project.metadata.name,
                    "convoy": convoy.metadata.name,
                    "issue": issue.reference,
                });
                let name = format!("dispatch-{}", content_hash(&identity).map_err(|error| error.to_string())?);
                match observations.get(&name).await {
                    Ok(_) => continue,
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error.to_string()),
                }
                match workflows.get(pinned_workflow_ref(convoy)).await {
                    Ok(_) => {}
                    Err(error) => {
                        warn!(
                            project = %project.metadata.name,
                            convoy = %convoy.metadata.name,
                            issue = %issue.reference.id,
                            %error,
                            "cannot record dispatch observation without its workflow; continuing queue reconciliation"
                        );
                        continue;
                    }
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
    use flotilla_resources::{
        single_agent_workflow_spec, ConvoyIssue, ConvoySpec, InputValue, IssueSnapshot, ProjectSpec, RepositoryKey, VirtualClock,
    };

    use super::*;

    const NAMESPACE: &str = "flotilla";

    // Stands in for the external tracker; storage/reconciliation remain real.
    struct FakeIssues {
        board_override: Mutex<Option<Vec<flotilla_protocol::DispatchBoardRepository>>>,
        ready: Mutex<Vec<Issue>>,
        by_ref: Mutex<HashMap<IssueRef, Issue>>,
        ready_calls: Mutex<usize>,
        facts: Mutex<HashMap<IssueRef, DispatchIssueFacts>>,
        failing_projects: Mutex<HashSet<String>>,
        failing_refs: Mutex<HashSet<IssueRef>>,
    }

    #[async_trait]
    impl DispatchIssueSource for FakeIssues {
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
                footprints: None,
            }])
        }
        async fn ready_issues(&self, project: &ResourceObject<Project>) -> Result<Vec<Issue>, String> {
            if self.failing_projects.lock().expect("failing projects lock").contains(&project.metadata.name) {
                return Err("issue source unavailable".to_string());
            }
            *self.ready_calls.lock().expect("ready calls lock") += 1;
            Ok(self.ready.lock().expect("ready lock").clone())
        }

        async fn fetch_issue(&self, reference: &IssueRef) -> Result<Issue, String> {
            if self.failing_refs.lock().expect("failing refs lock").contains(reference) {
                return Err("issue fetch unavailable".to_string());
            }
            self.by_ref.lock().expect("issues lock").get(reference).cloned().ok_or_else(|| "missing issue".to_string())
        }
        async fn dispatch_facts(&self, reference: &IssueRef) -> Result<DispatchIssueFacts, String> {
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

        assert_eq!(ready_issue_query(&binding), IssueQuery {
            search: None,
            label: Some(READY_ISSUE_LABEL.to_string()),
            match_fields: BTreeMap::from([("component".to_string(), vec!["terminal".to_string()])]),
        });
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
            .create(&InputMeta::builder().name("widgets".to_string()).build(), &ProjectSpec {
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
            })
            .await
            .expect("project");
        let issues = Arc::new(FakeIssues {
            board_override: Mutex::new(None),
            ready: Mutex::new(ready),
            by_ref: Mutex::new(blockers.into_iter().map(|issue| (issue.reference.clone(), issue)).collect()),
            ready_calls: Mutex::new(0),
            facts: Mutex::new(HashMap::new()),
            failing_projects: Mutex::new(Default::default()),
            failing_refs: Mutex::new(Default::default()),
        });
        let clock = Arc::new(VirtualClock::new("2026-08-04T12:00:00Z".parse().expect("clock timestamp")));
        let reconciler = DispatchReconciler::new(backend.clone(), NAMESPACE, Arc::clone(&issues) as Arc<dyn DispatchIssueSource>)
            .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        (backend, issues, clock, reconciler)
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
    // #2784: below threshold adds a score penalty; the boundary creates an
    // immutable automatic hold, and replacement with a disjoint diff clears it.
    #[tokio::test]
    async fn overlap_threshold_logs_and_holds_clear_after_diff_changes() {
        use flotilla_protocol::{FileFootprint, FootprintObservation, FootprintTarget, WorkFootprint};
        for threshold in [4, 5, 6] {
            let mut dispatch_policy = policy(60);
            dispatch_policy.overlap_policy = Some(flotilla_resources::OverlapPolicy { land_after_threshold: threshold });
            let (backend, issues, _, reconciler) = harness(
                vec![issue("1", &[READY_ISSUE_LABEL], Some("## Touches\n- src/rare.rs"), IssueState::Open)],
                vec![],
                dispatch_policy,
            )
            .await;
            let projects = backend.using::<Project>(NAMESPACE);
            let mut boards = issues.boards(&projects.get("widgets").await.expect("project")).await.expect("board");
            boards[0].footprints = Some(FootprintObservation {
                history: vec![FileFootprint::default(); 4],
                work: vec![WorkFootprint {
                    target: FootprintTarget::PullRequest { url: "https://github.com/acme/widgets/pull/99".into() },
                    convoy: None,
                    footprint: FileFootprint { files: BTreeMap::from([("src/rare.rs".into(), false)]) },
                    actual: true,
                    revision: "first".into(),
                    conflicts: Some(false),
                }],
                ..Default::default()
            });
            *issues.board_override.lock().expect("board") = Some(boards.clone());
            let outcome = reconciler.reconcile_once().await.expect("overlap pass");
            assert_eq!(outcome.project_errors, 0);
            assert_eq!(outcome.blocked, usize::from(threshold <= 5));
            assert_eq!(outcome.queued, usize::from(threshold > 5));
            let first = projects.get("widgets").await.expect("project").status.unwrap_or_default();
            if threshold > 5 {
                assert_eq!(first.dispatch_queue[0].score.as_ref().expect("score").conflict_penalty, 5);
            } else {
                assert!(first.dispatch_queue.is_empty());
            }
            let observations = backend.using::<DispatchOverlap>(NAMESPACE);
            assert_eq!(observations.list().await.expect("observations").items.len(), 1);
            reconciler.reconcile_once().await.expect("identical pass");
            assert_eq!(observations.list().await.expect("deduped").items.len(), 1);
            let original = boards.clone();
            let work = &mut boards[0].footprints.as_mut().expect("footprints").work[0];
            work.footprint.files = BTreeMap::from([("src/disjoint.rs".into(), false)]);
            work.revision = "second".into();
            work.conflicts = Some(true);
            *issues.board_override.lock().expect("board") = Some(boards);
            reconciler.reconcile_once().await.expect("replacement pass");
            let recovered = projects.get("widgets").await.expect("project").status.expect("status");
            assert_eq!(recovered.dispatch_queue.len(), 1);
            assert_eq!(recovered.dispatch_queue[0].score.as_ref().expect("score").conflict_penalty, 0);
            assert_eq!(observations.list().await.expect("observations").items.len(), 2);
            assert!(observations.list().await.expect("observations").items.iter().any(|o| o.spec.conflicts == Some(true)));
            for hold in backend.using::<DispatchHold>(NAMESPACE).list().await.expect("holds").items {
                assert!(hold.status.expect("status").cleared_at.is_some());
            }
            // A revert can restore the identical revision and overlap. Historical
            // cleared holds stay cleared, but a new relationship must block again.
            *issues.board_override.lock().expect("board") = Some(original);
            let recurrent = reconciler.reconcile_once().await.expect("recurrent pass");
            assert_eq!(recurrent.blocked, usize::from(threshold <= 5));
            assert_eq!(recurrent.queued, usize::from(threshold > 5));
            let holds = backend.using::<DispatchHold>(NAMESPACE).list().await.expect("holds").items;
            assert_eq!(
                holds.iter().filter(|h| !h.status.as_ref().is_some_and(|s| s.cleared_at.is_some())).count(),
                usize::from(threshold <= 5)
            );
            // A telemetry storage failure must neither unblock a required hold nor
            // withhold an otherwise-ready proposal. This real backend rejects an
            // invalid URL before I/O, isolating the evidence-store failure boundary.
            let mut telemetry_failure = reconciler;
            telemetry_failure.overlap_backend =
                ResourceBackend::Http(flotilla_resources::HttpBackend::new(flotilla_resources::tls::client(), "invalid URL"));
            let survived = telemetry_failure.reconcile_once().await.expect("telemetry failure pass");
            assert_eq!(survived.project_errors, 0);
            assert_eq!(survived.blocked, recurrent.blocked);
            assert_eq!(survived.queued, recurrent.queued);
        }
    }

    // Evidence retention is bounded independently of sliding-window/revision
    // churn. Identical passes and initial disjoint pairs create no extra records;
    // unknown/clear transitions and standalone outcomes remain distinguishable.
    #[tokio::test]
    async fn overlap_evidence_retention_and_sparse_pairs() {
        use flotilla_protocol::FootprintTarget;
        let (backend, _, clock, reconciler) = harness(vec![], vec![], policy(60)).await;
        let project = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project");
        let candidate = issue("1", &[READY_ISSUE_LABEL], None, IssueState::Open).reference;
        let measurement = ConflictMeasurement {
            outcome: false,
            source: source(),
            target: FootprintTarget::Convoy { name: "other".into() },
            candidate_actual: false,
            target_actual: true,
            revision: "first".into(),
            conflicts: None,
            weight: 0,
            files: vec![],
        };
        let observations = backend.using::<DispatchOverlap>(NAMESPACE);
        reconciler.record_overlaps(&project, vec![(candidate.clone(), measurement.clone())], clock.now()).await.expect("disjoint");
        assert!(observations.list().await.expect("empty evidence").items.is_empty());
        let mut overlapping = measurement.clone();
        overlapping.weight = 1;
        overlapping.files = vec!["src/a.rs".into()];
        reconciler.record_overlaps(&project, vec![(candidate.clone(), overlapping.clone())], clock.now()).await.expect("overlap");
        reconciler.record_overlaps(&project, vec![(candidate.clone(), overlapping)], clock.now()).await.expect("identical");
        assert_eq!(observations.list().await.expect("dedup").items.len(), 1);
        reconciler
            .record_overlaps(&project, vec![(candidate.clone(), measurement.clone())], clock.now())
            .await
            .expect("disjoint transition");
        assert_eq!(observations.list().await.expect("transition").items.len(), 2);
        let mut conflict = measurement.clone();
        conflict.conflicts = Some(true);
        let mut outcome = measurement.clone();
        outcome.outcome = true;
        reconciler
            .record_overlaps(&project, vec![(candidate.clone(), conflict), (candidate.clone(), outcome)], clock.now())
            .await
            .expect("conflict and outcome");
        assert_eq!(observations.list().await.expect("evidence").items.len(), 4);
        let samples = (0..=OVERLAP_RECORD_LIMIT)
            .map(|index| {
                let mut sample = measurement.clone();
                sample.weight = (index % 61) as u64 + 1;
                sample.files = vec!["src/a.rs".into()];
                sample.revision = format!("revision-{index}");
                (candidate.clone(), sample)
            })
            .collect();
        clock.advance(Duration::seconds(1));
        reconciler.record_overlaps(&project, samples, clock.now()).await.expect("history churn");
        assert_eq!(observations.list().await.expect("bounded evidence").items.len(), OVERLAP_RECORD_LIMIT);
        clock.advance(Duration::days(OVERLAP_RETENTION_DAYS + 1));
        reconciler.record_overlaps(&project, vec![], clock.now()).await.expect("expiry");
        assert!(observations.list().await.expect("expired evidence").items.is_empty());
        let mut renewed = measurement;
        renewed.weight = (OVERLAP_RECORD_LIMIT % 61) as u64 + 1;
        renewed.files = vec!["src/a.rs".into()];
        renewed.revision = format!("revision-{}", OVERLAP_RECORD_LIMIT);
        reconciler.record_overlaps(&project, vec![(candidate, renewed)], clock.now()).await.expect("renew expired evidence");
        assert_eq!(observations.list().await.expect("fresh evidence").items.len(), 1);
    }

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
            .create(&InputMeta::builder().name("human-dispatch".to_string()).build(), &ConvoySpec {
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
            })
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
            ConvoyPhase::Anchored,
            ConvoyPhase::Landing,
            ConvoyPhase::Landed,
            ConvoyPhase::Failed,
            ConvoyPhase::Cancelled,
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
            .create(&InputMeta::builder().name("missing-workflow-dispatch".to_string()).build(), &ConvoySpec {
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
            })
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
            issues.facts.lock().expect("facts").insert(IssueRef { source: source(), id: id.into() }, DispatchIssueFacts {
                issue_type: Some(kind.into()),
                ..Default::default()
            });
        }
        issues.facts.lock().expect("facts").insert(IssueRef { source: source(), id: "10".into() }, DispatchIssueFacts {
            has_open_pull_request: true,
            ..Default::default()
        });
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
            issues.facts.lock().expect("facts").insert(reference, DispatchIssueFacts {
                issue_type: if ideation >= 4 { kinds[ideation].map(str::to_string) } else { None },
                blockers: if native_blocked { vec![IssueRef { source: source(), id: "9".into() }] } else { vec![] },
                has_open_pull_request: serving_pr,
                landed: false,
            });
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
