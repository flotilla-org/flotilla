//! Awareness-band projection over curated query rows.
//!
//! This module is deliberately pure: callers inject the row windows they
//! already hold and choose the grouping parameter at query time.

use std::collections::BTreeMap;

use chrono::Utc;
use flotilla_protocol::{
    AwarenessCounts, AwarenessEntry, AwarenessGrouping, AwarenessKind, AwarenessLimit, AwarenessNode, AwarenessPhase, AwarenessState,
    CheckoutRow, ConvoyPhase, ConvoyRow, IndependentRow, IssueRow, QueryScope, ResourceRef, ResultSetState, WorkPhase,
};

#[derive(Debug, Clone, Default)]
pub struct AwarenessInput {
    pub scope: Option<QueryScope>,
    pub grouping: AwarenessGrouping,
    pub limit: AwarenessLimit,
    pub convoys: Vec<ConvoyRow>,
    pub issues: Vec<ScopedIssueRow>,
    pub checkouts: Vec<CheckoutRow>,
    pub independents: Vec<IndependentRow>,
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
        let kind = match input.grouping {
            AwarenessGrouping::Project => AwarenessKind::Project,
            AwarenessGrouping::Convoy => AwarenessKind::Convoy,
        };
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, kind));
        group.refs.push(convoy.resource.clone());
        group.add_entry(
            AwarenessEntry::builder()
                .id(format!("convoy/{}/{}", convoy.resource.namespace, convoy.name))
                .kind(AwarenessKind::Convoy)
                .label(convoy_label(convoy))
                .state(convoy_state(convoy.phase, convoy.initializing))
                .phase(AwarenessPhase::Convoy(convoy.phase))
                .as_of(as_of)
                .refs(vec![convoy.resource.clone()])
                .issue_refs(convoy.issues.iter().map(|issue| issue.reference.clone()).collect())
                .build(),
        );
        for vessel in &convoy.vessels {
            group.add_entry(
                AwarenessEntry::builder()
                    .id(format!("vessel/{}/{}/{}", convoy.resource.namespace, convoy.name, vessel.name))
                    .kind(AwarenessKind::Vessel)
                    .label(vessel.name.clone())
                    .state(work_state(vessel.phase))
                    .phase(AwarenessPhase::Work(vessel.phase))
                    .as_of(as_of)
                    .refs(vec![vessel.resource.clone()])
                    .build(),
            );
        }
    }

    for issue in &input.issues {
        let row = &issue.row;
        let key =
            issue.scope.as_ref().or(input.scope.as_ref()).map(group_key_for_scope).unwrap_or_else(|| {
                GroupKey::new(format!("issue-source/{}", row.reference.source.scope), row.reference.source.scope.clone())
            });
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, AwarenessKind::Project));
        group.add_entry(
            AwarenessEntry::builder()
                .id(format!("issue/{}/{}", row.reference.source.scope, row.reference.id))
                .kind(AwarenessKind::Issue)
                .label(format!("#{} {}", row.reference.id, row.issue.title))
                .state(AwarenessState::Waiting)
                .phase(AwarenessPhase::Issue(row.issue.state))
                .as_of(row.issue.as_of)
                .issue_refs(vec![row.reference.clone()])
                .build(),
        );
    }

    for checkout in &input.checkouts {
        let key = input
            .scope
            .as_ref()
            .map(group_key_for_scope)
            .unwrap_or_else(|| GroupKey::new(format!("repo/{}", checkout.repo), checkout.repo.to_string()));
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, AwarenessKind::Project));
        group.add_entry(
            AwarenessEntry::builder()
                .id(format!("checkout/{}/{}", checkout.host, checkout.path))
                .kind(AwarenessKind::Checkout)
                .label(format!("{} · {}", checkout.branch, checkout.path))
                .state(AwarenessState::Active)
                .as_of(as_of)
                .refs(vec![checkout.resource.clone()])
                .build(),
        );
    }

    for independent in &input.independents {
        let key = input
            .scope
            .as_ref()
            .map(group_key_for_scope)
            .or_else(|| independent.repository_key.as_ref().map(|repo| GroupKey::new(format!("repo/{repo}"), repo.to_string())))
            .unwrap_or_else(|| GroupKey::new("unparented", "Unparented"));
        let group = groups.entry(key.id.clone()).or_insert_with(|| Group::new(key, AwarenessKind::Project));
        group.add_entry(
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
                .build(),
        );
    }

    let mut nodes = groups.into_values().collect::<Vec<_>>();
    nodes.sort_by(|left, right| group_rank(left).cmp(&group_rank(right)).then_with(|| left.label.cmp(&right.label)));
    nodes.truncate(input.limit.groups);
    let rows = nodes
        .into_iter()
        .map(|mut group| {
            group.entries.sort_by(|left, right| entry_rank(left).cmp(&entry_rank(right)).then_with(|| left.label.cmp(&right.label)));
            group.entries.truncate(input.limit.entries);
            AwarenessNode::builder()
                .id(group.id)
                .kind(group.kind)
                .label(group.label)
                .maybe_scope(group.scope)
                .state(group.state)
                .as_of(as_of)
                .counts(group.counts)
                .refs(group.refs)
                .entries(group.entries)
                .build()
        })
        .collect();
    (rows, input.state)
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
    use flotilla_protocol::{HostName, Issue, IssueRef, IssueSource, IssueState, RepositoryKey, ResourceRef, SessionPhase};

    use super::*;

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
                .path("/work/flotilla")
                .branch("feat/awareness-band")
                .host(HostName::new("local"))
                .authority(flotilla_protocol::LifecycleAuthority::Observed)
                .build()],
            independents: vec![IndependentRow::builder()
                .resource(ResourceRef::new("flotilla.work/v1", "TerminalSession", "flotilla", "scratch"))
                .name("scratch")
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
}
