use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use flotilla_protocol::{
    DemandBackedMetadata, IssueRef, IssueRow, QueryChanges, QueryCursor, QueryId, ResultDelta, ResultSet, ResultSetCondition,
    ResultSetState, Rows,
};
use tokio::sync::{broadcast, watch};
use uuid::Uuid;

/// Subscriber ownership and live state for demand-backed query
/// materializations. Replacing one subscriber's set never disturbs another;
/// a demand-backed set exists exactly while its query has at least one owner.
#[derive(Clone, Debug)]
pub struct QueryRegistry {
    inner: Arc<Mutex<RegistryState>>,
    demand_tx: watch::Sender<HashMap<QueryId, u64>>,
    fetch_more_tx: broadcast::Sender<(QueryId, u64)>,
}

#[derive(Default, Debug)]
struct RegistryState {
    subscribers: HashMap<Uuid, HashSet<QueryId>>,
    demand_backed: HashMap<QueryId, ResultSet>,
    generations: HashMap<QueryId, u64>,
    next_generation: u64,
}

impl Default for QueryRegistry {
    fn default() -> Self {
        let (demand_tx, _) = watch::channel(HashMap::new());
        let (fetch_more_tx, _) = broadcast::channel(32);
        Self { inner: Arc::new(Mutex::new(RegistryState::default())), demand_tx, fetch_more_tx }
    }
}

impl QueryRegistry {
    /// Replace one subscriber's complete query set and return demand-backed
    /// queries whose materialization lifetime began with this replacement.
    /// Callers must replay those sets even when a retained client cursor
    /// happens to equal the new lifetime's initial sequence number.
    pub fn replace(&self, subscriber: Uuid, cursors: &[QueryCursor]) -> HashSet<QueryId> {
        let queries = cursors.iter().map(|cursor| cursor.query.clone()).collect();
        let mut state = self.inner.lock().expect("query registry lock poisoned");
        state.subscribers.insert(subscriber, queries);
        let created = reconcile_demand(&mut state);
        self.publish_demand(&state);
        created
    }

    pub fn remove(&self, subscriber: Uuid) {
        let mut state = self.inner.lock().expect("query registry lock poisoned");
        state.subscribers.remove(&subscriber);
        let _ = reconcile_demand(&mut state);
        self.publish_demand(&state);
    }

    pub fn result_set(&self, query: &QueryId) -> Option<ResultSet> {
        self.inner.lock().expect("query registry lock poisoned").demand_backed.get(query).cloned()
    }

    /// Replace the current window of a live issue materialization. A fetch
    /// that completes after the last subscriber left is discarded.
    pub fn replace_issues(&self, query: &QueryId, generation: u64, rows: Vec<IssueRow>, result_state: ResultSetState) -> Option<ResultSet> {
        let QueryId::Issues { scope } = query else { return None };
        let mut state = self.inner.lock().expect("query registry lock poisoned");
        if state.generations.get(query) != Some(&generation) {
            return None;
        }
        let current = state.demand_backed.get_mut(query)?;
        *current = ResultSet { seq: current.seq.saturating_add(1), rows: Rows::Issues { scope: scope.clone(), rows }, state: result_state };
        Some(current.clone())
    }

    /// Apply an ordinary typed issue delta to a live materialization and keep
    /// its replayable full window in sync.
    pub fn apply_issue_changes(
        &self,
        query: &QueryId,
        generation: u64,
        changed: Vec<IssueRow>,
        removed: Vec<IssueRef>,
        result_state: ResultSetState,
    ) -> Option<ResultDelta> {
        let QueryId::Issues { scope } = query else { return None };
        let mut registry = self.inner.lock().expect("query registry lock poisoned");
        if registry.generations.get(query) != Some(&generation) {
            return None;
        }
        let current = registry.demand_backed.get_mut(query)?;
        let Rows::Issues { rows, .. } = &mut current.rows else { return None };
        let mut by_reference = rows.drain(..).map(|row| (row.reference.clone(), row)).collect::<HashMap<_, _>>();
        for reference in &removed {
            by_reference.remove(reference);
        }
        for row in &changed {
            by_reference.insert(row.reference.clone(), row.clone());
        }
        *rows = by_reference.into_values().collect();
        rows.sort_by(|left, right| right.issue.as_of.cmp(&left.issue.as_of).then_with(|| left.reference.cmp(&right.reference)));
        current.seq = current.seq.saturating_add(1);
        current.state = result_state.clone();
        Some(ResultDelta {
            seq: current.seq,
            changes: QueryChanges::Issues { scope: scope.clone(), changed, removed },
            state: Some(result_state),
        })
    }

    pub fn subscribe_demand(&self) -> watch::Receiver<HashMap<QueryId, u64>> {
        self.demand_tx.subscribe()
    }

    pub fn subscribe_fetch_more(&self) -> broadcast::Receiver<(QueryId, u64)> {
        self.fetch_more_tx.subscribe()
    }

    pub fn request_fetch_more(&self, query: &QueryId) -> Result<(), String> {
        let state = self.inner.lock().expect("query registry lock poisoned");
        let result = state.demand_backed.get(query).ok_or_else(|| format!("query is not materialized: {query}"))?;
        if !result.state.demand.as_ref().is_some_and(|metadata| metadata.has_more) {
            return Err(format!("query has no more rows: {query}"));
        }
        let generation = *state.generations.get(query).ok_or_else(|| format!("query has no materialization generation: {query}"))?;
        drop(state);
        self.fetch_more_tx.send((query.clone(), generation)).map(|_| ()).map_err(|_| "issue materializer is unavailable".to_string())
    }

    fn publish_demand(&self, state: &RegistryState) {
        let demanded = state.generations.clone();
        if *self.demand_tx.borrow() != demanded {
            self.demand_tx.send_replace(demanded);
        }
    }

    #[cfg(test)]
    fn subscriber_count(&self, query: &QueryId) -> usize {
        self.inner.lock().expect("query registry lock poisoned").subscribers.values().filter(|queries| queries.contains(query)).count()
    }

    #[cfg(test)]
    fn generation(&self, query: &QueryId) -> u64 {
        *self.inner.lock().expect("query registry lock poisoned").generations.get(query).expect("live query generation")
    }
}

fn reconcile_demand(state: &mut RegistryState) -> HashSet<QueryId> {
    let demanded: HashSet<QueryId> = state
        .subscribers
        .values()
        .flat_map(|queries| queries.iter())
        .filter(|query| matches!(query, QueryId::Issues { .. }))
        .cloned()
        .collect();
    state.demand_backed.retain(|query, _| demanded.contains(query));
    state.generations.retain(|query, _| demanded.contains(query));
    let mut created = HashSet::new();
    for query in demanded {
        if !state.demand_backed.contains_key(&query) {
            state.next_generation = state.next_generation.saturating_add(1);
            let generation = state.next_generation;
            state.generations.insert(query.clone(), generation);
            state.demand_backed.insert(query.clone(), unavailable_issues_result_set(query.clone()));
            created.insert(query);
        }
    }
    created
}

fn unavailable_issues_result_set(query: QueryId) -> ResultSet {
    let QueryId::Issues { scope } = query else { unreachable!("demand-backed registry only materializes issue queries") };
    ResultSet {
        seq: 1,
        rows: Rows::Issues { scope, rows: vec![] },
        state: ResultSetState {
            demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more: false }),
            conditions: vec![ResultSetCondition::IssueSourceUnavailable {
                source: None,
                message: "issue source materialization is pending".to_string(),
            }],
        },
    }
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{QueryScope, RepositoryKey};

    use super::*;

    fn issues(name: &str) -> QueryId {
        QueryId::Issues { scope: QueryScope::Repository(RepositoryKey(name.into())) }
    }

    fn cursor(query: QueryId) -> QueryCursor {
        QueryCursor { query, since: None }
    }

    #[test]
    fn replacement_is_owned_by_subscriber_and_last_disconnect_tears_down() {
        let registry = QueryRegistry::default();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let a = issues("a");
        let b = issues("b");

        registry.replace(alice, &[cursor(a.clone())]);
        registry.replace(bob, &[cursor(a.clone())]);
        assert_eq!(registry.subscriber_count(&a), 2);
        assert!(registry.result_set(&a).is_some());

        registry.replace(alice, &[cursor(b.clone())]);
        assert_eq!(registry.subscriber_count(&a), 1);
        assert_eq!(registry.subscriber_count(&b), 1);
        assert!(registry.result_set(&a).is_some());

        registry.remove(bob);
        assert!(registry.result_set(&a).is_none());
        assert!(registry.result_set(&b).is_some());
        registry.remove(alice);
        assert!(registry.result_set(&b).is_none());
    }

    #[test]
    fn unavailable_is_distinct_from_a_valid_empty_window() {
        let registry = QueryRegistry::default();
        let query = issues("unserviceable");
        registry.replace(Uuid::new_v4(), &[cursor(query.clone())]);

        let result = registry.result_set(&query).expect("live materialization");
        assert!(result.rows.is_empty());
        assert!(matches!(result.state.conditions.as_slice(), [ResultSetCondition::IssueSourceUnavailable { source: None, .. }]));
        assert!(result.state.demand.is_some());
    }

    #[test]
    fn recreating_a_torn_down_query_reports_a_new_materialization_lifetime() {
        let registry = QueryRegistry::default();
        let subscriber = Uuid::new_v4();
        let query = issues("recreated");

        assert_eq!(registry.replace(subscriber, &[cursor(query.clone())]), HashSet::from([query.clone()]));
        let first_generation = registry.generation(&query);
        assert!(registry.replace(subscriber, &[cursor(query.clone())]).is_empty());
        registry.remove(subscriber);
        assert_eq!(registry.replace(subscriber, &[cursor(query.clone())]), HashSet::from([query.clone()]));
        let second_generation = registry.generation(&query);

        assert!(second_generation > first_generation);
        let ready = ResultSetState { demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more: false }), conditions: vec![] };
        assert!(registry.replace_issues(&query, first_generation, vec![], ready.clone()).is_none());
        assert!(registry.replace_issues(&query, second_generation, vec![], ready).is_some());
    }

    #[test]
    fn demand_watch_tracks_materialization_start_and_teardown() {
        let registry = QueryRegistry::default();
        let mut demand = registry.subscribe_demand();
        let subscriber = Uuid::new_v4();
        let query = issues("watched");

        registry.replace(subscriber, &[cursor(query.clone())]);
        assert!(demand.has_changed().expect("demand sender remains live"));
        assert_eq!(demand.borrow_and_update().keys().cloned().collect::<HashSet<_>>(), HashSet::from([query]));

        registry.remove(subscriber);
        assert!(demand.has_changed().expect("demand sender remains live"));
        assert!(demand.borrow_and_update().is_empty());
    }

    #[test]
    fn demand_watch_exposes_demand_that_predates_the_receiver() {
        let registry = QueryRegistry::default();
        let query = issues("already-live");
        registry.replace(Uuid::new_v4(), &[cursor(query.clone())]);

        let mut demand = registry.subscribe_demand();
        assert_eq!(demand.borrow_and_update().keys().cloned().collect::<HashSet<_>>(), HashSet::from([query]));
    }

    #[test]
    fn fetch_more_intent_is_available_only_for_a_live_window_with_more_rows() {
        let registry = QueryRegistry::default();
        let query = issues("paged");
        registry.replace(Uuid::new_v4(), &[cursor(query.clone())]);
        registry.replace_issues(&query, registry.generation(&query), vec![], ResultSetState {
            demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more: true }),
            conditions: vec![],
        });
        let mut intents = registry.subscribe_fetch_more();

        registry.request_fetch_more(&query).expect("live paged query accepts fetch-more");

        assert_eq!(intents.try_recv().expect("fetch-more intent"), (query.clone(), registry.generation(&query)));
    }

    #[test]
    fn queued_fetch_more_intent_retains_its_original_materialization_generation() {
        let registry = QueryRegistry::default();
        let subscriber = Uuid::new_v4();
        let query = issues("recreated-paged");
        let mut intents = registry.subscribe_fetch_more();
        registry.replace(subscriber, &[cursor(query.clone())]);
        let first_generation = registry.generation(&query);
        registry.replace_issues(&query, first_generation, vec![], ResultSetState {
            demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more: true }),
            conditions: vec![],
        });
        registry.request_fetch_more(&query).expect("first lifetime accepts fetch-more");

        registry.remove(subscriber);
        registry.replace(subscriber, &[cursor(query.clone())]);
        let second_generation = registry.generation(&query);

        assert!(second_generation > first_generation);
        assert_eq!(intents.try_recv().expect("queued fetch-more intent"), (query, first_generation));
    }
}
