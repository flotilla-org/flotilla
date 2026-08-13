//! Shared Aggregator state used for replay and fleet-replica export.
//!
//! [`QueryProjection`] maintains the unscoped Convoys family. Store-backed
//! families with Project views share the scoped projection in
//! `scoped_store`.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use flotilla_protocol::{
    issue_query::READY_ISSUE_LABEL,
    result_set::{
        AwarenessGrouping, AwarenessLimit, CheckoutRow, ConvoyPhase, ConvoyRow, IndependentRow, IssueRow, QueryId, QueryScope, ResultDelta,
        ResultSet, ResultSetState, Rows,
    },
    HostName, IssueRef, QueryCursor, RepositoryKey, ResourceRef,
};
use tokio::sync::{broadcast, watch, RwLock, RwLockWriteGuard};
use uuid::Uuid;

use crate::{
    awareness_projection::{project_awareness, AwarenessInput, ScopedIssueRow},
    query_registry::QueryRegistry,
    salience::SalienceFacts,
    scoped_store::{ScopedCheckoutProjection, ScopedIndependentProjection},
};

/// A typed row of some named query's result set.
pub trait QueryRow: Clone {
    fn resource(&self) -> &ResourceRef;
    fn into_rows(rows: Vec<Self>) -> Rows;
}

impl QueryRow for ConvoyRow {
    fn resource(&self) -> &ResourceRef {
        &self.resource
    }

    fn into_rows(rows: Vec<Self>) -> Rows {
        Rows::Convoys { scope: None, rows }
    }
}

/// Incrementally-maintained result set of one named query.
#[derive(Debug, Clone)]
pub struct QueryProjection<R> {
    pub local_rows: HashMap<ResourceRef, R>,
    pub replica_rows: HashMap<HostName, HashMap<ResourceRef, R>>,
    pub seq: u64,
}

impl<R> Default for QueryProjection<R> {
    fn default() -> Self {
        Self { local_rows: HashMap::new(), replica_rows: HashMap::new(), seq: 0 }
    }
}

impl<R: QueryRow> QueryProjection<R> {
    /// Full fleet-merged result set: local rows ∪ every replica's rows.
    pub fn result_set(&self) -> ResultSet {
        let rows = self.local_rows.values().chain(self.replica_rows.values().flat_map(|rows| rows.values())).cloned().collect();
        self.to_result_set(rows)
    }

    /// Local rows only — what this host contributes to federated query union.
    pub fn local_result_set(&self) -> ResultSet {
        let rows = self.local_rows.values().cloned().collect();
        self.to_result_set(rows)
    }

    fn to_result_set(&self, mut rows: Vec<R>) -> ResultSet {
        rows.sort_by(|left, right| {
            let left = left.resource();
            let right = right.resource();
            (&left.namespace, &left.name, &left.host).cmp(&(&right.namespace, &right.name, &right.host))
        });
        ResultSet { seq: self.seq, rows: R::into_rows(rows), state: Default::default() }
    }
}

impl<R: QueryRow + PartialEq> QueryProjection<R> {
    /// Replace every replica host's contribution after a fleet refresh and
    /// return the changed and removed rows when the result set advanced.
    pub fn replace_replica_rows(&mut self, replacements: HashMap<HostName, HashMap<ResourceRef, R>>) -> Option<(Vec<R>, Vec<ResourceRef>)> {
        let previous = std::mem::take(&mut self.replica_rows);
        let changed = replacements
            .iter()
            .flat_map(|(host, rows)| {
                let prior = previous.get(host);
                rows.iter()
                    .filter(move |(reference, row)| prior.and_then(|prior| prior.get(*reference)) != Some(*row))
                    .map(|(_, row)| row.clone())
            })
            .collect::<Vec<_>>();
        let removed = previous
            .iter()
            .flat_map(|(host, rows)| {
                let replacement = replacements.get(host);
                rows.keys().filter(move |reference| replacement.is_none_or(|replacement| !replacement.contains_key(*reference))).cloned()
            })
            .collect::<Vec<_>>();
        self.replica_rows = replacements;
        if changed.is_empty() && removed.is_empty() {
            return None;
        }
        self.seq = self.seq.saturating_add(1);
        Some((changed, removed))
    }
}

#[derive(Debug, Default)]
struct SalienceProjection {
    facts: SalienceFacts,
    revision: u64,
}

#[derive(Debug, Default)]
struct ProjectCatalogProjection {
    projects: HashMap<QueryScope, Vec<RepositoryKey>>,
    revision: u64,
}

#[derive(Debug, Default, Clone, bon::Builder)]
pub struct AggregatorProjectionState {
    convoys: Arc<RwLock<QueryProjection<ConvoyRow>>>,
    #[builder(skip)]
    independents: Arc<RwLock<ScopedIndependentProjection>>,
    #[builder(skip)]
    checkouts: Arc<RwLock<ScopedCheckoutProjection>>,
    #[builder(skip)]
    salience: Arc<RwLock<SalienceProjection>>,
    #[builder(skip)]
    project_catalog: Arc<RwLock<ProjectCatalogProjection>>,
    /// Last projected snapshot observed by subscription replay or live
    /// delivery. Scoped Convoys use full snapshots, so retaining their last
    /// rows lets rebuilds suppress notifications for unrelated projects.
    #[builder(skip)]
    scoped_convoy_snapshots: Arc<Mutex<HashMap<QueryId, ResultSet>>>,
    /// Subscriber ownership and demand-backed materializations belong to the
    /// Aggregator state, shared with the daemon's subscription transport.
    demand_backed: QueryRegistry,
}

impl AggregatorProjectionState {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn write(&self) -> RwLockWriteGuard<'_, QueryProjection<ConvoyRow>> {
        self.convoys.write().await
    }

    pub async fn result_set(&self) -> ResultSet {
        self.convoys.read().await.result_set()
    }

    pub async fn convoy_result_set(&self, scope: &Option<QueryScope>) -> ResultSet {
        let set = self.result_set().await;
        let Rows::Convoys { rows, .. } = set.rows else { unreachable!("convoy projection must produce convoy rows") };
        let rows = rows.into_iter().filter(|row| scope.as_ref().is_none_or(|scope| convoy_matches_scope(row, scope))).collect();
        ResultSet { seq: set.seq, rows: Rows::Convoys { scope: scope.clone(), rows }, state: set.state }
    }

    pub async fn seq(&self) -> u64 {
        self.convoys.read().await.seq
    }

    pub async fn local_result_set(&self) -> ResultSet {
        self.convoys.read().await.local_result_set()
    }

    pub async fn independents_result_set(&self, scope: &Option<QueryScope>) -> ResultSet {
        self.independents.write().await.result_set(scope)
    }

    /// This host's local store-backed result sets. Demand-backed reference
    /// data is never included in fleet replica snapshots.
    pub async fn local_result_sets(&self) -> Vec<ResultSet> {
        let mut sets = vec![self.local_result_set().await];
        sets.extend(self.independents.read().await.local_result_sets());
        sets.extend(self.checkouts.read().await.local_result_sets());
        sets
    }

    pub async fn replace_store_catalog(
        &self,
        repositories: HashMap<RepositoryKey, String>,
        projects: HashMap<QueryScope, Vec<RepositoryKey>>,
    ) -> Vec<ResultDelta> {
        let project_scopes = {
            let mut catalog = self.project_catalog.write().await;
            if catalog.projects != projects {
                catalog.projects = projects.clone();
                catalog.revision = catalog.revision.saturating_add(1);
            }
            sorted_project_scopes(&catalog.projects)
        };
        let mut deltas = self.independents.write().await.replace_catalog(repositories.clone(), projects.clone());
        deltas.extend(self.checkouts.write().await.replace_catalog(repositories, projects));
        self.demand_backed.replace_fleet_awareness_scopes(&project_scopes);
        deltas
    }

    pub async fn replace_local_independent_rows(&self, rows: Vec<IndependentRow>) -> Vec<ResultDelta> {
        self.independents.write().await.replace_local_rows(rows)
    }

    pub async fn replace_independent_replica_rows(&self, replicas: HashMap<HostName, Vec<IndependentRow>>) -> Vec<ResultDelta> {
        self.independents.write().await.replace_replica_rows(replicas)
    }

    pub async fn replace_local_checkout_rows(&self, rows: Vec<CheckoutRow>) -> Vec<ResultDelta> {
        self.checkouts.write().await.replace_local_rows(rows)
    }

    pub async fn replace_checkout_replica_rows(&self, replicas: HashMap<HostName, Vec<CheckoutRow>>) -> Vec<ResultDelta> {
        self.checkouts.write().await.replace_replica_rows(replicas)
    }

    /// Replace the mesh-side facts used by the central salience join. Returns
    /// whether any projected salience may have changed.
    pub async fn replace_salience_facts(&self, facts: SalienceFacts) -> bool {
        let mut current = self.salience.write().await;
        if current.facts == facts {
            return false;
        }
        current.facts = facts;
        current.revision = current.revision.saturating_add(1);
        true
    }

    /// Replace one subscriber's complete demand and return queries whose
    /// materialization lifetime was newly created.
    pub fn replace_subscriber(&self, subscriber: Uuid, cursors: &[QueryCursor]) -> HashSet<QueryId> {
        let newly_materialized = self.demand_backed.replace(subscriber, cursors);
        self.retain_subscribed_scoped_convoy_snapshots();
        newly_materialized
    }

    pub fn remove_subscriber(&self, subscriber: Uuid) {
        self.demand_backed.remove(subscriber);
        self.retain_subscribed_scoped_convoy_snapshots();
    }

    fn retain_subscribed_scoped_convoy_snapshots(&self) {
        let subscribed = self.demand_backed.subscribed_queries();
        self.scoped_convoy_snapshots.lock().expect("scoped convoy snapshot lock poisoned").retain(|query, _| subscribed.contains(query));
    }

    pub fn subscribed_queries(&self) -> HashSet<QueryId> {
        self.demand_backed.subscribed_queries()
    }

    /// Observe the complete set of live demand-backed query identities.
    /// The Aggregator uses this to start and stop source materializers.
    pub fn subscribe_demand(&self) -> watch::Receiver<HashMap<QueryId, u64>> {
        self.demand_backed.subscribe_demand()
    }

    pub fn subscribe_fetch_more(&self) -> broadcast::Receiver<(QueryId, u64)> {
        self.demand_backed.subscribe_fetch_more()
    }

    pub fn request_fetch_more(&self, query: &QueryId) -> Result<(), String> {
        self.demand_backed.request_fetch_more(query)
    }

    /// Replace the fetched window for a live issue materialization. Results
    /// racing with teardown are ignored by the registry.
    pub fn replace_issues(&self, query: &QueryId, generation: u64, rows: Vec<IssueRow>, state: ResultSetState) -> Option<ResultSet> {
        self.demand_backed.replace_issues(query, generation, rows, state)
    }

    pub fn apply_issue_changes(
        &self,
        query: &QueryId,
        generation: u64,
        changed: Vec<IssueRow>,
        removed: Vec<IssueRef>,
        state: ResultSetState,
    ) -> Option<ResultDelta> {
        self.demand_backed.apply_issue_changes(query, generation, changed, removed, state)
    }

    pub async fn represented_issue_refs(&self) -> HashSet<IssueRef> {
        let convoys = self.convoys.read().await;
        convoys
            .local_rows
            .values()
            .chain(convoys.replica_rows.values().flat_map(|rows| rows.values()))
            .filter(|convoy| convoy_phase_represents_issues(convoy.phase))
            .flat_map(|convoy| convoy.issues.iter().map(|issue| issue.reference.clone()))
            .collect()
    }

    pub fn suppress_issues(&self, represented: &HashSet<IssueRef>) -> Vec<ResultDelta> {
        self.demand_backed.suppress_issues(represented)
    }

    /// The current fleet-merged result set for one named query.
    pub async fn result_set_for(&self, query: &QueryId) -> Option<ResultSet> {
        let result_set = match query {
            QueryId::Convoys { scope } => Some(self.convoy_result_set(scope).await),
            QueryId::Independents { scope } => Some(self.independents_result_set(scope).await),
            QueryId::Issues { .. } => self.demand_backed.result_set(query),
            QueryId::Checkouts { scope } => Some(self.checkouts.write().await.result_set(scope)),
            QueryId::Awareness { scope, grouping, limit } => Some(self.awareness_result_set(scope, *grouping, *limit).await),
        };
        if let Some(result_set) = result_set.as_ref().filter(|result_set| matches!(result_set.query(), QueryId::Convoys { scope: Some(_) }))
        {
            self.remember_scoped_convoy_snapshot(result_set);
        }
        result_set
    }

    /// Return only live scoped Convoys snapshots whose projected rows changed
    /// since subscription replay or the previous live delivery.
    pub async fn changed_scoped_convoy_result_sets(&self) -> Vec<ResultSet> {
        let queries =
            self.subscribed_queries().into_iter().filter(|query| matches!(query, QueryId::Convoys { scope: Some(_) })).collect::<Vec<_>>();
        let mut changed = Vec::new();
        for query in queries {
            let QueryId::Convoys { scope } = &query else { unreachable!("queries were filtered to scoped Convoys") };
            let result_set = self.convoy_result_set(scope).await;
            let mut snapshots = self.scoped_convoy_snapshots.lock().expect("scoped convoy snapshot lock poisoned");
            let projected_changed =
                snapshots.get(&query).is_none_or(|previous| previous.rows != result_set.rows || previous.state != result_set.state);
            let is_stale = snapshots.get(&query).is_some_and(|previous| previous.seq > result_set.seq);
            tracing::debug!(
                query = %query,
                rows = result_set.rows.len(),
                projected_changed,
                "evaluated scoped convoy snapshot"
            );
            if !is_stale {
                snapshots.insert(query, result_set.clone());
            }
            drop(snapshots);
            if projected_changed && !is_stale {
                changed.push(result_set);
            }
        }
        changed
    }

    fn remember_scoped_convoy_snapshot(&self, result_set: &ResultSet) {
        let query = result_set.query();
        let mut snapshots = self.scoped_convoy_snapshots.lock().expect("scoped convoy snapshot lock poisoned");
        if snapshots.get(&query).is_none_or(|previous| previous.seq <= result_set.seq) {
            snapshots.insert(query, result_set.clone());
        }
    }

    pub async fn awareness_result_set(&self, scope: &Option<QueryScope>, grouping: AwarenessGrouping, limit: AwarenessLimit) -> ResultSet {
        let (projects, project_catalog_revision) = {
            let catalog = self.project_catalog.read().await;
            let projects = sorted_project_scopes(&catalog.projects)
                .into_iter()
                .filter(|project| scope.as_ref().is_none_or(|scope| project == scope))
                .collect::<Vec<_>>();
            (projects, catalog.revision)
        };
        let convoys = {
            let set = self.result_set().await;
            let rows = match set.rows {
                Rows::Convoys { rows, .. } => rows,
                _ => Vec::new(),
            };
            rows.into_iter().filter(|row| scope.as_ref().is_none_or(|scope| convoy_matches_scope(row, scope))).collect::<Vec<_>>()
        };
        let independents_set = self.independents_result_set(scope).await;
        let independents = independents_set.rows.as_independents().map_or_else(Vec::new, ToOwned::to_owned);
        let checkouts_set = self.checkouts.write().await.result_set(scope);
        let checkouts = checkouts_set.rows.as_checkouts().map_or_else(Vec::new, ToOwned::to_owned);
        let issue_sets = self.issue_sets_for_awareness(scope).await;
        let issues = issue_sets
            .iter()
            .flat_map(|(scope, set)| {
                set.rows.as_issues().into_iter().flatten().cloned().map(|row| ScopedIssueRow { scope: Some(scope.clone()), row })
            })
            .collect::<Vec<_>>();
        let state = merged_issue_state(&issue_sets);
        let (salience, salience_revision) = {
            let projection = self.salience.read().await;
            (projection.facts.clone(), projection.revision)
        };
        let base_seq =
            [self.seq().await, independents_set.seq, checkouts_set.seq, issue_sets.iter().map(|(_, set)| set.seq).max().unwrap_or(0)]
                .into_iter()
                .max()
                .unwrap_or(0);
        let seq = base_seq.saturating_add(project_catalog_revision).saturating_add(salience_revision);
        let (rows, state) = project_awareness(AwarenessInput {
            scope: scope.clone(),
            grouping,
            limit,
            projects,
            convoys,
            issues,
            checkouts,
            independents,
            salience,
            state,
        });
        ResultSet { seq, rows: Rows::Awareness { scope: scope.clone(), grouping, limit, rows }, state }
    }

    async fn issue_sets_for_awareness(&self, scope: &Option<QueryScope>) -> Vec<(QueryScope, ResultSet)> {
        let scopes = match scope {
            Some(scope) => vec![scope.clone()],
            None => sorted_project_scopes(&self.project_catalog.read().await.projects),
        };
        scopes
            .into_iter()
            .filter_map(|scope| {
                self.demand_backed
                    .result_set(&QueryId::Issues { scope: scope.clone(), search: None, label: Some(READY_ISSUE_LABEL.into()) })
                    .map(|set| (scope, set))
            })
            .collect()
    }
}

fn sorted_project_scopes(projects: &HashMap<QueryScope, Vec<RepositoryKey>>) -> Vec<QueryScope> {
    let mut scopes = projects.keys().cloned().collect::<Vec<_>>();
    scopes.sort_by(|left, right| (&left.namespace, &left.name).cmp(&(&right.namespace, &right.name)));
    scopes
}

fn convoy_phase_represents_issues(phase: ConvoyPhase) -> bool {
    matches!(phase, ConvoyPhase::Pending | ConvoyPhase::Active | ConvoyPhase::Interrupted | ConvoyPhase::Anchored | ConvoyPhase::Landing)
}

fn convoy_matches_scope(row: &ConvoyRow, scope: &QueryScope) -> bool {
    row.resource.namespace == scope.namespace
        && row
            .project_ref
            .as_deref()
            .is_some_and(|project| project == scope.name || project == format!("{}/{}", scope.namespace, scope.name))
}

fn merged_issue_state(issue_sets: &[(QueryScope, ResultSet)]) -> ResultSetState {
    let mut state = ResultSetState::default();
    for (_, set) in issue_sets {
        state.conditions.extend(set.state.conditions.clone());
        state.truncated |= set.state.truncated;
        if let Some(demand) = &set.state.demand {
            state.demand = Some(match state.demand {
                Some(existing) => flotilla_protocol::DemandBackedMetadata {
                    as_of: existing.as_of.max(demand.as_of),
                    has_more: existing.has_more || demand.has_more,
                },
                None => demand.clone(),
            });
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use flotilla_protocol::{
        AwarenessKind, DemandBackedMetadata, Issue, IssueRef, IssueSource, IssueState, QueryCursor, ResultSetState, Rows,
    };
    use flotilla_resources::PrincipalRef;

    use super::*;
    use crate::salience::RegardFact;

    fn scope_in(namespace: &str, name: &str) -> QueryScope {
        QueryScope::new(namespace, name)
    }

    fn scope(name: &str) -> QueryScope {
        scope_in("flotilla", name)
    }

    fn convoy_row(namespace: &str, name: &str, project_ref: &str) -> ConvoyRow {
        ConvoyRow::builder()
            .resource(ResourceRef::new("flotilla.work/v1", "Convoy", namespace, name))
            .name(name)
            .workflow_ref("implement")
            .phase(ConvoyPhase::Active)
            .project_ref(project_ref)
            .build()
    }

    fn issue_row(scope: &str, id: &str) -> IssueRow {
        let reference = IssueRef { source: IssueSource { service: "https://github.com".into(), scope: scope.into() }, id: id.into() };
        IssueRow {
            reference: reference.clone(),
            issue: Issue {
                reference,
                title: format!("Issue {id}"),
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

    #[tokio::test]
    async fn fleet_awareness_subscription_demands_project_issue_windows() {
        let state = AggregatorProjectionState::new();
        let project = scope("roadmap");
        state
            .replace_store_catalog(
                HashMap::from([(RepositoryKey("repo-a".into()), "a".to_string())]),
                HashMap::from([(project.clone(), vec![])]),
            )
            .await;

        let awareness = QueryId::Awareness { scope: None, grouping: AwarenessGrouping::Project, limit: AwarenessLimit::default() };
        state.replace_subscriber(Uuid::new_v4(), &[QueryCursor { query: awareness, since: None }]);

        assert!(state.subscribe_demand().borrow().contains_key(&QueryId::Issues {
            scope: project,
            search: None,
            label: Some(READY_ISSUE_LABEL.into()),
        }));
    }

    #[tokio::test]
    async fn fleet_awareness_subscription_retracts_removed_project_issue_windows_but_preserves_explicit_demand() {
        let state = AggregatorProjectionState::new();
        let retained = scope("retained");
        let removed = scope("removed");
        let repositories = HashMap::from([(RepositoryKey("repo-a".into()), "a".to_string())]);
        state.replace_store_catalog(repositories.clone(), HashMap::from([(retained.clone(), vec![]), (removed.clone(), vec![])])).await;

        let subscriber = Uuid::new_v4();
        let awareness = QueryId::Awareness { scope: None, grouping: AwarenessGrouping::Project, limit: AwarenessLimit::default() };
        state.replace_subscriber(subscriber, &[QueryCursor { query: awareness.clone(), since: None }]);
        let retained_issues = QueryId::Issues { scope: retained.clone(), search: None, label: Some(READY_ISSUE_LABEL.into()) };
        let removed_issues = QueryId::Issues { scope: removed.clone(), search: None, label: Some(READY_ISSUE_LABEL.into()) };
        assert_eq!(
            state.subscribe_demand().borrow().keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([retained_issues.clone(), removed_issues.clone()])
        );

        state.replace_store_catalog(repositories.clone(), HashMap::from([(retained, vec![])])).await;
        assert_eq!(state.subscribe_demand().borrow().keys().cloned().collect::<HashSet<_>>(), HashSet::from([retained_issues]));

        state.replace_subscriber(subscriber, &[QueryCursor { query: awareness, since: None }, QueryCursor {
            query: removed_issues.clone(),
            since: None,
        }]);
        state.replace_store_catalog(repositories, HashMap::new()).await;
        assert_eq!(state.subscribe_demand().borrow().keys().cloned().collect::<HashSet<_>>(), HashSet::from([removed_issues]));
    }

    #[tokio::test]
    async fn fleet_awareness_groups_loaded_project_issues_under_projects() {
        let state = AggregatorProjectionState::new();
        let project = scope("roadmap");
        state
            .replace_store_catalog(
                HashMap::from([(RepositoryKey("repo-a".into()), "a".to_string())]),
                HashMap::from([(project.clone(), vec![])]),
            )
            .await;
        let issue_query = QueryId::Issues { scope: project.clone(), search: None, label: Some(READY_ISSUE_LABEL.into()) };
        state.replace_subscriber(Uuid::new_v4(), &[QueryCursor { query: issue_query.clone(), since: None }]);
        let generation = *state.subscribe_demand().borrow().get(&issue_query).expect("issue query generation");
        state.replace_issues(&issue_query, generation, vec![issue_row("flotilla-org/flotilla", "862")], ResultSetState {
            demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more: false }),
            conditions: vec![],
            truncated: false,
        });

        let result = state.awareness_result_set(&None, AwarenessGrouping::Project, AwarenessLimit::default()).await;
        let Rows::Awareness { rows, .. } = result.rows else { panic!("awareness rows") };

        assert!(rows.iter().any(|node| {
            node.kind == AwarenessKind::Project
                && node.scope.as_ref() == Some(&project)
                && node.entries.iter().any(|entry| entry.kind == AwarenessKind::Issue)
        }));
    }

    #[tokio::test]
    async fn fleet_awareness_emits_every_catalogued_project_including_empty_projects() {
        let state = AggregatorProjectionState::new();
        let active = scope("active");
        let empty = scope("empty");
        state
            .replace_store_catalog(
                HashMap::from([(RepositoryKey("repo-a".into()), "a".to_string())]),
                HashMap::from([(active.clone(), vec![]), (empty.clone(), vec![])]),
            )
            .await;
        let active_convoy = convoy_row("flotilla", "ship-it", "active");
        {
            let mut convoys = state.write().await;
            convoys.local_rows.insert(active_convoy.resource.clone(), active_convoy);
        }

        let result = state.awareness_result_set(&None, AwarenessGrouping::Project, AwarenessLimit::default()).await;
        let Rows::Awareness { rows, .. } = result.rows else { panic!("awareness rows") };

        assert_eq!(rows.len(), 2);
        let empty_node = rows.iter().find(|node| node.scope.as_ref() == Some(&empty)).expect("empty project awareness node");
        assert_eq!(empty_node.state, flotilla_protocol::AwarenessState::Idle);
        assert_eq!(empty_node.counts, flotilla_protocol::AwarenessCounts::default());
        assert!(empty_node.entries.is_empty());
    }

    #[tokio::test]
    async fn convoy_result_sets_filter_by_optional_project_scope_without_changing_the_global_set() {
        let state = AggregatorProjectionState::new();
        let roadmap = convoy_row("flotilla", "roadmap-work", "roadmap");
        let other_project = convoy_row("flotilla", "other-work", "other");
        let other_namespace = convoy_row("platform", "roadmap-work", "roadmap");
        {
            let mut convoys = state.write().await;
            convoys.local_rows =
                [roadmap.clone(), other_project, other_namespace].into_iter().map(|row| (row.resource.clone(), row)).collect();
            convoys.seq = 1;
        }

        let scoped = state.result_set_for(&QueryId::Convoys { scope: Some(scope("roadmap")) }).await.expect("scoped convoy result set");
        assert_eq!(scoped.rows.as_convoys().expect("scoped convoy rows"), &[roadmap]);

        let global = state.result_set_for(&QueryId::Convoys { scope: None }).await.expect("global convoy result set");
        assert_eq!(global.rows.as_convoys().expect("global convoy rows").len(), 3);
    }

    #[tokio::test]
    async fn scoped_convoy_snapshots_advance_only_when_projected_rows_change() {
        let state = AggregatorProjectionState::new();
        let query = QueryId::Convoys { scope: Some(scope("roadmap")) };
        let roadmap = convoy_row("flotilla", "roadmap-work", "roadmap");
        let mut unrelated = convoy_row("flotilla", "other-work", "other");
        {
            let mut convoys = state.write().await;
            convoys.local_rows = [roadmap.clone(), unrelated.clone()].into_iter().map(|row| (row.resource.clone(), row)).collect();
            convoys.seq = 1;
        }
        state.replace_subscriber(Uuid::new_v4(), &[QueryCursor { query: query.clone(), since: None }]);
        let initial = state.result_set_for(&query).await.expect("initial scoped result set");
        assert_eq!(initial.rows.as_convoys().expect("initial scoped rows"), std::slice::from_ref(&roadmap));

        unrelated.workflow_ref = "review".into();
        {
            let mut convoys = state.write().await;
            convoys.local_rows.insert(unrelated.resource.clone(), unrelated);
            convoys.seq = 2;
        }
        assert!(state.changed_scoped_convoy_result_sets().await.is_empty());

        let mut changed_roadmap = roadmap.clone();
        changed_roadmap.workflow_ref = "review".into();
        {
            let mut convoys = state.write().await;
            convoys.local_rows.insert(changed_roadmap.resource.clone(), changed_roadmap.clone());
            convoys.seq = 3;
        }
        let changed = state.changed_scoped_convoy_result_sets().await;
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].rows.as_convoys().expect("changed scoped rows"), std::slice::from_ref(&changed_roadmap));

        {
            let mut convoys = state.write().await;
            convoys.local_rows.remove(&changed_roadmap.resource);
            convoys.seq = 4;
        }
        let removed = state.changed_scoped_convoy_result_sets().await;
        assert_eq!(removed.len(), 1);
        assert!(removed[0].rows.is_empty());
    }

    #[tokio::test]
    async fn convoy_result_set_includes_terminal_records_until_they_are_deleted() {
        let state = AggregatorProjectionState::new();
        let active = convoy_row("flotilla", "active", "roadmap");
        let mut landing = convoy_row("flotilla", "landing", "roadmap");
        landing.phase = ConvoyPhase::Landing;
        let mut landed = convoy_row("flotilla", "landed", "roadmap");
        landed.phase = ConvoyPhase::Landed;
        let mut failed = convoy_row("flotilla", "failed", "roadmap");
        failed.phase = ConvoyPhase::Failed;
        {
            let mut convoys = state.write().await;
            convoys.local_rows = [active.clone(), landing.clone(), landed.clone(), failed.clone()]
                .into_iter()
                .map(|row| (row.resource.clone(), row))
                .collect();
        }

        let result = state.result_set_for(&QueryId::Convoys { scope: None }).await.expect("convoy result set");

        assert_eq!(result.rows.as_convoys().expect("convoy rows"), &[active, failed, landed, landing]);
    }

    #[tokio::test]
    async fn empty_project_catalog_changes_advance_awareness_sequence() {
        let state = AggregatorProjectionState::new();
        state.write().await.seq = 10;
        let before = state.awareness_result_set(&None, AwarenessGrouping::Project, AwarenessLimit::default()).await;

        state.replace_store_catalog(HashMap::new(), HashMap::from([(scope("empty"), vec![])])).await;

        let after = state.awareness_result_set(&None, AwarenessGrouping::Project, AwarenessLimit::default()).await;
        assert!(after.seq > before.seq);
        assert_eq!(after.rows.as_awareness().expect("awareness rows").len(), 1);
    }

    #[tokio::test]
    async fn scoped_awareness_filters_convoys_by_project_namespace() {
        let state = AggregatorProjectionState::new();
        let project = scope_in("team-a", "roadmap");
        let matching = convoy_row("team-a", "matching", "roadmap");
        let other_namespace = convoy_row("team-b", "other-namespace", "roadmap");
        {
            let mut convoys = state.write().await;
            convoys.local_rows = [matching.clone(), other_namespace].into_iter().map(|row| (row.resource.clone(), row)).collect();
            convoys.seq = 1;
        }

        let result = state.awareness_result_set(&Some(project.clone()), AwarenessGrouping::Project, AwarenessLimit::default()).await;
        let Rows::Awareness { rows, .. } = result.rows else { panic!("awareness rows") };

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].scope.as_ref(), Some(&project));
        assert_eq!(rows[0].counts.convoys, 1);
        assert_eq!(rows[0].entries.iter().filter(|entry| entry.kind == AwarenessKind::Convoy).count(), 1);
        assert!(rows[0].refs.contains(&matching.resource));
    }

    #[tokio::test]
    async fn salience_only_changes_advance_awareness_sequence() {
        let state = AggregatorProjectionState::new();
        let query_scope = Some(scope("roadmap"));
        let before = state.awareness_result_set(&query_scope, AwarenessGrouping::Project, AwarenessLimit::default()).await;

        assert!(
            state
                .replace_salience_facts(SalienceFacts {
                    regards: vec![RegardFact {
                        principal: PrincipalRef { namespace: "flotilla".into(), name: "operator".into() },
                        target: ResourceRef::new("flotilla.work/v1", "Project", "flotilla", "roadmap"),
                        as_of: Utc::now(),
                    }],
                    ..SalienceFacts::default()
                })
                .await
        );

        let after = state.awareness_result_set(&query_scope, AwarenessGrouping::Project, AwarenessLimit::default()).await;
        assert!(after.seq > before.seq);
    }
}
