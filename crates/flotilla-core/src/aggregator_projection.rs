//! Shared Aggregator state used for replay and fleet-replica export.
//!
//! [`QueryProjection`] is the query-agnostic core: rows keyed by
//! [`ResourceRef`] (local plus per-replica-host), one contiguous sequence
//! counter per query. Each named query instantiates it with that query's
//! typed row.

use std::{collections::HashMap, sync::Arc};

use flotilla_protocol::{
    result_set::{ConvoyRow, QueryId, ResultSet, Rows},
    HostName, ResourceRef,
};
use tokio::sync::{RwLock, RwLockWriteGuard};

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
        ResultSet { seq: self.seq, rows: R::into_rows(rows) }
    }
}

#[derive(Debug, Default, Clone)]
pub struct AggregatorProjectionState {
    convoys: Arc<RwLock<QueryProjection<ConvoyRow>>>,
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

    /// This host's local result sets across all named queries, in the order
    /// of [`QueryId::ALL`].
    pub async fn local_result_sets(&self) -> Vec<ResultSet> {
        vec![self.local_result_set().await]
    }

    /// The current fleet-merged result set for one named query.
    pub async fn result_set_for(&self, query: QueryId) -> ResultSet {
        match query {
            QueryId::Convoys => self.result_set().await,
        }
    }
}
