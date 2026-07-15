//! Shared Aggregator state used for replay and fleet-replica export.
//!
//! [`QueryProjection`] is the query-agnostic core: rows keyed by
//! [`ResourceRef`] (local plus per-replica-host), one contiguous sequence
//! counter per query. Each named query instantiates it with that query's
//! typed row.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use flotilla_protocol::{
    result_set::{ConvoyRow, IndependentRow, IssueRow, QueryId, ResultDelta, ResultSet, ResultSetState, Rows},
    HostName, IssueRef, QueryCursor, ResourceRef,
};
use tokio::sync::{broadcast, watch, RwLock, RwLockWriteGuard};
use uuid::Uuid;

use crate::query_registry::QueryRegistry;

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

impl QueryRow for IndependentRow {
    fn resource(&self) -> &ResourceRef {
        &self.resource
    }

    fn into_rows(rows: Vec<Self>) -> Rows {
        Rows::Independents(rows)
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

#[derive(Debug, Default, Clone)]
pub struct AggregatorProjectionState {
    convoys: Arc<RwLock<QueryProjection<ConvoyRow>>>,
    independents: Arc<RwLock<QueryProjection<IndependentRow>>>,
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

    pub async fn write_independents(&self) -> RwLockWriteGuard<'_, QueryProjection<IndependentRow>> {
        self.independents.write().await
    }

    pub async fn independents_result_set(&self) -> ResultSet {
        self.independents.read().await.result_set()
    }

    pub async fn independents_seq(&self) -> u64 {
        self.independents.read().await.seq
    }

    pub async fn local_independents_result_set(&self) -> ResultSet {
        self.independents.read().await.local_result_set()
    }

    /// This host's local store-backed result sets. Demand-backed reference
    /// data is never included in fleet replica snapshots.
    pub async fn local_result_sets(&self) -> Vec<ResultSet> {
        vec![self.local_result_set().await, self.local_independents_result_set().await]
    }

    /// Replace one subscriber's complete demand and return queries whose
    /// materialization lifetime was newly created.
    pub fn replace_subscriber(&self, subscriber: Uuid, cursors: &[QueryCursor]) -> HashSet<QueryId> {
        self.demand_backed.replace(subscriber, cursors)
    }

    pub fn remove_subscriber(&self, subscriber: Uuid) {
        self.demand_backed.remove(subscriber);
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

    /// The current fleet-merged result set for one named query.
    pub async fn result_set_for(&self, query: &QueryId) -> Option<ResultSet> {
        match query {
            QueryId::Convoys => Some(self.result_set().await),
            QueryId::Independents => Some(self.independents_result_set().await),
            QueryId::Issues { .. } => self.demand_backed.result_set(query),
        }
    }
}
