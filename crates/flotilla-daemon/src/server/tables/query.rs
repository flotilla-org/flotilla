//! Named queries as tables. A [`QueryFeed`] is one aggregator subscriber, as a framed client
//! connection is: a counted set of the queries its subscriptions read. A table reads its query's
//! current result set and shreds it through the row type's description.
use super::{bad, Failure, Row};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{AwarenessGrouping, AwarenessLimit, DaemonEvent, QueryCursor, QueryId, QueryScope};
use flotilla_schema::TableSet;
use futures::{stream::BoxStream, StreamExt};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::{broadcast::error::RecvError, watch};

/// One named-query family served as tables: its description and the query a table read names
/// when it names none (`None` where the query needs a scope).
struct Family {
    set: TableSet,
    default: Option<QueryId>,
}

fn families() -> &'static [Family] {
    static FAMILIES: OnceLock<Vec<Family>> = OnceLock::new();
    FAMILIES.get_or_init(|| {
        let issues = QueryId::Issues { scope: QueryScope::new("", ""), search: None, label: None };
        let awareness = QueryId::Awareness { scope: None, grouping: AwarenessGrouping::default(), limit: AwarenessLimit::default() };
        QueryId::ALWAYS_MATERIALIZED
            .iter()
            .cloned()
            .map(Some)
            .chain([Some(awareness)])
            .map(|default| Family { set: default.as_ref().expect("default").table_set(), default })
            .chain([Family { set: issues.table_set(), default: None }])
            .collect()
    })
}

fn family(table: &str) -> Option<&'static Family> {
    families().iter().find(|family| family.set.tables.iter().any(|candidate| candidate.name == table))
}

/// The `/tables` listing entries of the named-query families.
pub(super) fn list() -> Vec<serde_json::Value> {
    families()
        .iter()
        .map(|family| {
            let names: Vec<_> = family.set.tables.iter().map(|table| table.name.clone()).collect();
            let query = family.default.as_ref().map(QueryId::family).unwrap_or("issues");
            serde_json::json!({"kind": family.set.kind, "query": query, "root": names[0], "tables": names})
        })
        .collect()
}

/// One aggregator subscriber: a WebSocket connection's, or a GET read's, which holds no query.
pub(crate) struct QueryFeed {
    daemon: Arc<InProcessDaemon>,
    subscriber: uuid::Uuid,
    incarnation: String,
    /// Held queries, counted by the subscriptions reading them, each with its change signal.
    held: Mutex<HashMap<QueryId, (usize, watch::Sender<()>)>>,
}

impl QueryFeed {
    pub(crate) fn new(daemon: Arc<InProcessDaemon>, incarnation: String) -> Arc<Self> {
        Arc::new(Self { daemon, subscriber: uuid::Uuid::new_v4(), incarnation, held: Mutex::default() })
    }

    pub(crate) fn daemon(&self) -> &Arc<InProcessDaemon> {
        &self.daemon
    }

    fn held(&self) -> std::sync::MutexGuard<'_, HashMap<QueryId, (usize, watch::Sender<()>)>> {
        self.held.lock().expect("query feed lock poisoned")
    }

    /// Holds `query` for one more subscription; the first materialises it for this subscriber.
    pub(crate) async fn join(&self, query: QueryId) -> Result<(), Failure> {
        let first = {
            let mut held = self.held();
            let entry = held.entry(query.clone()).or_insert_with(|| (0, watch::channel(()).0));
            entry.0 += 1;
            entry.0 == 1
        };
        if !first {
            return Ok(());
        }
        let subscribed = self.resubscribe().await;
        if subscribed.is_err() {
            self.leave(&query).await;
        }
        subscribed
    }

    /// Releases one subscription's hold on `query`; the last releases the daemon's demand.
    pub(crate) async fn leave(&self, query: &QueryId) {
        let last = {
            let mut held = self.held();
            let Some(entry) = held.get_mut(query) else { return };
            entry.0 -= 1;
            entry.0 == 0 && held.remove(query).is_some()
        };
        if last {
            let _ = self.resubscribe().await;
        }
    }

    /// Replaces this subscriber's demand with the held queries, as `SubscribeQueries` does.
    async fn resubscribe(&self) -> Result<(), Failure> {
        let cursors: Vec<_> = self.held().keys().map(|query| QueryCursor { query: query.clone(), since: None }).collect();
        if cursors.is_empty() {
            self.daemon.unsubscribe_queries(self.subscriber).await;
            return Ok(());
        }
        self.daemon.subscribe_queries(self.subscriber, &cursors).await.map(drop).map_err(|reason| bad("table_source", reason))
    }

    pub(crate) fn holds(&self, query: &QueryId) -> bool {
        self.held().contains_key(query)
    }

    /// Signals the held query a daemon event changes; `None` (a lagged receiver) signals all.
    pub(crate) fn offer(&self, event: Option<&DaemonEvent>) {
        let query = match event {
            Some(DaemonEvent::ResultSet(set)) => Some(set.query()),
            Some(DaemonEvent::ResultDelta(delta)) => Some(delta.query()),
            Some(_) => return,
            None => None,
        };
        for (held, (_, changed)) in self.held().iter() {
            if query.as_ref().is_none_or(|query| query == held) {
                changed.send_replace(());
            }
        }
    }

    /// Ends the subscriber: no held query, no demand.
    pub(crate) async fn close(&self) {
        self.held().clear();
        self.daemon.unsubscribe_queries(self.subscriber).await;
    }

    /// The incarnation of `query`'s sequence: the daemon run's, and for queries over issue
    /// materialisations, whose sequences restart with each, their generations.
    async fn incarnation(&self, query: &QueryId) -> String {
        let demand = self.daemon.aggregator_projection_state().await.subscribe_demand().borrow().clone();
        let mut generations: Vec<u64> = demand
            .into_iter()
            .filter(|(issues, _)| match query {
                QueryId::Issues { .. } => issues == query,
                QueryId::Awareness { scope, .. } => matches!(issues, QueryId::Issues { scope: issued, search: None, label: Some(_) }
                    if scope.as_ref().is_none_or(|scope| scope == issued)),
                _ => false,
            })
            .map(|(_, generation)| generation)
            .collect();
        generations.sort_unstable();
        generations.iter().fold(self.incarnation.clone(), |text, generation| format!("{text}.{generation}"))
    }
}

/// One table of a named query's description, read through a feed.
#[derive(Clone)]
pub(crate) struct Source {
    feed: Arc<QueryFeed>,
    family: &'static Family,
    query: Option<QueryId>,
    table: String,
}

impl Source {
    pub(super) fn open(table: &str, feed: Option<&Arc<QueryFeed>>) -> Option<Self> {
        let family = family(table)?;
        let feed = Arc::clone(feed?);
        Some(Self { feed, family, query: family.default.clone(), table: table.to_owned() })
    }

    pub(super) fn name(&self) -> &str {
        &self.table
    }

    pub(super) fn set(&self) -> &TableSet {
        &self.family.set
    }

    /// Reads `query` instead of the family's default; it must be of the table's family.
    pub(crate) fn with_query(mut self, query: QueryId) -> Result<Self, Failure> {
        if query.table_set().kind != self.family.set.kind {
            return Err(bad("invalid_request", format!("query {query} does not serve table {}", self.table)));
        }
        self.query = Some(query);
        Ok(self)
    }

    pub(crate) fn query(&self) -> Result<&QueryId, Failure> {
        self.query.as_ref().ok_or_else(|| bad("invalid_request", format!("table {} needs a query", self.table)))
    }

    /// The current rows of this table, the result set's sequence and its incarnation.
    pub(super) async fn rows(&self) -> Result<(Vec<Row>, u64, String), Failure> {
        let query = self.query()?;
        loop {
            let incarnation = self.feed.incarnation(query).await;
            let recovered = self.feed.daemon.recover_queries(&[QueryCursor { query: query.clone(), since: None }]).await;
            let set = recovered.map_err(|reason| bad("table_source", reason))?.into_iter().find_map(|event| match event {
                DaemonEvent::ResultSet(set) => Some(set),
                _ => None,
            });
            let set = set.ok_or_else(|| bad("table_source", format!("query is not materialized: {query}")))?;
            // A rematerialisation between the two reads would pair a sequence with the wrong lifetime.
            if self.feed.incarnation(query).await == incarnation {
                let mut tables = set.rows.tables(&self.family.set.tables[0].name);
                return Ok((tables.remove(&self.table).unwrap_or_default(), set.seq, incarnation));
            }
        }
    }

    /// A signal per change of the query, starting with one so a change since the snapshot is
    /// read. A held query is signalled by its feed; a read holding none follows the daemon itself.
    pub(super) fn events(&self) -> BoxStream<'static, Result<(), String>> {
        let Ok(query) = self.query().cloned() else { return futures::stream::once(async { Err("table needs a query".into()) }).boxed() };
        let first = futures::stream::once(async { Ok(()) });
        if let Some((_, changed)) = self.feed.held().get(&query) {
            let changes = futures::stream::unfold(changed.subscribe(), |mut changes| async move {
                changes.changed().await.ok().map(|()| (Ok(()), changes))
            });
            return first.chain(changes).boxed();
        }
        let changes = futures::stream::unfold(self.feed.daemon.subscribe(), move |mut events| {
            let query = query.clone();
            async move {
                loop {
                    match events.recv().await {
                        Ok(DaemonEvent::ResultSet(set)) if set.query() == query => break,
                        Ok(DaemonEvent::ResultDelta(delta)) if delta.query() == query => break,
                        Ok(_) => continue,
                        Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => return None,
                    }
                }
                Some((Ok(()), events))
            }
        });
        first.chain(changes).boxed()
    }
}
