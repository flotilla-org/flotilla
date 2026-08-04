use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_protocol::{
    issue_query::{IssueQuery, READY_ISSUE_LABEL},
    ConvoyAutoAttach, ConvoyStartIntent, Issue, IssueRef, IssueSelector, IssueState, QueryScope,
};
use flotilla_resources::{
    apply_status_patch, Convoy, DispatchAttention, DispatchPolicy, Project, ProjectStatusPatch, ResourceBackend, ResourceObject,
    DISPATCH_PROVENANCE_ANNOTATION,
};
use tracing::{debug, info, warn};

const ISSUE_PAGE_SIZE: usize = 100;

#[async_trait]
pub(crate) trait DispatchIssueSource: Send + Sync {
    async fn ready_issues(&self, project: &ResourceObject<Project>) -> Result<Vec<Issue>, String>;
    async fn fetch_issue(&self, reference: &IssueRef) -> Result<Issue, String>;
}

#[async_trait]
pub(crate) trait DispatchAdmission: Send + Sync {
    async fn admit(
        &self,
        project: &ResourceObject<Project>,
        issue: &Issue,
        policy: &DispatchPolicy,
        provenance: String,
    ) -> Result<String, String>;
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

pub(crate) struct DaemonDispatchAdmission {
    daemon: Arc<InProcessDaemon>,
}

impl DaemonDispatchAdmission {
    pub(crate) fn new(daemon: Arc<InProcessDaemon>) -> Self {
        Self { daemon }
    }
}

#[async_trait]
impl DispatchAdmission for DaemonDispatchAdmission {
    async fn admit(
        &self,
        project: &ResourceObject<Project>,
        issue: &Issue,
        policy: &DispatchPolicy,
        provenance: String,
    ) -> Result<String, String> {
        let intent = ConvoyStartIntent::builder()
            .namespace(project.metadata.namespace.clone())
            .project_ref(project.metadata.name.clone())
            .issues(vec![IssueSelector::Reference(issue.reference.clone())])
            .maybe_placement_policy(policy.placement_policy.clone())
            .auto_attach(ConvoyAutoAttach::Never)
            .build();
        self.daemon.admit_dispatch_reconciler_convoy(&project.metadata.namespace, &intent, policy.stance_preference, provenance).await
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReconcilePass {
    pub admitted: usize,
    pub blocked: usize,
    pub deferred_cap_full: bool,
    pub refused: bool,
}

pub(crate) struct DispatchReconciler {
    backend: ResourceBackend,
    namespace: String,
    issues: Arc<dyn DispatchIssueSource>,
    admission: Arc<dyn DispatchAdmission>,
}

impl DispatchReconciler {
    pub(crate) fn new(
        backend: ResourceBackend,
        namespace: impl Into<String>,
        issues: Arc<dyn DispatchIssueSource>,
        admission: Arc<dyn DispatchAdmission>,
    ) -> Self {
        Self { backend, namespace: namespace.into(), issues, admission }
    }

    pub(crate) async fn reconcile_once(&self) -> Result<ReconcilePass, String> {
        let projects = self.backend.clone().using::<Project>(&self.namespace).list().await.map_err(|error| error.to_string())?;
        let mut total = ReconcilePass::default();
        for project in projects.items {
            let outcome = self.reconcile_project(&project, Utc::now()).await?;
            total.admitted += outcome.admitted;
            total.blocked += outcome.blocked;
            total.deferred_cap_full |= outcome.deferred_cap_full;
            total.refused |= outcome.refused;
        }
        Ok(total)
    }

    async fn reconcile_project(&self, project: &ResourceObject<Project>, now: DateTime<Utc>) -> Result<ReconcilePass, String> {
        let Some(policy) = project.spec.dispatch_policy.as_ref().filter(|policy| policy.enabled) else {
            return Ok(ReconcilePass::default());
        };
        let convoys = self.backend.clone().using::<Convoy>(&project.metadata.namespace);
        let existing = convoys.list().await.map_err(|error| error.to_string())?.items;
        let belongs_to_project = |convoy: &&ResourceObject<Convoy>| convoy.spec.project_ref.as_deref() == Some(&project.metadata.name);
        let active_auto_admitted = existing
            .iter()
            .filter(belongs_to_project)
            .filter(|convoy| convoy.metadata.annotations.contains_key(DISPATCH_PROVENANCE_ANNOTATION))
            .filter(|convoy| !convoy.status.as_ref().is_some_and(|status| status.phase.is_terminal()))
            .count();
        if active_auto_admitted >= policy.max_concurrent {
            debug!(project = %project.metadata.name, active_auto_admitted, cap = policy.max_concurrent, "automatic dispatch cap is full");
            return Ok(ReconcilePass { deferred_cap_full: true, ..ReconcilePass::default() });
        }

        let dispatched = existing
            .iter()
            .filter(belongs_to_project)
            .flat_map(|convoy| convoy.spec.issues.iter().map(|issue| issue.reference.clone()))
            .collect::<HashSet<_>>();
        let mut ready = self.issues.ready_issues(project).await?;
        ready.retain(|issue| issue.state == IssueState::Open && issue.labels.iter().any(|label| label == READY_ISSUE_LABEL));
        ready.sort_by(|left, right| left.reference.cmp_id_desc(&right.reference).reverse());

        let mut outcome = ReconcilePass::default();
        let mut available = policy.max_concurrent - active_auto_admitted;
        for issue in ready {
            if available == 0 {
                outcome.deferred_cap_full = true;
                break;
            }
            if dispatched.contains(&issue.reference) {
                continue;
            }
            if project.status.as_ref().and_then(|status| status.dispatch_attention.as_ref()).is_some_and(|attention| {
                attention.issue == issue.reference && attention.issue_as_of == issue.as_of && attention.policy == *policy
            }) {
                continue;
            }

            let blockers = match blocked_by_references(&issue) {
                Ok(blockers) => blockers,
                Err(error) => {
                    warn!(project = %project.metadata.name, issue = %issue.reference.id, %error, "issue Blocked by section is unparseable; treating issue as blocked");
                    outcome.blocked += 1;
                    continue;
                }
            };
            let mut blocked = false;
            for blocker in blockers {
                match self.issues.fetch_issue(&blocker).await {
                    Ok(blocker) if blocker.state == IssueState::Closed => {}
                    Ok(_) => blocked = true,
                    Err(error) => {
                        warn!(project = %project.metadata.name, issue = %issue.reference.id, blocker = %blocker.id, %error, "blocker could not be observed; treating issue as blocked");
                        blocked = true;
                    }
                }
            }
            if blocked {
                outcome.blocked += 1;
                continue;
            }

            let provenance = format!("dispatch-reconciler, issue #{} ready+unblocked at {}", issue.reference.id, now.to_rfc3339());
            match self.admission.admit(project, &issue, policy, provenance).await {
                Ok(convoy) => {
                    info!(project = %project.metadata.name, issue = %issue.reference.id, %convoy, "automatically admitted convoy");
                    outcome.admitted += 1;
                    available -= 1;
                    self.clear_attention(project).await?;
                }
                Err(reason) => {
                    warn!(project = %project.metadata.name, issue = %issue.reference.id, %reason, "automatic convoy admission refused; project needs attention");
                    let attention = DispatchAttention {
                        issue: issue.reference.clone(),
                        issue_as_of: issue.as_of,
                        policy: policy.clone(),
                        reason,
                        observed_at: now,
                    };
                    apply_status_patch(
                        &self.backend.clone().using::<Project>(&project.metadata.namespace),
                        &project.metadata.name,
                        &ProjectStatusPatch::SetDispatchAttention(attention),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                    outcome.refused = true;
                    break;
                }
            }
        }
        Ok(outcome)
    }

    async fn clear_attention(&self, project: &ResourceObject<Project>) -> Result<(), String> {
        if project.status.as_ref().and_then(|status| status.dispatch_attention.as_ref()).is_none() {
            return Ok(());
        }
        apply_status_patch(
            &self.backend.clone().using::<Project>(&project.metadata.namespace),
            &project.metadata.name,
            &ProjectStatusPatch::ClearDispatchAttention,
        )
        .await
        .map_err(|error| error.to_string())?;
        Ok(())
    }
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

    use flotilla_protocol::{AssociationKey, IssueSource};
    use flotilla_resources::{
        ConvoyIssue, ConvoySpec, InputMeta, InputValue, IssueSnapshot, ProjectSpec, RepositoryKey, ResourceBackend, Stance,
    };

    use super::*;

    const NAMESPACE: &str = "flotilla";

    struct FakeIssues {
        ready: Mutex<Vec<Issue>>,
        by_ref: Mutex<HashMap<IssueRef, Issue>>,
        ready_calls: Mutex<usize>,
    }

    #[async_trait]
    impl DispatchIssueSource for FakeIssues {
        async fn ready_issues(&self, _project: &ResourceObject<Project>) -> Result<Vec<Issue>, String> {
            *self.ready_calls.lock().expect("ready calls lock") += 1;
            Ok(self.ready.lock().expect("ready lock").clone())
        }

        async fn fetch_issue(&self, reference: &IssueRef) -> Result<Issue, String> {
            self.by_ref.lock().expect("issues lock").get(reference).cloned().ok_or_else(|| "missing issue".to_string())
        }
    }

    struct InMemoryAdmission {
        backend: ResourceBackend,
        refusal: Mutex<Option<String>>,
    }

    #[async_trait]
    impl DispatchAdmission for InMemoryAdmission {
        async fn admit(
            &self,
            project: &ResourceObject<Project>,
            issue: &Issue,
            policy: &DispatchPolicy,
            provenance: String,
        ) -> Result<String, String> {
            if let Some(error) = self.refusal.lock().expect("refusal lock").clone() {
                return Err(error);
            }
            let name = format!("issue-{}", issue.reference.id);
            self.backend
                .clone()
                .using::<Convoy>(&project.metadata.namespace)
                .create(
                    &InputMeta::builder()
                        .name(name.clone())
                        .annotations(std::collections::BTreeMap::from([(DISPATCH_PROVENANCE_ANNOTATION.to_string(), provenance)]))
                        .build(),
                    &ConvoySpec {
                        workflow_ref: project.spec.default_workflow_ref.clone(),
                        dispatching_principal_ref: flotilla_protocol::PrincipalRef {
                            namespace: project.metadata.namespace.clone(),
                            name: "dispatch-reconciler".to_string(),
                        },
                        inputs: std::collections::BTreeMap::<String, InputValue>::new(),
                        placement_policy: policy.placement_policy.clone(),
                        repositories: Vec::new(),
                        r#ref: Some(name.clone()),
                        project_ref: Some(project.metadata.name.clone()),
                        adopted_checkout_refs: Default::default(),
                        issues: vec![ConvoyIssue {
                            reference: issue.reference.clone(),
                            repository_ref: None,
                            snapshot: IssueSnapshot {
                                title: issue.title.clone(),
                                body: issue.body.clone(),
                                state: issue.state,
                                labels: issue.labels.clone(),
                                as_of: issue.as_of,
                            },
                        }],
                        change_request: None,
                        instruction: None,
                    },
                )
                .await
                .map_err(|error| error.to_string())?;
            Ok(name)
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
            association_keys: Vec::<AssociationKey>::new(),
            provider_name: "fake".to_string(),
            provider_display_name: "Fake".to_string(),
        }
    }

    async fn harness(
        ready: Vec<Issue>,
        blockers: Vec<Issue>,
        policy: DispatchPolicy,
    ) -> (ResourceBackend, Arc<FakeIssues>, Arc<InMemoryAdmission>, DispatchReconciler) {
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
        });
        let admission = Arc::new(InMemoryAdmission { backend: backend.clone(), refusal: Mutex::new(None) });
        let reconciler = DispatchReconciler::new(
            backend.clone(),
            NAMESPACE,
            Arc::clone(&issues) as Arc<dyn DispatchIssueSource>,
            Arc::clone(&admission) as Arc<dyn DispatchAdmission>,
        );
        (backend, issues, admission, reconciler)
    }

    fn policy(cap: usize) -> DispatchPolicy {
        DispatchPolicy::builder().max_concurrent(cap).stance_preference(Stance::Contained).build()
    }

    #[tokio::test]
    async fn ready_unblocked_issue_is_admitted_with_provenance_while_ineligible_issues_are_not() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let unlabeled = issue("1", &[], None, IssueState::Open);
        let blocked = issue("3", &[READY_ISSUE_LABEL], Some("## Blocked by\n\n#9"), IssueState::Open);
        let blocker = issue("9", &[], None, IssueState::Open);
        let (backend, _, _, reconciler) = harness(vec![unlabeled, blocked, ready.clone()], vec![blocker], policy(3)).await;

        let outcome = reconciler.reconcile_once().await.expect("reconcile");

        assert_eq!(outcome.admitted, 1);
        assert_eq!(outcome.blocked, 1);
        let convoys = backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys");
        assert_eq!(convoys.items.len(), 1);
        assert_eq!(convoys.items[0].spec.issues[0].reference, ready.reference);
        assert!(convoys.items[0]
            .metadata
            .annotations
            .get(DISPATCH_PROVENANCE_ANNOTATION)
            .expect("provenance")
            .starts_with("dispatch-reconciler, issue #2 ready+unblocked at "));
    }

    #[tokio::test]
    async fn closing_a_blocker_makes_the_dependent_dispatchable_on_the_next_pass() {
        let dependent = issue("2", &[READY_ISSUE_LABEL], Some("## Blocked by\n#9"), IssueState::Open);
        let blocker = issue("9", &[], None, IssueState::Open);
        let (backend, issues, _, reconciler) = harness(vec![dependent], vec![blocker], policy(3)).await;
        assert_eq!(reconciler.reconcile_once().await.expect("blocked pass").admitted, 0);

        issues
            .by_ref
            .lock()
            .expect("issues lock")
            .insert(IssueRef { source: source(), id: "9".to_string() }, issue("9", &[], None, IssueState::Closed));

        assert_eq!(reconciler.reconcile_once().await.expect("unblocked pass").admitted, 1);
        assert_eq!(backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys").items.len(), 1);
    }

    #[tokio::test]
    async fn full_cap_defers_without_querying_the_issue_source() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let (backend, issues, admission, reconciler) = harness(vec![ready], vec![], policy(1)).await;
        let project = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project");
        admission
            .admit(
                &project,
                &issue("1", &[READY_ISSUE_LABEL], None, IssueState::Open),
                project.spec.dispatch_policy.as_ref().expect("policy"),
                "dispatch-reconciler".to_string(),
            )
            .await
            .expect("existing convoy");

        let outcome = reconciler.reconcile_once().await.expect("reconcile");

        assert!(outcome.deferred_cap_full);
        assert_eq!(*issues.ready_calls.lock().expect("ready calls lock"), 0);
        assert_eq!(backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys").items.len(), 1);
    }

    #[tokio::test]
    async fn a_limited_pass_admits_the_oldest_ready_issue_first() {
        let newer = issue("20", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let older = issue("10", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let (backend, _, _, reconciler) = harness(vec![newer, older.clone()], vec![], policy(1)).await;

        let outcome = reconciler.reconcile_once().await.expect("reconcile");

        assert_eq!(outcome.admitted, 1);
        let convoys = backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys");
        assert_eq!(convoys.items[0].spec.issues[0].reference, older.reference);
    }

    #[tokio::test]
    async fn disabled_policy_is_an_immediate_kill_switch() {
        let mut disabled = policy(3);
        disabled.enabled = false;
        let (backend, issues, _, reconciler) =
            harness(vec![issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open)], vec![], disabled).await;

        assert_eq!(reconciler.reconcile_once().await.expect("reconcile"), ReconcilePass::default());
        assert_eq!(*issues.ready_calls.lock().expect("ready calls lock"), 0);
        assert!(backend.using::<Convoy>(NAMESPACE).list().await.expect("convoys").items.is_empty());
    }

    #[tokio::test]
    async fn refusal_sets_attention_and_identical_snapshot_is_not_retried() {
        let ready = issue("2", &[READY_ISSUE_LABEL], None, IssueState::Open);
        let (backend, _, admission, reconciler) = harness(vec![ready.clone()], vec![], policy(3)).await;
        *admission.refusal.lock().expect("refusal lock") = Some("placement unavailable".to_string());

        assert!(reconciler.reconcile_once().await.expect("first pass").refused);
        *admission.refusal.lock().expect("refusal lock") = None;
        assert_eq!(reconciler.reconcile_once().await.expect("second pass").admitted, 0);

        let project = backend.using::<Project>(NAMESPACE).get("widgets").await.expect("project");
        let attention = project.status.expect("status").dispatch_attention.expect("attention");
        assert_eq!(attention.issue, ready.reference);
        assert_eq!(attention.reason, "placement unavailable");
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
