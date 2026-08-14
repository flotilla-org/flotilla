use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{
    issue_query::{IssueQuery, READY_ISSUE_LABEL},
    Issue, IssueRef, IssueState, QueryScope,
};
use flotilla_resources::{
    apply_status_patch, content_hash, pinned_workflow_ref, Clock, Convoy, DispatchObservation, DispatchObservationSpec, DispatchPolicy,
    DispatchQueueAttention, DispatchQueueEntry, InputMeta, Project, ProjectStatusPatch, ResourceBackend, ResourceError, ResourceObject,
    SystemClock, WorkflowTemplate, DISPATCH_RECONCILER_PROVENANCE,
};
use tracing::{info, warn};

const ISSUE_PAGE_SIZE: usize = 100;

#[async_trait]
pub(crate) trait DispatchIssueSource: Send + Sync {
    async fn ready_issues(&self, project: &ResourceObject<Project>) -> Result<Vec<Issue>, String>;
    async fn fetch_issue(&self, reference: &IssueRef) -> Result<Issue, String>;
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
    async fn ready_issues(&self, project: &ResourceObject<Project>) -> Result<Vec<Issue>, String> {
        let scope = QueryScope::new(&project.metadata.namespace, &project.metadata.name);
        let sources = self.daemon.resolve_issue_sources(&scope).await?;
        let mut issues = Vec::new();
        for source in sources {
            let provider = self.daemon.issue_provider_for_source(&source).await?;
            let mut page = 1;
            loop {
                let result = provider
                    .query(&source, &IssueQuery { search: None, label: Some(READY_ISSUE_LABEL.to_string()) }, page, ISSUE_PAGE_SIZE)
                    .await?;
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
}

impl DispatchReconciler {
    pub(crate) fn new(backend: ResourceBackend, namespace: impl Into<String>, issues: Arc<dyn DispatchIssueSource>) -> Self {
        Self { backend, namespace: namespace.into(), issues, clock: Arc::new(SystemClock) }
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
                    total.project_errors += 1;
                }
            }
        }
        Ok(total)
    }

    async fn reconcile_project(&self, project: &ResourceObject<Project>, now: DateTime<Utc>) -> Result<ReconcilePass, String> {
        let Some(policy) = project.spec.dispatch_policy.as_ref().filter(|policy| policy.enabled) else {
            self.replace_queue(project, Vec::new(), None).await?;
            return Ok(ReconcilePass::default());
        };

        let existing = self.project_convoys(project).await?;
        let previous_queue = project.status.as_ref().map(|status| status.dispatch_queue.as_slice()).unwrap_or_default();
        let observations_recorded = self.observe_dispatches(project, previous_queue, &existing, now).await?;
        // Admission itself makes an issue dispatched, regardless of the convoy's later phase. Failed,
        // cancelled, and abandoned convoys remain durable evidence of that decision; retrying requires
        // an explicit human/provider re-triage rather than automatic re-proposal from a terminal phase.
        let dispatched = existing
            .iter()
            .flat_map(|convoy| convoy.spec.issues.iter().map(|issue| issue.reference.clone()))
            .collect::<std::collections::HashSet<_>>();
        let previous_by_issue = previous_queue.iter().map(|entry| (entry.issue.clone(), entry)).collect::<BTreeMap<_, _>>();

        let mut ready = self.issues.ready_issues(project).await?;
        ready.retain(|issue| issue.state == IssueState::Open && issue.labels.iter().any(|label| label == READY_ISSUE_LABEL));
        ready.sort_by(|left, right| left.reference.cmp(&right.reference));

        let mut queue = Vec::new();
        let mut blocked = 0;
        for issue in ready {
            if dispatched.contains(&issue.reference) {
                continue;
            }
            let blockers = match blocked_by_references(&issue) {
                Ok(blockers) => blockers,
                Err(error) => {
                    warn!(project = %project.metadata.name, issue = %issue.reference.id, %error, "issue Blocked by section is unparseable; treating issue as blocked");
                    blocked += 1;
                    continue;
                }
            };
            let previous = previous_by_issue.get(&issue.reference).copied();
            let mut is_blocked = false;
            let mut blockers_unknown = false;
            for blocker in blockers {
                match self.issues.fetch_issue(&blocker).await {
                    Ok(blocker) if blocker.state == IssueState::Closed => {}
                    Ok(_) => is_blocked = true,
                    Err(error) => {
                        warn!(project = %project.metadata.name, issue = %issue.reference.id, blocker = %blocker.id, %error, "blocker could not be observed; preserving only a previously verified proposal");
                        blockers_unknown = true;
                    }
                }
            }
            if is_blocked {
                blocked += 1;
                continue;
            }
            if blockers_unknown {
                if let Some(previous) = previous {
                    queue.push(previous.clone());
                } else {
                    blocked += 1;
                }
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
            queue.push(DispatchQueueEntry {
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
        Ok(ReconcilePass { queued: queue.len(), blocked, observations_recorded, ..ReconcilePass::default() })
    }

    async fn project_convoys(&self, project: &ResourceObject<Project>) -> Result<Vec<ResourceObject<Convoy>>, String> {
        let listed = self
            .backend
            .clone()
            .including_replicas::<Convoy>(&project.metadata.namespace)
            .list()
            .await
            .map_err(|error| error.to_string())?;
        let mut by_name = BTreeMap::new();
        for convoy in listed.items.into_iter().map(|item| item.object) {
            if convoy.spec.project_ref.as_deref() == Some(&project.metadata.name) {
                by_name.entry(convoy.metadata.name.clone()).or_insert(convoy);
            }
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
        let workflows = self.backend.clone().using::<WorkflowTemplate>(&project.metadata.namespace);
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
                let workflow = match workflows.get(pinned_workflow_ref(convoy)).await {
                    Ok(workflow) => workflow,
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
                };
                let stance = workflow.spec.vessels.iter().map(|vessel| vessel.stance).max().unwrap_or_default();
                let dispatched_at = convoy.metadata.creation_timestamp;
                let time_from_ready_seconds =
                    dispatched_at.signed_duration_since(queue_entry.ready_observed_at).num_seconds().max(0) as u64;
                let spec = DispatchObservationSpec::builder()
                    .project_ref(project.metadata.name.clone())
                    .convoy_ref(convoy.metadata.name.clone())
                    .issue(issue.reference.clone())
                    .workflow_ref(convoy.spec.workflow_ref.clone())
                    .maybe_placement_policy(convoy.spec.placement_policy.clone())
                    .stance(stance)
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

fn dispatch_queue_attention(
    queue: &[DispatchQueueEntry],
    policy: &DispatchPolicy,
    previous: Option<&DispatchQueueAttention>,
    now: DateTime<Utc>,
) -> Option<DispatchQueueAttention> {
    let oldest = queue.first()?.ready_observed_at;
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

fn blocked_by_references(issue: &Issue) -> Result<Vec<IssueRef>, String> {
    let Some(body) = issue.body.as_deref() else { return Ok(Vec::new()) };
    let mut lines = body.lines();
    let Some(_) = lines.find(|line| line.trim().eq_ignore_ascii_case("## blocked by")) else {
        return Ok(Vec::new());
    };
    let section = lines.take_while(|line| !line.trim_start().starts_with("## ")).collect::<Vec<_>>().join("\n");
    if section.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut references = section.split_whitespace().filter_map(issue_url_reference).collect::<Vec<_>>();
    let bytes = section.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'#' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > start {
            let token_start = section[..index]
                .rfind(|character: char| character.is_whitespace() || matches!(character, '(' | '[' | '`'))
                .map_or(0, |position| position + 1);
            let prefix = section[token_start..index].trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '/');
            let source = issue_reference_source(&issue.reference.source, prefix).unwrap_or_else(|| issue.reference.source.clone());
            references.push(IssueRef { source, id: section[start..end].to_string() });
            index = end;
        } else {
            index += 1;
        }
    }
    if references.is_empty() {
        Err("section contains no issue references".to_string())
    } else {
        references.sort();
        references.dedup();
        Ok(references)
    }
}

fn issue_reference_source(current: &flotilla_protocol::IssueSource, prefix: &str) -> Option<flotilla_protocol::IssueSource> {
    let mut segments = prefix.split('/');
    let owner = segments.next()?;
    let repository = segments.next()?;
    if owner.is_empty() || repository.is_empty() || segments.next().is_some() {
        return None;
    }
    Some(flotilla_protocol::IssueSource { service: current.service.clone(), scope: format!("{owner}/{repository}") })
}

fn issue_url_reference(token: &str) -> Option<IssueRef> {
    let token = token.trim_matches(|character: char| matches!(character, '(' | ')' | '[' | ']' | '<' | '>' | ',' | '.' | ';' | '`'));
    let url = url::Url::parse(token).ok()?;
    let segments = url.path_segments()?.collect::<Vec<_>>();
    let [scope @ .., "issues", id] = segments.as_slice() else { return None };
    if scope.len() != 2 || id.is_empty() {
        return None;
    }
    let host = url.host_str()?;
    let service = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    Some(IssueRef { source: flotilla_protocol::IssueSource { service, scope: scope.join("/") }, id: (*id).to_string() })
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use flotilla_protocol::IssueSource;
    use flotilla_resources::{
        single_agent_contained_workflow_spec, ConvoyIssue, ConvoySpec, InputValue, IssueSnapshot, ProjectSpec, RepositoryKey, Stance,
        VirtualClock,
    };

    use super::*;

    const NAMESPACE: &str = "flotilla";

    struct FakeIssues {
        ready: Mutex<Vec<Issue>>,
        by_ref: Mutex<HashMap<IssueRef, Issue>>,
        ready_calls: Mutex<usize>,
        failing_projects: Mutex<std::collections::HashSet<String>>,
        failing_refs: Mutex<std::collections::HashSet<IssueRef>>,
    }

    #[async_trait]
    impl DispatchIssueSource for FakeIssues {
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
            as_of: "2026-08-04T12:00:00Z".parse().expect("timestamp"),
            observed_at: Some("2026-08-04T12:01:00Z".parse().expect("timestamp")),
            provider_name: "fake".to_string(),
            provider_display_name: "Fake".to_string(),
        }
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
                display_name: "Widgets".to_string(),
                default_workflow_ref: "implement".to_string(),
                issue_source: Some(source()),
                repositories: vec![flotilla_resources::ProjectRepositorySpec {
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
            ready: Mutex::new(ready),
            by_ref: Mutex::new(blockers.into_iter().map(|issue| (issue.reference.clone(), issue)).collect()),
            ready_calls: Mutex::new(0),
            failing_projects: Mutex::new(Default::default()),
            failing_refs: Mutex::new(Default::default()),
        });
        let clock = Arc::new(VirtualClock::new("2026-08-04T12:00:00Z".parse().expect("clock timestamp")));
        let reconciler = DispatchReconciler::new(backend.clone(), NAMESPACE, Arc::clone(&issues) as Arc<dyn DispatchIssueSource>)
            .with_clock(Arc::clone(&clock) as Arc<dyn Clock>);
        (backend, issues, clock, reconciler)
    }

    fn policy(stale_after_seconds: u64) -> DispatchPolicy {
        DispatchPolicy::builder().stale_after_seconds(stale_after_seconds).build()
    }

    #[tokio::test]
    async fn ready_unblocked_issues_are_proposed_without_creating_any_convoys() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let unlabeled = issue("1", &[], None, IssueState::Open);
        let blocked = issue("3", &[READY_ISSUE_LABEL], Some("## Blocked by\n\n#9"), IssueState::Open);
        let blocker = issue("9", &[], None, IssueState::Open);
        let (backend, _, _, reconciler) = harness(vec![unlabeled, blocked, ready.clone()], vec![blocker], policy(300)).await;

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
    async fn a_transient_blocker_fetch_failure_preserves_a_verified_proposals_ready_time() {
        let dependent = issue("2", &[READY_ISSUE_LABEL], Some("## Blocked by\n#9"), IssueState::Open);
        let newcomer = issue("3", &[READY_ISSUE_LABEL], Some("## Blocked by\n#9"), IssueState::Open);
        let blocker = issue("9", &[], None, IssueState::Closed);
        let blocker_ref = blocker.reference.clone();
        let (backend, issues, clock, reconciler) = harness(vec![dependent.clone()], vec![blocker], policy(60)).await;
        reconciler.reconcile_once().await.expect("verified pass");
        let original =
            backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status").dispatch_queue[0].clone();
        issues.ready.lock().expect("ready lock").push(newcomer);
        issues.failing_refs.lock().expect("failing refs lock").insert(blocker_ref);
        clock.advance(Duration::seconds(60));

        let outcome = reconciler.reconcile_once().await.expect("transient failure pass");

        assert_eq!(outcome.queued, 1);
        assert_eq!(outcome.blocked, 1, "an unverified new issue remains conservatively excluded");
        let status = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project").status.expect("status");
        assert_eq!(status.dispatch_queue, vec![original]);
        assert_eq!(status.dispatch_queue_attention.expect("stale attention").count, 1);
    }

    #[tokio::test]
    async fn manual_dispatch_of_a_queued_issue_records_the_a_decision_and_drops_it_from_the_queue() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let (backend, _, clock, reconciler) = harness(vec![ready.clone()], vec![], policy(300)).await;
        reconciler.reconcile_once().await.expect("queue pass");
        backend
            .clone()
            .using::<WorkflowTemplate>(NAMESPACE)
            .create(&InputMeta::builder().name("review-and-fix".to_string()).build(), &single_agent_contained_workflow_spec())
            .await
            .expect("workflow");
        backend
            .clone()
            .using::<Convoy>(NAMESPACE)
            .create(&InputMeta::builder().name("human-dispatch".to_string()).build(), &ConvoySpec {
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
        assert_eq!(observation.stance, Stance::Contained);
        assert_eq!(
            observation.time_from_ready_seconds,
            observation.dispatched_at.signed_duration_since(observation.ready_observed_at).num_seconds().max(0) as u64
        );
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

    #[test]
    fn prose_only_blocked_by_section_is_unparseable() {
        let issue = issue("2", &[READY_ISSUE_LABEL], Some("## Blocked by\n\nSequencing decision — later."), IssueState::Open);
        assert_eq!(blocked_by_references(&issue).expect_err("prose is not a reference"), "section contains no issue references");
    }

    #[test]
    fn blocked_by_parser_preserves_qualified_reference_sources() {
        let issue = issue(
            "2",
            &[READY_ISSUE_LABEL],
            Some("## Blocked by\n\nother/repo#7 and https://forgejo.example/team/widgets/issues/8"),
            IssueState::Open,
        );

        assert_eq!(blocked_by_references(&issue).expect("qualified references"), vec![
            IssueRef {
                source: IssueSource { service: "https://forgejo.example".to_string(), scope: "team/widgets".to_string() },
                id: "8".to_string(),
            },
            IssueRef {
                source: IssueSource { service: "https://github.com".to_string(), scope: "other/repo".to_string() },
                id: "7".to_string(),
            },
        ]);
    }
}
