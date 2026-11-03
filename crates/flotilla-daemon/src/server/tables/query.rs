//! Named queries as tables. A [`QueryFeed`] is one aggregator subscriber, as a framed client
//! connection is: it holds the result set of every query its tables read, applies the daemon's
//! result-set events to it, and its tables shred the held set through the row type's description.
use super::{bad, Failure, Row};
use flotilla_core::in_process::InProcessDaemon;
use flotilla_daemon_api::daemon::DaemonHandle;
use flotilla_protocol::{AwarenessGrouping, AwarenessLimit, DaemonEvent, QueryCursor, QueryId, QueryScope, ResultDelta, ResultSet};
use flotilla_schema::TableSet;
use futures::{stream::BoxStream, StreamExt};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::{broadcast::error::RecvError, watch};
use tracing::warn;

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

#[derive(Clone)]
enum Entry {
    /// A join is waiting on the daemon; result-set events for the query queue here meanwhile.
    Joining(Vec<Update>),
    Live {
        set: watch::Sender<Arc<ResultSet>>,
        users: usize,
    },
}

#[derive(Clone)]
enum Update {
    Set(ResultSet),
    Delta(ResultDelta),
}

impl Update {
    fn query(&self) -> QueryId {
        match self {
            Self::Set(set) => set.query(),
            Self::Delta(delta) => delta.query(),
        }
    }
}

/// Applies `update` to `held`; `Err` carries the sequence a gap left it at.
fn apply(held: &mut ResultSet, update: Update) -> Result<bool, u64> {
    match update {
        Update::Set(set) if set.seq == held.seq => Ok(false),
        Update::Set(set) => {
            *held = set;
            Ok(true)
        }
        Update::Delta(delta) if delta.seq <= held.seq => Ok(false),
        Update::Delta(delta) => held.apply(&delta).map(|()| true).map_err(|_| held.seq),
    }
}

struct Shared {
    daemon: Arc<InProcessDaemon>,
    subscriber: uuid::Uuid,
    entries: Mutex<HashMap<QueryId, Entry>>,
}

impl Shared {
    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<QueryId, Entry>> {
        self.entries.lock().expect("query feed lock poisoned")
    }

    /// Offers one daemon update; a gap answers the cursor to recover from.
    fn offer(&self, update: Update) -> Option<QueryCursor> {
        let query = update.query();
        let mut entries = self.entries();
        match entries.get_mut(&query)? {
            Entry::Joining(queued) => {
                queued.push(update);
                None
            }
            Entry::Live { set, .. } => {
                let mut held = ResultSet::clone(&set.borrow());
                match apply(&mut held, update) {
                    Ok(true) => {
                        set.send_replace(Arc::new(held));
                        None
                    }
                    Ok(false) => None,
                    Err(seq) => Some(QueryCursor { query, since: Some(seq) }),
                }
            }
        }
    }

    fn cursors(&self) -> Vec<QueryCursor> {
        self.entries()
            .iter()
            .filter_map(|(query, entry)| match entry {
                Entry::Live { set, .. } => Some(QueryCursor { query: query.clone(), since: Some(set.borrow().seq) }),
                Entry::Joining(_) => None,
            })
            .collect()
    }

    async fn recover(&self, cursors: Vec<QueryCursor>) {
        if cursors.is_empty() {
            return;
        }
        match self.daemon.recover_queries(&cursors).await {
            Ok(events) => {
                for event in events {
                    if let DaemonEvent::ResultSet(set) = event {
                        self.offer(Update::Set(*set));
                    }
                }
            }
            Err(error) => warn!(%error, subscriber = %self.subscriber, "named-query table recovery failed"),
        }
    }

    async fn relay(self: Arc<Self>, mut events: tokio::sync::broadcast::Receiver<DaemonEvent>) {
        loop {
            let update = match events.recv().await {
                Ok(DaemonEvent::ResultSet(set)) => Update::Set(*set),
                Ok(DaemonEvent::ResultDelta(delta)) => Update::Delta(*delta),
                Ok(_) => continue,
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, subscriber = %self.subscriber, "named-query table feed lagged");
                    self.recover(self.cursors()).await;
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            if let Some(gap) = self.offer(update) {
                self.recover(vec![gap]).await;
            }
        }
    }
}

/// One aggregator subscriber's held result sets: a WebSocket connection's, or one GET read's.
pub(crate) struct QueryFeed {
    shared: Arc<Shared>,
    incarnation: String,
    joins: tokio::sync::Mutex<()>,
    relay: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl QueryFeed {
    pub(crate) fn new(daemon: Arc<InProcessDaemon>, incarnation: String) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared { daemon, subscriber: uuid::Uuid::new_v4(), entries: Mutex::default() }),
            incarnation,
            joins: tokio::sync::Mutex::new(()),
            relay: Mutex::new(None),
        })
    }

    pub(crate) fn incarnation(&self) -> &str {
        &self.incarnation
    }

    pub(crate) fn daemon(&self) -> &Arc<InProcessDaemon> {
        &self.shared.daemon
    }

    /// Subscribes to `query`'s result set and returns it, and whether it is still at `since`.
    async fn join(self: &Arc<Self>, query: QueryId, since: Option<u64>) -> Result<(watch::Receiver<Arc<ResultSet>>, bool), Failure> {
        let _serial = self.joins.lock().await;
        {
            let mut relay = self.relay.lock().expect("query feed relay lock poisoned");
            if relay.is_none() {
                // Subscribe to events before the daemon replays, so none falls between the two.
                let events = self.shared.daemon.subscribe();
                *relay = Some(tokio::spawn(Arc::clone(&self.shared).relay(events)));
            }
        }
        {
            let mut entries = self.shared.entries();
            if let Some(Entry::Live { set, users }) = entries.get_mut(&query) {
                *users += 1;
                let receiver = set.subscribe();
                let current = since == Some(receiver.borrow().seq);
                return Ok((receiver, current));
            }
            entries.insert(query.clone(), Entry::Joining(Vec::new()));
        }
        let mut cursors = self.shared.cursors();
        cursors.push(QueryCursor { query: query.clone(), since });
        let joined = self.fetch(&query, since, &cursors).await;
        let (mut held, current) = match joined {
            Ok(joined) => joined,
            Err(error) => {
                self.shared.entries().remove(&query);
                self.resubscribe().await;
                return Err(error);
            }
        };
        let gap = {
            let mut entries = self.shared.entries();
            let Some(Entry::Joining(queued)) = entries.remove(&query) else { unreachable!("joins are serialized") };
            let mut gap = None;
            for update in queued {
                if let Err(seq) = apply(&mut held, update) {
                    gap = Some(QueryCursor { query: query.clone(), since: Some(seq) });
                }
            }
            let (set, receiver) = watch::channel(Arc::new(held));
            entries.insert(query, Entry::Live { set, users: 1 });
            (receiver, gap)
        };
        let (receiver, gap) = gap;
        if let Some(gap) = gap {
            self.shared.recover(vec![gap]).await;
        }
        Ok((receiver, current))
    }

    /// The daemon's result set for `query`. A cursor the daemon finds current gets no replay;
    /// its rows are then read without changing the subscription.
    async fn fetch(&self, query: &QueryId, since: Option<u64>, cursors: &[QueryCursor]) -> Result<(ResultSet, bool), Failure> {
        let daemon = &self.shared.daemon;
        let mut fresh = None;
        for event in daemon.subscribe_queries(self.shared.subscriber, cursors).await.map_err(Failure::from)? {
            if let DaemonEvent::ResultSet(set) = event {
                if set.query() == *query {
                    fresh = Some(*set);
                } else {
                    self.shared.offer(Update::Set(*set));
                }
            }
        }
        if let Some(set) = fresh {
            return Ok((set, false));
        }
        let recovered = daemon.recover_queries(&[QueryCursor { query: query.clone(), since: None }]).await.map_err(Failure::from)?;
        let set = recovered
            .into_iter()
            .find_map(|event| match event {
                DaemonEvent::ResultSet(set) => Some(*set),
                _ => None,
            })
            .ok_or_else(|| bad("table_source", format!("query is not materialized: {query}")))?;
        let current = since == Some(set.seq);
        Ok((set, current))
    }

    async fn leave(&self, query: &QueryId) {
        let _serial = self.joins.lock().await;
        {
            let mut entries = self.shared.entries();
            match entries.get_mut(query) {
                Some(Entry::Live { users, .. }) if *users > 1 => {
                    *users -= 1;
                    return;
                }
                Some(Entry::Live { .. }) => {
                    entries.remove(query);
                }
                _ => return,
            }
        }
        self.resubscribe().await;
    }

    /// Narrows the daemon's subscription to the held queries, releasing demand for the rest.
    async fn resubscribe(&self) {
        let cursors = self.shared.cursors();
        if cursors.is_empty() {
            self.shared.daemon.unsubscribe_queries(self.shared.subscriber).await;
            return;
        }
        match self.shared.daemon.subscribe_queries(self.shared.subscriber, &cursors).await {
            Ok(events) => {
                for event in events {
                    if let DaemonEvent::ResultSet(set) = event {
                        self.shared.offer(Update::Set(*set));
                    }
                }
            }
            Err(error) => warn!(%error, subscriber = %self.shared.subscriber, "named-query table resubscription failed"),
        }
    }

    /// Whether a table read on this feed holds `query`.
    pub(crate) fn holds(&self, query: &QueryId) -> bool {
        matches!(self.shared.entries().get(query), Some(Entry::Live { .. }))
    }

    /// Ends the subscriber: no held query, no demand.
    pub(crate) async fn close(&self) {
        if let Some(relay) = self.relay.lock().expect("query feed relay lock poisoned").take() {
            relay.abort();
        }
        self.shared.entries().clear();
        self.shared.daemon.unsubscribe_queries(self.shared.subscriber).await;
    }
}

impl Drop for QueryFeed {
    fn drop(&mut self) {
        if let Some(relay) = self.relay.get_mut().expect("query feed relay lock poisoned").take() {
            relay.abort();
        }
    }
}

/// A joined query, released when the last table read holding it ends.
struct Lease {
    feed: Arc<QueryFeed>,
    query: QueryId,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let (feed, query) = (Arc::clone(&self.feed), self.query.clone());
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { feed.leave(&query).await });
        }
    }
}

#[derive(Clone)]
struct Joined {
    set: watch::Receiver<Arc<ResultSet>>,
    current: bool,
    _lease: Arc<Lease>,
}

/// One table of a named query's description, read through a feed.
#[derive(Clone)]
pub(crate) struct Source {
    feed: Arc<QueryFeed>,
    family: &'static Family,
    query: Option<QueryId>,
    table: String,
    joined: Option<Joined>,
}

impl Source {
    pub(super) fn open(table: &str, feed: Option<&Arc<QueryFeed>>) -> Option<Self> {
        let family = family(table)?;
        let feed = Arc::clone(feed?);
        Some(Self { feed, family, query: family.default.clone(), table: table.to_owned(), joined: None })
    }

    pub(super) fn name(&self) -> &str {
        &self.table
    }

    pub(super) fn incarnation(&self) -> &str {
        self.feed.incarnation()
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

    /// Joins the query; `since` is a resume sequence of this feed's incarnation.
    pub(super) async fn join(&mut self, since: Option<u64>) -> Result<(), Failure> {
        if self.joined.is_some() {
            return Ok(());
        }
        let query = self.query.clone().ok_or_else(|| bad("invalid_request", format!("table {} needs a query", self.table)))?;
        let (set, current) = self.feed.join(query.clone(), since).await?;
        self.joined = Some(Joined { set, current, _lease: Arc::new(Lease { feed: Arc::clone(&self.feed), query }) });
        Ok(())
    }

    /// Whether the joined set was still at the resume sequence it was joined with.
    pub(super) fn resumed(&self) -> bool {
        self.joined.as_ref().is_some_and(|joined| joined.current)
    }

    /// The current rows of this table and the result set's sequence.
    pub(super) fn rows(&self) -> Result<(Vec<Row>, u64, String), Failure> {
        let joined = self.joined.as_ref().ok_or_else(|| bad("table_source", "named query is not joined"))?;
        let set = Arc::clone(&joined.set.borrow());
        let mut tables = set.rows.tables(&self.family.set.tables[0].name);
        Ok((tables.remove(&self.table).unwrap_or_default(), set.seq, self.feed.incarnation.clone()))
    }

    pub(super) fn events(&self) -> BoxStream<'static, Result<(), String>> {
        let Some(joined) = &self.joined else { return futures::stream::once(async { Err("named query is not joined".into()) }).boxed() };
        let mut set = joined.set.clone();
        set.mark_unchanged();
        futures::stream::unfold(Some(set), |set| async move {
            let mut set = set?;
            match set.changed().await {
                Ok(()) => Some((Ok(()), Some(set))),
                Err(_) => Some((Err("named-query feed ended".into()), None)),
            }
        })
        .boxed()
    }
}
