//! Awareness-band projection over curated query rows.
//!
//! This module is deliberately pure: callers inject the row windows they
//! already hold and choose the grouping parameter at query time.

use std::collections::{BTreeMap, HashMap};

use chrono::Utc;
use flotilla_protocol::{
    AwarenessCounts, AwarenessEntry, AwarenessFamily, AwarenessFamilySummary, AwarenessGrouping, AwarenessKind, AwarenessLimit,
    AwarenessNode, AwarenessPhase, AwarenessState, CheckoutRow, ConvoyPhase, ConvoyRow, IndependentRow, IssueRow, QueryScope, ResourceRef,
    ResultSetState, Salience, WorkPhase, UNKNOWN_REPOSITORY_LABEL,
};
use flotilla_resources::{api_version, Project, Resource};

use crate::salience::{evaluate_entry, SalienceFacts};

const REPO_FACT_ANNOTATION: &str = "vcs.repo";

#[derive(Debug, Clone, Default)]
pub struct AwarenessInput {
    pub scope: Option<QueryScope>,
    pub grouping: AwarenessGrouping,
    pub limit: AwarenessLimit,
    pub convoys: Vec<ConvoyRow>,
    pub issues: Vec<ScopedIssueRow>,
    pub checkouts: Vec<CheckoutRow>,
    pub independents: Vec<IndependentRow>,
    pub salience: SalienceFacts,
    pub state: ResultSetState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedIssueRow {
    pub scope: Option<QueryScope>,
    pub row: IssueRow,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Group {
    id: String,
    label: String,
    scope: Option<QueryScope>,
    kind: AwarenessKind,
    refs: Vec<ResourceRef>,
    entries: Vec<AwarenessEntry>,
    counts: AwarenessCounts,
    state: AwarenessState,
}

pub fn project_awareness(input: AwarenessInput) -> (Vec<AwarenessNode>, ResultSetState) {
    let as_of = input.state.demand.as_ref().map_or_else(Utc::now, |metadata| metadata.as_of);
    let mut groups = BTreeMap::<String, Group>::new();

    for convoy in &input.convoys {
        let key = input.scope.as_ref().map(group_key_for_scope).unwrap_or_else(|| group_key_for_convoy(input.grouping, convoy));
        let kind = group_kind_for_query(input.scope.as_ref(), input.grouping);
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, kind));
        group.refs.push(convoy.resource.clone());
        let project_ancestors = convoy
            .project_ref
            .as_deref()
            .map(|project| project_resource_ref(&convoy.resource.namespace, project))
            .into_iter()
            .collect::<Vec<_>>();
        group.add_entry(enrich_salience(
            AwarenessEntry::builder()
                .id(format!("convoy/{}/{}", convoy.resource.namespace, convoy.name))
                .kind(AwarenessKind::Convoy)
                .label(convoy_label(convoy))
                .state(convoy_state(convoy.phase, convoy.initializing))
                .phase(AwarenessPhase::Convoy(convoy.phase))
                .as_of(as_of)
                .refs(vec![convoy.resource.clone()])
                .issue_refs(convoy.issues.iter().map(|issue| issue.reference.clone()).collect())
                .annotations(repo_fact_annotations(convoy.repo.as_ref()))
                .build(),
            &input.salience,
            &project_ancestors,
        ));
        for vessel in &convoy.vessels {
            let mut refs = vec![vessel.resource.clone()];
            refs.extend(vessel.vessel_resource.clone());
            let mut ancestors = vec![convoy.resource.clone()];
            ancestors.extend(project_ancestors.clone());
            group.add_entry(enrich_salience(
                AwarenessEntry::builder()
                    .id(format!("vessel/{}/{}/{}", convoy.resource.namespace, convoy.name, vessel.name))
                    .kind(AwarenessKind::Vessel)
                    .label(vessel.name.clone())
                    .state(work_state(vessel.phase))
                    .phase(AwarenessPhase::Work(vessel.phase))
                    .as_of(as_of)
                    .refs(refs)
                    .annotations(repo_fact_annotations(convoy.repo.as_ref()))
                    .build(),
                &input.salience,
                &ancestors,
            ));
        }
    }

    for issue in &input.issues {
        let row = &issue.row;
        let key =
            issue.scope.as_ref().or(input.scope.as_ref()).map(group_key_for_scope).unwrap_or_else(|| {
                GroupKey::new(format!("issue-source/{}", row.reference.source.scope), row.reference.source.scope.clone())
            });
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, AwarenessKind::Project));
        let project_ancestors = issue
            .scope
            .as_ref()
            .or(input.scope.as_ref())
            .map(|scope| project_resource_ref(&scope.namespace, &scope.name))
            .into_iter()
            .collect::<Vec<_>>();
        group.add_entry(enrich_salience(
            AwarenessEntry::builder()
                .id(format!("issue/{}/{}", row.reference.source.scope, row.reference.id))
                .kind(AwarenessKind::Issue)
                .label(format!("#{} {}", row.reference.id, row.issue.title))
                .state(AwarenessState::Waiting)
                .phase(AwarenessPhase::Issue(row.issue.state))
                .as_of(row.issue.as_of)
                .issue_refs(vec![row.reference.clone()])
                .build(),
            &input.salience,
            &project_ancestors,
        ));
    }

    for checkout in &input.checkouts {
        let key = input
            .scope
            .as_ref()
            .map(group_key_for_scope)
            .unwrap_or_else(|| GroupKey::new(format!("repo/{}", checkout.repo), checkout.repo_label.clone()));
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, AwarenessKind::Project));
        let project_ancestors =
            input.scope.as_ref().map(|scope| project_resource_ref(&scope.namespace, &scope.name)).into_iter().collect::<Vec<_>>();
        group.add_entry(enrich_salience(
            AwarenessEntry::builder()
                .id(format!("checkout/{}/{}", checkout.host, checkout.path))
                .kind(AwarenessKind::Checkout)
                .label(format!("{} · {}", checkout.branch, checkout.path))
                .state(AwarenessState::Active)
                .as_of(as_of)
                .refs(vec![checkout.resource.clone()])
                .annotations(repo_fact_annotations(checkout.repo_fact.as_ref()))
                .build(),
            &input.salience,
            &project_ancestors,
        ));
    }

    for independent in &input.independents {
        let key = input
            .scope
            .as_ref()
            .map(group_key_for_scope)
            .or_else(|| {
                independent.repository_key.as_ref().map(|repo| {
                    let label = independent.repo.as_ref().map_or_else(|| UNKNOWN_REPOSITORY_LABEL.to_string(), |label| label.0.clone());
                    GroupKey::new(format!("repo/{repo}"), label)
                })
            })
            .unwrap_or_else(|| GroupKey::new("unparented", "Unparented"));
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, AwarenessKind::Project));
        let project_ancestors =
            input.scope.as_ref().map(|scope| project_resource_ref(&scope.namespace, &scope.name)).into_iter().collect::<Vec<_>>();
        group.add_entry(enrich_salience(
            AwarenessEntry::builder()
                .id(format!("independent/{}/{}", independent.resource.namespace, independent.name))
                .kind(AwarenessKind::Independent)
                .label(independent.name.clone())
                .state(match independent.phase {
                    flotilla_protocol::SessionPhase::Starting => AwarenessState::Waiting,
                    flotilla_protocol::SessionPhase::Running => AwarenessState::Active,
                    flotilla_protocol::SessionPhase::Stopped => AwarenessState::Done,
                    flotilla_protocol::SessionPhase::Failed => AwarenessState::Failed,
                })
                .phase(AwarenessPhase::Session(independent.phase))
                .as_of(as_of)
                .refs(vec![independent.resource.clone()])
                .annotations(repo_fact_annotations(independent.repo_fact.as_ref()))
                .build(),
            &input.salience,
            &project_ancestors,
        ));
    }

    let mut nodes = groups.into_values().collect::<Vec<_>>();
    nodes.sort_by(|left, right| group_rank(left).cmp(&group_rank(right)).then_with(|| left.label.cmp(&right.label)));
    nodes.truncate(input.limit.groups);
    let rows = nodes
        .into_iter()
        .map(|mut group| {
            group.entries.sort_by(|left, right| entry_rank(left).cmp(&entry_rank(right)).then_with(|| left.label.cmp(&right.label)));
            let salience = group.entries.iter().map(|entry| entry.salience).max().unwrap_or(Salience::None);
            let node_as_of = group.entries.iter().map(|entry| entry.as_of).max().unwrap_or(as_of);
            let family_summaries = awareness_family_summaries(&group.entries);
            group.entries.truncate(input.limit.entries);
            AwarenessNode::builder()
                .id(group.id)
                .kind(group.kind)
                .label(group.label)
                .maybe_scope(group.scope)
                .state(group.state)
                .salience(salience)
                .as_of(node_as_of)
                .counts(group.counts)
                .refs(group.refs)
                .entries(group.entries)
                .family_summaries(family_summaries)
                .build()
        })
        .collect();
    (rows, input.state)
}

fn repo_fact_annotations(repo: Option<&flotilla_protocol::RepoKey>) -> HashMap<String, String> {
    repo.map(|repo| HashMap::from([(REPO_FACT_ANNOTATION.to_string(), repo.0.clone())])).unwrap_or_default()
}

fn awareness_family_summaries(entries: &[AwarenessEntry]) -> Vec<AwarenessFamilySummary> {
    [AwarenessFamily::Convoys, AwarenessFamily::Issues, AwarenessFamily::Checkouts, AwarenessFamily::Independents]
        .into_iter()
        .filter_map(|family| {
            let mut matching = entries.iter().filter(|entry| family_contains(family, entry.kind));
            let first = matching.next()?;
            let (salience, as_of) = matching
                .fold((first.salience, first.as_of), |(salience, as_of), entry| (salience.max(entry.salience), as_of.max(entry.as_of)));
            Some(AwarenessFamilySummary::builder().family(family).salience(salience).as_of(as_of).build())
        })
        .collect()
}

fn family_contains(family: AwarenessFamily, kind: AwarenessKind) -> bool {
    match family {
        AwarenessFamily::Convoys => matches!(kind, AwarenessKind::Convoy | AwarenessKind::Vessel),
        AwarenessFamily::Issues => kind == AwarenessKind::Issue,
        AwarenessFamily::Checkouts => kind == AwarenessKind::Checkout,
        AwarenessFamily::Independents => kind == AwarenessKind::Independent,
    }
}

fn enrich_salience(mut entry: AwarenessEntry, facts: &SalienceFacts, ancestors: &[ResourceRef]) -> AwarenessEntry {
    let mut coverage_targets = entry.refs.clone();
    coverage_targets.extend_from_slice(ancestors);
    let evaluation = evaluate_entry(&entry.refs, &coverage_targets, facts, entry.as_of);
    entry.salience = evaluation.salience;
    entry.as_of = evaluation.as_of;
    entry
}

fn project_resource_ref(default_namespace: &str, project_ref: &str) -> ResourceRef {
    let (namespace, name) = project_ref.split_once('/').unwrap_or((default_namespace, project_ref));
    ResourceRef::new(api_version(Project::API_PATHS), Project::API_PATHS.kind, namespace, name)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupKey {
    id: String,
    label: String,
    scope: Option<QueryScope>,
}

impl GroupKey {
    fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { id: id.into(), label: label.into(), scope: None }
    }

    fn scoped(scope: QueryScope) -> Self {
        Self { id: format!("project/{}/{}", scope.namespace, scope.name), label: scope.name.clone(), scope: Some(scope) }
    }
}

impl Group {
    fn new(key: GroupKey, kind: AwarenessKind) -> Self {
        Self {
            id: key.id,
            label: key.label,
            scope: key.scope,
            kind,
            refs: Vec::new(),
            entries: Vec::new(),
            counts: AwarenessCounts::default(),
            state: AwarenessState::Unknown,
        }
    }

    fn add_entry(&mut self, entry: AwarenessEntry) {
        self.counts.total += 1;
        match entry.kind {
            AwarenessKind::Issue => self.counts.issues += 1,
            AwarenessKind::Convoy => self.counts.convoys += 1,
            AwarenessKind::Vessel => self.counts.vessels += 1,
            AwarenessKind::Checkout => self.counts.checkouts += 1,
            AwarenessKind::Independent => self.counts.independents += 1,
            AwarenessKind::Fleet | AwarenessKind::Project => {}
        }
        match entry.state {
            AwarenessState::Waiting => self.counts.waiting += 1,
            AwarenessState::Active => self.counts.active += 1,
            AwarenessState::Done => self.counts.done += 1,
            AwarenessState::Failed => self.counts.failed += 1,
            AwarenessState::Unknown | AwarenessState::Pending | AwarenessState::Cancelled => {}
        }
        self.state = stronger_state(self.state, entry.state);
        self.entries.push(entry);
    }
}

fn group_key_for_scope(scope: &QueryScope) -> GroupKey {
    GroupKey::scoped(scope.clone())
}

fn group_kind_for_query(scope: Option<&QueryScope>, grouping: AwarenessGrouping) -> AwarenessKind {
    if scope.is_some() {
        return AwarenessKind::Project;
    }
    match grouping {
        AwarenessGrouping::Project => AwarenessKind::Project,
        AwarenessGrouping::Convoy => AwarenessKind::Convoy,
    }
}

fn group_key_for_convoy(grouping: AwarenessGrouping, convoy: &ConvoyRow) -> GroupKey {
    match grouping {
        AwarenessGrouping::Project => convoy.project_ref.as_deref().map(project_ref_key).unwrap_or_else(|| {
            convoy
                .repo
                .as_ref()
                .map_or_else(|| GroupKey::new("unparented", "Unparented"), |repo| GroupKey::new(format!("repo/{}", repo.0), repo.0.clone()))
        }),
        AwarenessGrouping::Convoy => GroupKey::new(format!("convoy/{}/{}", convoy.resource.namespace, convoy.name), convoy.name.clone()),
    }
}

fn project_ref_key(value: &str) -> GroupKey {
    if let Some((namespace, name)) = value.split_once('/') {
        return GroupKey::scoped(QueryScope::new(namespace, name));
    }
    GroupKey::new(format!("project/{value}"), value.to_string())
}

fn convoy_label(convoy: &ConvoyRow) -> String {
    convoy
        .change_request
        .as_ref()
        .map_or_else(|| convoy.name.clone(), |change_request| format!("{} · PR #{}", convoy.name, change_request.id))
}

fn convoy_state(phase: ConvoyPhase, initializing: bool) -> AwarenessState {
    if initializing {
        return AwarenessState::Waiting;
    }
    match phase {
        ConvoyPhase::Pending => AwarenessState::Pending,
        ConvoyPhase::Active => AwarenessState::Active,
        ConvoyPhase::Completed => AwarenessState::Done,
        ConvoyPhase::Failed => AwarenessState::Failed,
        ConvoyPhase::Cancelled | ConvoyPhase::Abandoned => AwarenessState::Cancelled,
    }
}

fn work_state(phase: WorkPhase) -> AwarenessState {
    match phase {
        WorkPhase::Pending => AwarenessState::Pending,
        WorkPhase::Ready => AwarenessState::Waiting,
        WorkPhase::Launching | WorkPhase::Running => AwarenessState::Active,
        WorkPhase::Complete => AwarenessState::Done,
        WorkPhase::Failed => AwarenessState::Failed,
        WorkPhase::Cancelled | WorkPhase::Abandoned => AwarenessState::Cancelled,
    }
}

fn stronger_state(left: AwarenessState, right: AwarenessState) -> AwarenessState {
    if state_rank(right) > state_rank(left) {
        right
    } else {
        left
    }
}

fn state_rank(state: AwarenessState) -> u8 {
    match state {
        AwarenessState::Failed => 5,
        AwarenessState::Waiting => 4,
        AwarenessState::Active => 3,
        AwarenessState::Pending => 2,
        AwarenessState::Unknown => 1,
        AwarenessState::Done | AwarenessState::Cancelled => 0,
    }
}

fn group_rank(group: &Group) -> (u8, std::cmp::Reverse<usize>, String) {
    (std::cmp::Reverse(state_rank(group.state)).0, std::cmp::Reverse(group.counts.total), group.label.clone())
}

fn entry_rank(entry: &AwarenessEntry) -> (u8, u8) {
    (std::cmp::Reverse(state_rank(entry.state)).0, match entry.kind {
        AwarenessKind::Issue => 0,
        AwarenessKind::Convoy => 1,
        AwarenessKind::Vessel => 2,
        AwarenessKind::Independent => 3,
        AwarenessKind::Checkout => 4,
        AwarenessKind::Fleet | AwarenessKind::Project => 5,
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use flotilla_protocol::{HostName, Issue, IssueRef, IssueSource, IssueState, RepositoryKey, ResourceRef, SessionPhase};
    use flotilla_resources::{DemandState, PrincipalRef, TerminalAttentionState};

    use super::*;
    use crate::salience::{AttentionFact, DemandFact, RegardFact};

    fn convoy(project_ref: Option<&str>, name: &str, phase: ConvoyPhase) -> ConvoyRow {
        ConvoyRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", name))
            .name(name.to_string())
            .workflow_ref("implement")
            .phase(phase)
            .maybe_project_ref(project_ref.map(str::to_owned))
            .build()
    }

    fn issue(id: &str, title: &str) -> IssueRow {
        let reference =
            IssueRef { source: IssueSource { service: "https://github.com".into(), scope: "flotilla-org/flotilla".into() }, id: id.into() };
        IssueRow {
            reference: reference.clone(),
            issue: Issue {
                reference,
                title: title.into(),
                body: None,
                state: IssueState::Open,
                labels: vec![],
                as_of: Utc::now(),
                observed_at: None,
                association_keys: vec![],
                provider_name: "github".into(),
                provider_display_name: "GitHub".into(),
            },
        }
    }

    #[test]
    fn project_grouping_joins_convoys_issues_checkouts_and_independents() {
        let scope = QueryScope::new("flotilla", "platform");
        let (nodes, _) = project_awareness(AwarenessInput {
            scope: Some(scope.clone()),
            grouping: AwarenessGrouping::Project,
            convoys: vec![convoy(Some("flotilla/platform"), "ship-it", ConvoyPhase::Active)],
            issues: vec![ScopedIssueRow { scope: Some(scope.clone()), row: issue("862", "awareness band") }],
            checkouts: vec![CheckoutRow::builder()
                .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "checkout"))
                .repo(RepositoryKey("repo-a".into()))
                .repo_label("github.com/flotilla-org/flotilla")
                .repo_fact(flotilla_protocol::RepoKey("flotilla-org/flotilla".into()))
                .path("/work/flotilla")
                .branch("feat/awareness-band")
                .host(HostName::new("local"))
                .authority(flotilla_protocol::LifecycleAuthority::Observed)
                .build()],
            independents: vec![IndependentRow::builder()
                .resource(ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", "scratch"))
                .name("scratch")
                .repo(flotilla_protocol::RepoKey("github.com/flotilla-org/flotilla".into()))
                .repo_fact(flotilla_protocol::RepoKey("flotilla-org/flotilla".into()))
                .repository_key(RepositoryKey("repo-a".into()))
                .host(HostName::new("local"))
                .phase(SessionPhase::Running)
                .build()],
            ..AwarenessInput::default()
        });

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].label, "platform");
        assert_eq!(nodes[0].counts.issues, 1);
        assert_eq!(nodes[0].counts.checkouts, 1);
        assert_eq!(nodes[0].counts.independents, 1);
        assert!(nodes[0].entries.iter().any(|entry| entry.kind == AwarenessKind::Convoy && entry.label == "ship-it"));
        for kind in [AwarenessKind::Checkout, AwarenessKind::Independent] {
            let entry = nodes[0].entries.iter().find(|entry| entry.kind == kind).expect("awareness entry");
            assert_eq!(
                entry.annotations.get(REPO_FACT_ANNOTATION).map(String::as_str),
                Some("flotilla-org/flotilla"),
                "repo grouping uses canonical fact value, not display label",
            );
        }
    }

    #[test]
    fn scoped_awareness_reports_project_kind_even_with_convoy_grouping() {
        let scope = QueryScope::new("flotilla", "platform");
        let (nodes, _) = project_awareness(AwarenessInput {
            scope: Some(scope.clone()),
            grouping: AwarenessGrouping::Convoy,
            convoys: vec![convoy(Some("flotilla/platform"), "ship-it", ConvoyPhase::Active)],
            ..AwarenessInput::default()
        });

        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].kind, AwarenessKind::Project);
        assert_eq!(nodes[0].scope.as_ref(), Some(&scope));
        assert_eq!(nodes[0].counts.convoys, 1);
    }

    #[test]
    fn grouping_parameter_changes_shape_without_changing_input_facts() {
        let input = AwarenessInput {
            grouping: AwarenessGrouping::Project,
            convoys: vec![
                convoy(Some("flotilla/platform"), "first", ConvoyPhase::Active),
                convoy(Some("flotilla/platform"), "second", ConvoyPhase::Pending),
            ],
            ..AwarenessInput::default()
        };
        let (project_nodes, _) = project_awareness(input.clone());
        let (convoy_nodes, _) = project_awareness(AwarenessInput { grouping: AwarenessGrouping::Convoy, ..input });

        assert_eq!(project_nodes.len(), 1);
        assert_eq!(convoy_nodes.len(), 2);
    }

    #[test]
    fn salience_is_joined_onto_entries_and_aggregated_to_their_node() {
        let base = Utc.with_ymd_and_hms(2026, 7, 22, 12, 0, 0).single().expect("timestamp");
        let demand_at = Utc.with_ymd_and_hms(2026, 7, 22, 12, 1, 0).single().expect("timestamp");
        let attention_at = Utc.with_ymd_and_hms(2026, 7, 22, 12, 2, 0).single().expect("timestamp");
        let mut convoy = convoy(Some("flotilla/platform"), "ship-it", ConvoyPhase::Active);
        let vessel = convoy.resource.subresource("vessels/implement");
        convoy.vessels.push(
            flotilla_protocol::VesselRow::builder()
                .resource(vessel.clone())
                .name("implement")
                .phase(WorkPhase::Running)
                .host(HostName::new("local"))
                .build(),
        );
        let principal = PrincipalRef::implicit_for_namespace("flotilla");

        let (nodes, _) = project_awareness(AwarenessInput {
            scope: Some(QueryScope::new("flotilla", "platform")),
            convoys: vec![convoy.clone()],
            checkouts: vec![CheckoutRow::builder()
                .resource(ResourceRef::new("flotilla.work/v1", "Checkout", "flotilla", "platform"))
                .repo(RepositoryKey("repo-a".into()))
                .repo_label("platform")
                .path("/work/platform")
                .branch("main")
                .host(HostName::new("local"))
                .authority(flotilla_protocol::LifecycleAuthority::Observed)
                .build()],
            salience: SalienceFacts {
                demands: vec![DemandFact {
                    target: vessel.clone(),
                    addressee: Some(principal.clone()),
                    state: DemandState::Raised,
                    as_of: demand_at,
                }],
                regards: vec![RegardFact { principal, target: project_resource_ref("flotilla", "flotilla/platform"), as_of: base }],
                attention: vec![AttentionFact {
                    target: vessel,
                    state: TerminalAttentionState::NeedsInput,
                    work_unsettled: true,
                    as_of: attention_at,
                }],
            },
            state: ResultSetState {
                demand: Some(flotilla_protocol::DemandBackedMetadata { as_of: base, has_more: false }),
                conditions: Vec::new(),
            },
            ..AwarenessInput::default()
        });

        let node = nodes.first().expect("project node");
        let vessel = node.entries.iter().find(|entry| entry.kind == AwarenessKind::Vessel).expect("vessel entry");
        assert_eq!(vessel.salience, Salience::Urgent);
        assert_eq!(vessel.as_of, attention_at);
        assert_eq!(node.salience, Salience::Urgent);
        assert_eq!(node.as_of, attention_at);
        let convoys = node.family_summary(AwarenessFamily::Convoys).expect("convoy family summary");
        assert_eq!(convoys.salience, Salience::Urgent);
        assert_eq!(convoys.as_of, attention_at);
        let checkouts = node.family_summary(AwarenessFamily::Checkouts).expect("checkout family summary");
        assert_eq!(checkouts.salience, Salience::Info);
        assert_eq!(checkouts.as_of, base);
    }
}
