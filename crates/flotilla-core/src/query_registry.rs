use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use flotilla_protocol::{DemandBackedMetadata, QueryCursor, QueryId, ResultSet, ResultSetCondition, ResultSetState, Rows};
use tokio::sync::watch;
use uuid::Uuid;

/// Subscriber ownership and live state for demand-backed query
/// materializations. Replacing one subscriber's set never disturbs another;
/// a demand-backed set exists exactly while its query has at least one owner.
#[derive(Clone, Debug)]
pub struct QueryRegistry {
    inner: Arc<Mutex<RegistryState>>,
    demand_tx: watch::Sender<HashSet<QueryId>>,
}

#[derive(Default, Debug)]
struct RegistryState {
    subscribers: HashMap<Uuid, HashSet<QueryId>>,
    demand_backed: HashMap<QueryId, ResultSet>,
}

impl Default for QueryRegistry {
    fn default() -> Self {
        let (demand_tx, _) = watch::channel(HashSet::new());
        Self { inner: Arc::new(Mutex::new(RegistryState::default())), demand_tx }
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

    pub fn subscribe_demand(&self) -> watch::Receiver<HashSet<QueryId>> {
        self.demand_tx.subscribe()
    }

    fn publish_demand(&self, state: &RegistryState) {
        let demanded = state.demand_backed.keys().cloned().collect();
        if *self.demand_tx.borrow() != demanded {
            self.demand_tx.send_replace(demanded);
        }
    }

    #[cfg(test)]
    fn subscriber_count(&self, query: &QueryId) -> usize {
        self.inner.lock().expect("query registry lock poisoned").subscribers.values().filter(|queries| queries.contains(query)).count()
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
    let mut created = HashSet::new();
    for query in demanded {
        if !state.demand_backed.contains_key(&query) {
            state.demand_backed.insert(query.clone(), unavailable_issues_result_set(query.clone()));
            created.insert(query);
        }
    }
    created
}

/// Until #747 supplies source resolution and the source-addressed provider
/// seam, a live issue query is explicitly unavailable. It must never be
/// represented as an ordinary empty issue window.
fn unavailable_issues_result_set(query: QueryId) -> ResultSet {
    let QueryId::Issues { scope } = query else { unreachable!("demand-backed registry only materializes issue queries") };
    ResultSet {
        seq: 1,
        rows: Rows::Issues { scope, rows: vec![] },
        state: ResultSetState {
            demand: Some(DemandBackedMetadata { as_of: Utc::now(), has_more: false }),
            conditions: vec![ResultSetCondition::IssueSourceUnavailable {
                source: None,
                message: "issue source resolution is unavailable until #747 supplies the source-addressed fetch seam".to_string(),
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
        assert!(registry.replace(subscriber, &[cursor(query.clone())]).is_empty());
        registry.remove(subscriber);
        assert_eq!(registry.replace(subscriber, &[cursor(query.clone())]), HashSet::from([query]));
    }

    #[test]
    fn demand_watch_tracks_materialization_start_and_teardown() {
        let registry = QueryRegistry::default();
        let mut demand = registry.subscribe_demand();
        let subscriber = Uuid::new_v4();
        let query = issues("watched");

        registry.replace(subscriber, &[cursor(query.clone())]);
        assert!(demand.has_changed().expect("demand sender remains live"));
        assert_eq!(*demand.borrow_and_update(), HashSet::from([query]));

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
        assert_eq!(*demand.borrow_and_update(), HashSet::from([query]));
    }
}
