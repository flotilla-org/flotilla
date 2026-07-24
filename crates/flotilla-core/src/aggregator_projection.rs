//! Shared Aggregator state used for replay and fleet-replica export.
//!
//! [`QueryProjection`] maintains the unscoped Convoys family. Store-backed
//! families with Project views share the scoped projection in
//! `scoped_store`.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
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
        Rows::Convoys(rows)
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

#[derive(Debug, Default, Clone, bon::Builder)]
pub struct AggregatorProjectionState {
    convoys: Arc<RwLock<QueryProjection<ConvoyRow>>>,
    #[builder(skip)]
    independents: Arc<RwLock<ScopedIndependentProjection>>,
    #[builder(skip)]
    checkouts: Arc<RwLock<ScopedCheckoutProjection>>,
    #[builder(skip)]
    salience: Arc<RwLock<SalienceProjection>>,
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
        let mut deltas = self.independents.write().await.replace_catalog(repositories.clone(), projects.clone());
        deltas.extend(self.checkouts.write().await.replace_catalog(repositories, projects));
        let scopes = self.checkouts.read().await.project_scopes();
        self.demand_backed.expand_fleet_awareness_demands(&scopes);
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
        self.demand_backed.replace(subscriber, cursors)
    }

    pub async fn replace_subscriber_expanding_awareness(&self, subscriber: Uuid, cursors: &[QueryCursor]) -> HashSet<QueryId> {
        let mut expanded = cursors.to_vec();
        if cursors.iter().any(|cursor| matches!(cursor.query, QueryId::Awareness { scope: None, .. })) {
            for scope in self.checkouts.read().await.project_scopes() {
                let query = QueryId::Issues { scope, search: None, label: Some(READY_ISSUE_LABEL.into()) };
                if !expanded.iter().any(|cursor| cursor.query == query) {
                    expanded.push(QueryCursor { query, since: None });
                }
            }
        }
        self.replace_subscriber(subscriber, &expanded)
    }

    pub fn remove_subscriber(&self, subscriber: Uuid) {
        self.demand_backed.remove(subscriber);
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
        match query {
            QueryId::Convoys => Some(self.result_set().await),
            QueryId::Independents { scope } => Some(self.independents_result_set(scope).await),
            QueryId::Issues { .. } => self.demand_backed.result_set(query),
            QueryId::Checkouts { scope } => Some(self.checkouts.write().await.result_set(scope)),
            QueryId::Awareness { scope, grouping, limit } => Some(self.awareness_result_set(scope, *grouping, *limit).await),
        }
    }

    pub async fn awareness_result_set(&self, scope: &Option<QueryScope>, grouping: AwarenessGrouping, limit: AwarenessLimit) -> ResultSet {
        let convoys = {
            let set = self.result_set().await;
            let rows = match set.rows {
                Rows::Convoys(rows) => rows,
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
        let seq = base_seq.saturating_add(salience_revision);
        let (rows, state) = project_awareness(AwarenessInput {
            scope: scope.clone(),
            grouping,
            limit,
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
            None => self.checkouts.read().await.project_scopes(),
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

fn convoy_phase_represents_issues(phase: ConvoyPhase) -> bool {
    matches!(phase, ConvoyPhase::Pending | ConvoyPhase::Active)
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
        state.replace_subscriber_expanding_awareness(Uuid::new_v4(), &[QueryCursor { query: awareness, since: None }]).await;

        assert!(state.subscribe_demand().borrow().contains_key(&QueryId::Issues {
            scope: project,
            search: None,
            label: Some(READY_ISSUE_LABEL.into()),
        }));
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
