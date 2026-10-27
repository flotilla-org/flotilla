#[path = "message_query.rs"]
mod message_query;
use self::message_query::MessageQuerySql;
use crate::load_metrics::StoreLoadMetrics;
use crate::Store;
#[cfg(test)]
use crate::StoreExt;
use crate::{convert_read, ReadObserver, ReadOperation, ResourceKind, SerializedResource};
use crate::{watch, DigestQuery, MessageQuery, PartitionDigest, ResourcePosition};
use crate::{DecodedList, DecodedObject, DecodedReadObject};
use crate::{SerializedReadWatchEvent, SerializedWatchEvent, SerializedWatchStream, WatchAction, WatchObject, WatchPayload};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use flotilla_resources::{Convoy, ReadResourceObject, ReplicaCursor, Resource, ResourceProvenance};
use flotilla_resources::{ResourceList, ResourceTombstone, WatchEvent, WatchStart};
use futures::{stream, StreamExt};
use rusqlite::{params, Connection as RusqliteConnection, OpenFlags, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_rusqlite::Connection;

use crate::{
    error::ResourceError,
    replica::{StoredReplicaEvent, StoredReplicaEventKind},
    resource::{InputMeta, MergeMetadata, ObjectMeta, ResourceObject},
    retention::{
        EventRetention, ResourceDecodeQuarantine, ResourceEventDecodeQuarantine, ResourceStoreDiagnostics,
        FIELD_OWNERSHIP_VIOLATION_TTL_HOURS, MAX_FIELD_OWNERSHIP_VIOLATIONS,
    },
    watch::{decode_watch_tombstone, WatchChannel},
    FieldOwnershipViolation,
};

#[path = "sqlite/digest.rs"]
mod digest;
#[cfg(test)]
#[path = "sqlite/query_projection_tests.rs"]
mod query_projection_tests;
#[path = "sqlite/statements.rs"]
pub(crate) mod statements;
#[cfg(test)]
#[path = "sqlite/watch_replay_tests.rs"]
mod watch_replay_tests;

use statements::{CachedSql, ConnectionMeter, PrepareCounters};

#[derive(Clone)]
enum QueryColumns {
    Identity,
    Names,
    LocalList,
    ReadRows,
    Field(String),
}

/// Decodes one stored body for a read. It is the only pass over the body and
/// doubles as the read's quarantine check.
type RowDecode<R> = fn(ResourceKind, &str) -> Result<R, ResourceError>;

/// The two shapes a read hands back: the validated stored tree for pass-through
/// callers (replication, resource HTTP), or the kind's registered type.
struct RowForm;
impl RowForm {
    fn serialized(kind: ResourceKind, body: &str) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        kind.decode_object_text(body)
    }
    fn typed(kind: ResourceKind, body: &str) -> Result<DecodedObject, ResourceError> {
        kind.decode_typed_text(body)
    }
}

type StoreKey = (String, String, String, String);
type WatchersByStore = HashMap<StoreKey, WatchChannel<StoredEvent>>;
type ReplicaWatchersByStore = HashMap<StoreKey, WatchChannel<StoredReplicaEvent>>;
type CleanupKey = (StoreKey, &'static str);

const REPLICA_DELETIONS_SELECT_SQL: &str = "SELECT origin_root, name, last_synced_at FROM replica_tombstones WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 ORDER BY origin_root, name";

const READER_COUNT: usize = 4;
// Bump when metadata or declared-field extraction semantics change.
const QUERY_PROJECTION_SCHEMA_VERSION: &str = "query-projections-v1";
const READ_DEADLINE: Duration = Duration::from_secs(2);
const READ_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const SLOW_STORE_OPERATION: Duration = Duration::from_millis(100);

// Statements shared by several write paths keep one text, so they share one
// cached compilation per connection.
const REPLICA_OBJECT_UPSERT_SQL: &str = "INSERT INTO replica_objects (origin_root, group_name, version, kind, namespace, name, body_json, last_synced_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(origin_root, group_name, version, kind, namespace, name) DO UPDATE SET body_json = excluded.body_json, last_synced_at = excluded.last_synced_at";
const REPLICA_OBJECT_DELETE_SQL: &str = "DELETE FROM replica_objects WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND name = ?6";
const REPLICA_TOMBSTONE_UPSERT_SQL: &str = "INSERT INTO replica_tombstones (origin_root, group_name, version, kind, namespace, name, last_synced_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(origin_root, group_name, version, kind, namespace, name) DO UPDATE SET last_synced_at = excluded.last_synced_at";
const RESOURCE_OBJECT_BODY_SQL: &str =
    "SELECT body_json FROM resource_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5";
const REPLICA_TOMBSTONE_CLEAR_SQL: &str = "DELETE FROM replica_tombstones WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND name = ?6";
const RESOURCE_OBJECT_DELETE_SQL: &str =
    "DELETE FROM resource_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5";
const REPLICA_PARTITION_DELETE_SQL: [&str; 3] = [
    "DELETE FROM replica_objects WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5",
    "DELETE FROM replica_tombstones WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5",
    "DELETE FROM replica_cursors WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5",
];

#[derive(bon::Builder)]
struct ReplicaMutation {
    kind: StoredReplicaEventKind,
    name: String,
    value: Value,
    synced_at: DateTime<Utc>,
}

#[derive(Debug, Clone, bon::Builder)]
#[builder(builder_type(vis = "pub(in crate::sqlite)"))]
pub struct SqliteBackend {
    connection: Connection,
    read_connections: Vec<Connection>,
    // Index-aligned with `connection` and `read_connections`.
    writer_meter: Arc<ConnectionMeter>,
    reader_meters: Vec<Arc<ConnectionMeter>>,
    reader_slots: Arc<tokio::sync::Semaphore>,
    available_readers: Arc<Mutex<Vec<usize>>>,
    discovery_slots: Arc<tokio::sync::Semaphore>,
    discovery_readers: Arc<Mutex<Vec<usize>>>,
    writer_depth: Arc<AtomicUsize>,
    read_depth: Arc<AtomicUsize>,
    read_warnings: Arc<Mutex<ReadWarnings>>,
    // Mutations notify, and watches read the stream's current version and
    // register, on the writer thread. A watch therefore receives live exactly
    // the events committed after the version it registered at; its replay up
    // to that version runs on a reader (`watch_events`).
    watchers: Arc<Mutex<WatchersByStore>>,
    replica_watchers: Arc<Mutex<ReplicaWatchersByStore>>,
    pending_cleanups: Arc<Mutex<HashSet<CleanupKey>>>,
    event_retention: EventRetention,
    local_root: Option<NodeId>,
    read_observer: Option<Arc<dyn ReadObserver>>,
    // Serialize cross-kind Project/FleetDesignation admission across clones.
    pub(crate) hierarchy_admission: Arc<tokio::sync::Mutex<()>>,
    #[builder(default)]
    replay_probe: ReplayProbe,
    /// Queue and execution timings for load measurement (#3206).
    #[builder(default)]
    load_metrics: Arc<StoreLoadMetrics>,
}

// The worker owns leases and depth claims, including after a caller disappears.
struct ReaderLease {
    index: usize,
    available: Arc<Mutex<Vec<usize>>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl Drop for ReaderLease {
    fn drop(&mut self) {
        if let Ok(mut available) = self.available.lock() {
            available.push(self.index);
        }
    }
}
struct ReadCaller(Arc<AtomicBool>);
impl Drop for ReadCaller {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
struct DepthClaim {
    depth: Arc<AtomicUsize>,
}
impl DepthClaim {
    fn new(depth: Arc<AtomicUsize>) -> Self {
        depth.fetch_add(1, Ordering::Relaxed);
        Self { depth }
    }
}
impl Drop for DepthClaim {
    fn drop(&mut self) {
        self.depth.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
struct StoredEvent {
    kind: StoredEventKind,
    object: Value,
}

struct CleanupClaim {
    key: CleanupKey,
    pending: Arc<Mutex<HashSet<CleanupKey>>>,
}

impl Drop for CleanupClaim {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.key);
        }
    }
}

#[derive(Debug)]
enum ReplicaApplyError {
    Rejected(ResourceError),
    Storage(ResourceError),
}

/// Events a reader replayed for a watch, and the stored events it could not
/// decode. Readers are read-only, so the writer persists those failures.
#[derive(Default)]
struct ReplayedEvents {
    events: Vec<SerializedWatchEvent>,
    failures: Vec<ReplayFailure>,
}

struct ReplayFailure {
    event_version: u64,
    name: String,
    body_json: String,
    error: String,
}

/// Which part of a read the reader deadline bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadDeadline {
    /// Queue wait and execution: a point read or list that overruns is shed.
    Total,
    /// Queue wait only. Watch replay is bounded by event retention, and once it
    /// runs its result is the cheapest way to resume; failing it after the work
    /// is done would only send the watcher to a more expensive relist.
    Queue,
}

/// Test pauses around watch replay, so tests can commit writes at exact points
/// between subscriber registration and the reader's replay. Empty outside tests.
#[derive(Clone, Default)]
struct ReplayProbe {
    /// Awaited after the subscriber registered on the writer and before the
    /// replay is queued on a reader.
    #[cfg(test)]
    registered: Option<Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>>,
    /// Called on the reader thread once its replay snapshot is fixed and before
    /// it reads the events.
    #[cfg(test)]
    in_snapshot: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for ReplayProbe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ReplayProbe")
    }
}

impl ReplayProbe {
    async fn registered(&self) {
        #[cfg(test)]
        if let Some(pause) = &self.registered {
            pause().await;
        }
    }

    fn in_snapshot(&self) {
        #[cfg(test)]
        if let Some(pause) = &self.in_snapshot {
            pause();
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum StoredEventKind {
    Added,
    Modified,
    Deleted,
}

impl StoredEventKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Added => "ADDED",
            Self::Modified => "MODIFIED",
            Self::Deleted => "DELETED",
        }
    }

    fn from_str(value: &str) -> Result<Self, ResourceError> {
        match value {
            "ADDED" => Ok(Self::Added),
            "MODIFIED" => Ok(Self::Modified),
            "DELETED" => Ok(Self::Deleted),
            other => Err(ResourceError::decode(format!("unknown stored event type '{other}'"))),
        }
    }
}

// Per-backend aggregate: pressure cannot turn every refused/slow read into a
// file write. Counts include suppressed warnings and reset only on publication.
#[derive(Debug, Default)]
struct ReadWarnings {
    last: Option<Instant>,
    slow: u64,
    abandoned: u64,
    deadlines: u64,
}
impl ReadWarnings {
    fn record(&mut self, slow: u64, abandoned: u64, deadlines: u64, now: Instant) -> bool {
        self.slow += slow;
        self.abandoned += abandoned;
        self.deadlines += deadlines;
        if self.last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(30)) {
            return false;
        }
        self.last = Some(now);
        true
    }
    fn warn(shared: &Mutex<Self>, slow: u64, abandoned: u64, deadlines: u64, operation: &'static str) {
        let Ok(mut state) = shared.lock() else { return };
        if state.record(slow, abandoned, deadlines, Instant::now()) {
            tracing::warn!(
                operation,
                slow_reads = state.slow,
                abandoned_reads = state.abandoned,
                read_deadlines = state.deadlines,
                "sqlite read pressure summary (at most every 30s)"
            );
            state.slow = 0;
            state.abandoned = 0;
            state.deadlines = 0;
        }
    }
}

impl SqliteBackend {
    fn read_namespace_inventory(connection: &mut RusqliteConnection) -> Result<BTreeMap<String, Vec<String>>, ResourceError> {
        let mut inventory = crate::declared_resource_kinds()
            .into_iter()
            .map(|kind| (kind.paths.plural.to_string(), Vec::new()))
            .collect::<BTreeMap<_, _>>();
        let kinds = crate::declared_resource_kinds();
        let mut statement = connection
            .prepare_cached(
                "SELECT group_name, version, kind, namespace FROM resource_objects
                 UNION SELECT group_name, version, kind, namespace FROM replica_objects
                 ORDER BY namespace",
            )
            .map_err(|err| Self::map_sqlite(err, "prepare namespace inventory"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            })
            .map_err(|err| Self::map_sqlite(err, "query namespace inventory"))?;
        for row in rows {
            let (group, version, kind, namespace) = row.map_err(|err| Self::map_sqlite(err, "read namespace inventory"))?;
            if let Some(descriptor) = kinds
                .iter()
                .find(|descriptor| descriptor.paths.group == group && descriptor.paths.version == version && descriptor.paths.kind == kind)
            {
                inventory.get_mut(descriptor.paths.plural).expect("registered kind").push(namespace);
            }
        }
        Ok(inventory)
    }

    #[cfg(test)]
    pub(crate) fn writer_for_test(&self) -> Connection {
        self.connection.clone()
    }

    /// Statement compiles and executed operations per connection since open.
    ///
    /// Take a snapshot before and after an operation and compare them with
    /// [`PrepareCounters::since`]. Zero `compile_events` means the operation
    /// reused cached statements only; `operations` shows which connections
    /// ran it.
    pub fn prepare_counters(&self) -> PrepareCounters {
        PrepareCounters { writer: self.writer_meter.counters(), readers: self.reader_meters.iter().map(|meter| meter.counters()).collect() }
    }

    pub fn with_read_observer(mut self, observer: std::sync::Arc<dyn ReadObserver>) -> Self {
        self.read_observer = Some(observer);
        self
    }
    fn observe(&self, resource_kind: ResourceKind, operation: ReadOperation, objects: usize) {
        if let Some(observer) = &self.read_observer {
            observer.read(resource_kind.paths.kind, operation, objects);
        }
    }

    pub fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.read_observer.as_deref()
    }

    pub async fn local_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        let group = resource_kind.paths.group.to_string();
        let version = resource_kind.paths.version.to_string();
        let kind = resource_kind.paths.kind.to_string();
        self.read_call("list namespaces", move |connection| {
            let mut statement = connection
                .prepare_cached(
                    "SELECT DISTINCT namespace FROM resource_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3 ORDER BY namespace",
                )
                .map_err(|error| Self::map_sqlite(error, "prepare sqlite local namespace list"))?;
            let rows = statement
                .query_map(params![group, version, kind], |row| row.get::<_, String>(0))
                .map_err(|error| Self::map_sqlite(error, "query sqlite local namespace list"))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|error| Self::map_sqlite(error, "read sqlite local namespace list"))
        })
        .await
    }

    pub async fn stored_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        let group = resource_kind.paths.group.to_string();
        let version = resource_kind.paths.version.to_string();
        let kind = resource_kind.paths.kind.to_string();
        self.read_call("list all namespaces", move |connection| {
            let mut statement = connection
                .prepare_cached(
                    "SELECT namespace FROM resource_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3
                     UNION SELECT namespace FROM replica_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3
                     ORDER BY namespace",
                )
                .map_err(|error| Self::map_sqlite(error, "prepare stored namespace list"))?;
            let rows = statement
                .query_map(params![group, version, kind], |row| row.get::<_, String>(0))
                .map_err(|error| Self::map_sqlite(error, "query stored namespace list"))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|error| Self::map_sqlite(error, "read stored namespace list"))
        })
        .await
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, ResourceError> {
        Self::open_with_event_retention(path, EventRetention::default())
    }

    pub fn open_with_event_retention(path: impl AsRef<Path>, event_retention: EventRetention) -> Result<Self, ResourceError> {
        let connection =
            RusqliteConnection::open(&path).map_err(|err| ResourceError::other(format!("open sqlite resource store: {err}")))?;
        let mut backend = Self::from_connection(connection, event_retention)?;
        let started = Instant::now();
        for _ in 0..READER_COUNT {
            let read = RusqliteConnection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|err| ResourceError::other(format!("open sqlite resource reader: {err}")))?;
            backend
                .reader_meters
                .push(statements::configure(&read).map_err(|err| Self::map_sqlite(err, "configure sqlite reader statements"))?);
            Self::register_digest_functions(&read)?;
            Self::register_query_functions(&read)?;
            read.busy_timeout(READ_BUSY_TIMEOUT).map_err(|err| Self::map_sqlite(err, "configure reader timeout"))?;
            backend.read_connections.push(read.into());
        }
        Self::log_open_step("reader_opens", started, None);
        backend.reader_slots = Arc::new(tokio::sync::Semaphore::new(READER_COUNT - 1));
        backend.available_readers = Arc::new(Mutex::new((0..READER_COUNT - 1).collect()));
        backend.discovery_readers = Arc::new(Mutex::new(vec![READER_COUNT - 1]));
        Ok(backend)
    }

    pub async fn open_async(path: impl AsRef<Path>) -> Result<Self, ResourceError> {
        let connection = Connection::open(&path).await.map_err(|err| ResourceError::other(format!("open sqlite resource store: {err}")))?;
        let mut backend = Self::from_async_connection(connection, EventRetention::default()).await?;
        let started = Instant::now();
        for _ in 0..READER_COUNT {
            let read = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .await
                .map_err(|err| ResourceError::other(format!("open sqlite resource reader: {err}")))?;
            let meter = read
                .call(|connection| {
                    let meter =
                        statements::configure(connection).map_err(|err| Self::map_sqlite(err, "configure sqlite reader statements"))?;
                    Self::register_digest_functions(connection)?;
                    Self::register_query_functions(connection)?;
                    connection.busy_timeout(READ_BUSY_TIMEOUT).map_err(|err| Self::map_sqlite(err, "configure reader timeout"))?;
                    Ok(meter)
                })
                .await
                .map_err(Self::map_connection_error)?;
            backend.read_connections.push(read);
            backend.reader_meters.push(meter);
        }
        Self::log_open_step("reader_opens", started, None);
        backend.reader_slots = Arc::new(tokio::sync::Semaphore::new(READER_COUNT - 1));
        backend.available_readers = Arc::new(Mutex::new((0..READER_COUNT - 1).collect()));
        backend.discovery_readers = Arc::new(Mutex::new(vec![READER_COUNT - 1]));
        Ok(backend)
    }

    pub fn open_in_memory() -> Result<Self, ResourceError> {
        Self::open_in_memory_with_event_retention(EventRetention::default())
    }

    pub fn open_in_memory_with_event_retention(event_retention: EventRetention) -> Result<Self, ResourceError> {
        let connection =
            RusqliteConnection::open_in_memory().map_err(|err| ResourceError::other(format!("open sqlite resource store: {err}")))?;
        Self::from_connection(connection, event_retention)
    }

    fn from_connection(mut connection: RusqliteConnection, event_retention: EventRetention) -> Result<Self, ResourceError> {
        let writer_meter = statements::configure(&connection).map_err(|err| Self::map_sqlite(err, "configure sqlite writer statements"))?;
        Self::initialize_connection(&mut connection, event_retention)?;
        Ok(Self {
            connection: connection.into(),
            read_connections: Vec::new(),
            writer_meter,
            reader_meters: Vec::new(),
            reader_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            available_readers: Arc::new(Mutex::new(vec![0])),
            discovery_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            discovery_readers: Arc::new(Mutex::new(vec![0])),
            writer_depth: Arc::default(),
            read_depth: Arc::default(),
            read_warnings: Arc::default(),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            replica_watchers: Arc::new(Mutex::new(HashMap::new())),
            pending_cleanups: Arc::new(Mutex::new(HashSet::new())),
            event_retention,
            local_root: None,
            read_observer: None,
            hierarchy_admission: Arc::default(),
            replay_probe: ReplayProbe::default(),
            load_metrics: Arc::default(),
        })
    }

    async fn from_async_connection(connection: Connection, event_retention: EventRetention) -> Result<Self, ResourceError> {
        let writer_meter = connection
            .call(move |connection| {
                let meter = statements::configure(connection).map_err(|err| Self::map_sqlite(err, "configure sqlite writer statements"))?;
                Self::initialize_connection(connection, event_retention)?;
                Ok(meter)
            })
            .await
            .map_err(Self::map_connection_error)?;
        Ok(Self {
            connection,
            read_connections: Vec::new(),
            writer_meter,
            reader_meters: Vec::new(),
            reader_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            available_readers: Arc::new(Mutex::new(vec![0])),
            discovery_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            discovery_readers: Arc::new(Mutex::new(vec![0])),
            writer_depth: Arc::default(),
            read_depth: Arc::default(),
            read_warnings: Arc::default(),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            replica_watchers: Arc::new(Mutex::new(HashMap::new())),
            pending_cleanups: Arc::new(Mutex::new(HashSet::new())),
            event_retention,
            local_root: None,
            read_observer: None,
            hierarchy_admission: Arc::default(),
            replay_probe: ReplayProbe::default(),
            load_metrics: Arc::default(),
        })
    }

    pub(crate) fn with_local_root(mut self, local_root: NodeId) -> Self {
        self.local_root = Some(local_root);
        self
    }

    pub(crate) fn local_root(&self) -> NodeId {
        self.local_root.clone().unwrap_or_else(|| NodeId::new("local"))
    }

    /// Register the connection-local projection used by native TEMP triggers.
    /// Persisted schema never requires this function. Unconfigured writers may
    /// leave derived values stale; the next native open repairs them.
    pub fn register_query_functions(connection: &RusqliteConnection) -> Result<(), ResourceError> {
        connection
            .create_scalar_function(
                "flotilla_declared_values",
                2,
                rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC | rusqlite::functions::FunctionFlags::SQLITE_UTF8,
                |context| {
                    let body: String = context.get(0)?;
                    let field: String = context.get(1)?;
                    let values = serde_json::from_str::<Value>(&body)
                        .ok()
                        .map(|value| crate::declared_field_values(&value, &field))
                        .unwrap_or_default();
                    Ok(serde_json::json!(values).to_string())
                },
            )
            .map_err(|error| Self::map_sqlite(error, "register declared field projection"))?;
        Ok(())
    }

    // Issue 3197 requires every phase in normal startup logs, including fast
    // phases, so missed health windows can be attributed after the fact.
    fn log_open_step(step: &'static str, started: Instant, repaired: Option<bool>) {
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(repaired) = repaired {
            tracing::info!(step, elapsed_ms, repaired, "sqlite store open step completed");
        } else {
            tracing::info!(step, elapsed_ms, "sqlite store open step completed");
        }
    }

    fn initialize_connection(connection: &mut RusqliteConnection, event_retention: EventRetention) -> Result<(), ResourceError> {
        let started = Instant::now();
        Self::register_query_functions(connection)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|err| ResourceError::other(format!("configure sqlite resource store busy timeout: {err}")))?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|err| ResourceError::other(format!("configure sqlite resource store WAL mode: {err}")))?;
        connection
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS resource_sequences (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    next_version INTEGER NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace)
                );

                CREATE TABLE IF NOT EXISTS resource_objects (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    name TEXT NOT NULL,
                    resource_version INTEGER NOT NULL,
                    body_json TEXT NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace, name)
                );

                CREATE TABLE IF NOT EXISTS resource_events (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    event_version INTEGER NOT NULL,
                    event_type TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace, event_version)
                );

                CREATE TABLE IF NOT EXISTS replica_apply_quarantine (
                    origin_root TEXT NOT NULL,
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    name TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    last_synced_at TEXT NOT NULL,
                    error TEXT NOT NULL,
                    quarantined_at TEXT NOT NULL,
                    PRIMARY KEY (origin_root, group_name, version, kind, namespace, name)
                );

                CREATE TABLE IF NOT EXISTS resource_decode_quarantine (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    name TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    error TEXT NOT NULL,
                    quarantined_at TEXT NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace, name)
                );

                CREATE TABLE IF NOT EXISTS resource_event_compaction (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    compacted_through INTEGER NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace)
                );

                CREATE TABLE IF NOT EXISTS resource_event_decode_quarantine (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    event_version INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    error TEXT NOT NULL,
                    quarantined_at TEXT NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace, event_version)
                );

                CREATE TABLE IF NOT EXISTS field_ownership_violations (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    body_json TEXT NOT NULL,
                    observed_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS replica_objects (
                    origin_root TEXT NOT NULL,
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    name TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    last_synced_at TEXT NOT NULL,
                    PRIMARY KEY (origin_root, group_name, version, kind, namespace, name)
                );

                CREATE INDEX IF NOT EXISTS replica_objects_by_resource_name
                    ON replica_objects (group_name, version, kind, namespace, name, origin_root);

                CREATE INDEX IF NOT EXISTS resource_objects_message_active ON resource_objects (group_name,version,kind,namespace,json_extract(body_json, '$.spec.receiver'),name) WHERE kind='Message' AND COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') NOT IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered');
                CREATE INDEX IF NOT EXISTS resource_objects_message_reply ON resource_objects (group_name,version,kind,namespace,json_extract(body_json, '$.spec.in_reply_to'),name) WHERE kind='Message';
                CREATE INDEX IF NOT EXISTS resource_objects_message_batch ON resource_objects (group_name,version,kind,namespace,json_extract(body_json, '$.status.submission.batch_id'),name) WHERE kind='Message';
                CREATE INDEX IF NOT EXISTS replica_objects_message_active ON replica_objects (group_name,version,kind,namespace,json_extract(body_json, '$.spec.receiver'),name) WHERE kind='Message' AND COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') NOT IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered');
                CREATE INDEX IF NOT EXISTS replica_objects_message_reply ON replica_objects (group_name,version,kind,namespace,json_extract(body_json, '$.spec.in_reply_to'),name) WHERE kind='Message';
                CREATE INDEX IF NOT EXISTS replica_objects_message_batch ON replica_objects (group_name,version,kind,namespace,json_extract(body_json, '$.status.submission.batch_id'),name) WHERE kind='Message';
                CREATE INDEX IF NOT EXISTS message_audit_age ON resource_objects (group_name,version,kind,namespace,julianday(json_extract(body_json, '$.status.since'))) WHERE kind='Message' AND COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered') AND json_extract(body_json, '$.spec.body_digest') IS NULL;
                CREATE INDEX IF NOT EXISTS resource_objects_message_inbox ON resource_objects (group_name,version,kind,namespace,name) WHERE kind='Message' AND COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') NOT IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered');
                CREATE INDEX IF NOT EXISTS replica_objects_message_inbox ON replica_objects (group_name,version,kind,namespace,name) WHERE kind='Message' AND COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') NOT IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered');
                CREATE INDEX IF NOT EXISTS replica_message_audit_age ON replica_objects (group_name,version,kind,namespace,julianday(json_extract(body_json, '$.status.since'))) WHERE kind='Message' AND COALESCE(json_extract(body_json, '$.status.phase'), 'accepted') IN ('satisfied','answered','outcome_met','expired','superseded','dead_lettered') AND json_extract(body_json, '$.spec.body_digest') IS NULL;
                CREATE TABLE IF NOT EXISTS replica_cursors (
                    origin_root TEXT NOT NULL,
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    resource_version TEXT NOT NULL,
                    generation TEXT,
                    last_synced_at TEXT NOT NULL,
                    PRIMARY KEY (origin_root, group_name, version, kind, namespace)
                );

                CREATE TABLE IF NOT EXISTS replica_tombstones (
                    origin_root TEXT NOT NULL,
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    name TEXT NOT NULL,
                    last_synced_at TEXT NOT NULL,
                    PRIMARY KEY (origin_root, group_name, version, kind, namespace, name)
                );

                CREATE INDEX IF NOT EXISTS replica_tombstones_relay_snapshot
                    ON replica_tombstones (group_name, version, kind, namespace, origin_root, name, last_synced_at);

                CREATE TABLE IF NOT EXISTS resource_tombstones (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    name TEXT NOT NULL,
                    body_json TEXT NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace, name)
                );

                CREATE TABLE IF NOT EXISTS resource_store_migrations (
                    name TEXT PRIMARY KEY
                );
                "#,
            )
            .map_err(|err| ResourceError::other(format!("initialize sqlite resource store: {err}")))?;
        let has_prefix = connection
            .prepare("PRAGMA table_info(replica_cursors)")
            .map_err(|err| Self::map_sqlite(err, "inspect replica prefix schema"))?
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|err| Self::map_sqlite(err, "read replica prefix schema"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| Self::map_sqlite(err, "decode replica prefix schema"))?
            .iter()
            .any(|name| name == "complete_prefix");
        if !has_prefix {
            // Old cursors could be advanced by relays or skipped decode failures.
            // Do not trust them until an authoritative snapshot establishes a prefix.
            connection
                .execute("ALTER TABLE replica_cursors ADD COLUMN complete_prefix INTEGER NOT NULL DEFAULT 0", [])
                .map_err(|err| Self::map_sqlite(err, "add replica prefix proof"))?;
        }
        let workflow_templates_reset = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM resource_store_migrations WHERE name = 'workflow-template-definitions-v1')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|err| Self::map_sqlite(err, "inspect workflow template definitions migration"))?;
        if !workflow_templates_reset {
            let transaction =
                connection.transaction().map_err(|err| Self::map_sqlite(err, "start workflow template definitions migration"))?;
            for table in [
                "resource_objects",
                "resource_events",
                "resource_decode_quarantine",
                "resource_tombstones",
                "replica_objects",
                "replica_cursors",
                "replica_tombstones",
            ] {
                transaction
                    .execute(
                        &format!("DELETE FROM {table} WHERE group_name = 'flotilla.work' AND version = 'v1' AND kind = 'WorkflowTemplate'"),
                        [],
                    )
                    .map_err(|err| Self::map_sqlite(err, "wipe pre-definitions workflow templates"))?;
            }
            transaction
                .execute("INSERT INTO resource_store_migrations (name) VALUES ('workflow-template-definitions-v1')", [])
                .map_err(|err| Self::map_sqlite(err, "record workflow template definitions migration"))?;
            transaction.commit().map_err(|err| Self::map_sqlite(err, "commit workflow template definitions migration"))?;
        }
        let has_replica_object_sync_timestamp = {
            let mut statement = connection
                .prepare("PRAGMA table_info(replica_objects)")
                .map_err(|err| Self::map_sqlite(err, "inspect sqlite replica object schema"))?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|err| Self::map_sqlite(err, "query sqlite replica object schema"))?;
            columns
                .collect::<Result<Vec<_>, _>>()
                .map_err(|err| Self::map_sqlite(err, "read sqlite replica object schema"))?
                .iter()
                .any(|column| column == "last_synced_at")
        };
        if !has_replica_object_sync_timestamp {
            connection
                .execute_batch(
                    r#"
                    ALTER TABLE replica_objects ADD COLUMN last_synced_at TEXT;
                    UPDATE replica_objects
                    SET last_synced_at = (
                        SELECT c.last_synced_at
                        FROM replica_cursors c
                        WHERE c.origin_root = replica_objects.origin_root
                          AND c.group_name = replica_objects.group_name
                          AND c.version = replica_objects.version
                          AND c.kind = replica_objects.kind
                          AND c.namespace = replica_objects.namespace
                    );
                    "#,
                )
                .map_err(|err| Self::map_sqlite(err, "migrate sqlite replica object timestamps"))?;
        }
        Self::log_open_step("schema", started, None);
        let started = Instant::now();
        Self::initialize_query_indexes(connection)?;
        Self::log_open_step("query_indexes", started, None);
        let started = Instant::now();
        let startup_diagnostics = Self::diagnostics_from_connection(connection, event_retention)?;
        Self::log_open_step("diagnostics", started, None);
        if !startup_diagnostics.warnings.is_empty() {
            tracing::warn!(
                event_count = startup_diagnostics.event_count,
                object_count = startup_diagnostics.object_count,
                resource_stream_count = startup_diagnostics.resource_stream_count,
                max_retained_events = startup_diagnostics.max_retained_events,
                warnings = ?startup_diagnostics.warnings,
                "resource event log tripwire triggered on startup; compacting",
            );
        }
        let started = Instant::now();
        Self::compact_existing_events(connection, event_retention)?;
        Self::log_open_step("compact_existing_events", started, None);
        let started = Instant::now();
        Self::initialize_digests(connection)?;
        Self::log_open_step("initialize_digests", started, None);
        Ok(())
    }

    /// Versioned, transactional materialized-view repair with a per-write native
    /// proof. See `docs/implementation-choices/sqlite-query-projection-validity.md`.
    fn initialize_query_indexes(connection: &mut rusqlite::Connection) -> Result<(), ResourceError> {
        let tx = connection.transaction().map_err(|e| Self::map_sqlite(e, "begin query indexes"))?;
        let started = Instant::now();
        // Field additions/removals and kind identity changes are detected automatically.
        // A Debug formatting change only causes a benign one-off projection repair.
        let mut declarations = crate::declared_resource_kinds()
            .into_iter()
            .map(|kind| format!("{}/{}/{}:{:?}", kind.paths.group, kind.paths.version, kind.paths.kind, kind.queryable_fields))
            .collect::<Vec<_>>();
        declarations.sort();
        let schema = format!("{:x}", Sha256::digest(format!("{QUERY_PROJECTION_SCHEMA_VERSION}:{declarations:?}")));
        tx.execute_batch("CREATE TABLE IF NOT EXISTS resource_query_projection (singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS resource_query_native_write (singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema TEXT NOT NULL);")
            .map_err(|e| Self::map_sqlite(e, "create query projection marker"))?;
        let initialized = tx
            .query_row("SELECT schema FROM resource_query_projection WHERE singleton=1", [], |row| row.get::<_, String>(0))
            .optional()
            .map_err(|e| Self::map_sqlite(e, "read query projection marker"))?;
        let repair = initialized.as_deref() != Some(&schema);
        for table in ["resource_objects", "replica_objects"] {
            for action in ["INSERT", "UPDATE", "DELETE"] {
                if repair {
                    tx.execute_batch(&format!("DROP TRIGGER IF EXISTS main.{table}_query_dirty_{action};"))
                        .map_err(|e| Self::map_sqlite(e, "upgrade query projection invalidation"))?;
                }
                // BEFORE and AFTER phases give an ordering guarantee independent
                // of SQLite's ordering of triggers within each phase. A native
                // write supplies a one-row proof; every writer consumes it.
                // Both proof and invalidation roll back with the object mutation.
                tx.execute_batch(&format!(
                    "CREATE TEMP TRIGGER IF NOT EXISTS {table}_query_native_{action} BEFORE {action} ON main.{table}
                    BEGIN INSERT INTO resource_query_native_write(singleton,schema) VALUES (1,'{schema}')
                    ON CONFLICT(singleton) DO UPDATE SET schema=excluded.schema; END;
                    CREATE TRIGGER IF NOT EXISTS {table}_query_dirty_{action} AFTER {action} ON {table}
                    BEGIN DELETE FROM resource_query_projection WHERE NOT EXISTS (
                        SELECT 1 FROM resource_query_native_write n WHERE n.singleton=1 AND n.schema=resource_query_projection.schema);
                    DELETE FROM resource_query_native_write; END;"
                ))
                .map_err(|e| Self::map_sqlite(e, "install query projection invalidation"))?;
            }
        }
        for (migration, table, origin) in
            [("resource-labels-v1", "resource_objects", "''"), ("replica-labels-v1", "replica_objects", "origin_root")]
        {
            let done: bool = tx
                .query_row("SELECT EXISTS(SELECT 1 FROM resource_store_migrations WHERE name=?1)", [migration], |r| r.get(0))
                .map_err(|e| Self::map_sqlite(e, "inspect query migration"))?;
            let labels = format!("{table}_labels");
            let owners = format!("{table}_owners");
            let identity = "group_name,version,kind,namespace,name,origin";
            if !done {
                tx.execute_batch(&format!("
                    CREATE TABLE {labels} (group_name TEXT, version TEXT, kind TEXT, namespace TEXT, name TEXT, origin TEXT, key TEXT, value TEXT, PRIMARY KEY ({identity},key));
                    CREATE INDEX {labels}_lookup ON {labels} (group_name,version,kind,namespace,key,value,name,origin);
                    CREATE TABLE {owners} (group_name TEXT, version TEXT, kind TEXT, namespace TEXT, name TEXT, origin TEXT, owner_key TEXT, PRIMARY KEY ({identity},owner_key));
                    CREATE INDEX {owners}_lookup ON {owners} (group_name,version,kind,namespace,owner_key,name,origin);
                ")).map_err(|e| Self::map_sqlite(e, "create metadata query indexes"))?;
                tx.execute("INSERT INTO resource_store_migrations(name) VALUES (?1)", [migration])
                    .map_err(|e| Self::map_sqlite(e, "record query migration"))?;
            }
            let new_origin: String = if origin == "''" { "''".into() } else { format!("NEW.{origin}") };
            let old_origin: String = if origin == "''" { "''".into() } else { format!("OLD.{origin}") };
            let delete_identity = format!("group_name=OLD.group_name AND version=OLD.version AND kind=OLD.kind AND namespace=OLD.namespace AND name=OLD.name AND origin={old_origin}");
            let new_identity = format!("NEW.group_name,NEW.version,NEW.kind,NEW.namespace,NEW.name,{new_origin}");
            let body = "CASE WHEN json_valid(NEW.body_json) THEN NEW.body_json ELSE '{}' END";
            // Labels decode as a map: if legacy JSON repeats a key, its last
            // value wins. SQLite's single max aggregate selects that row's value.
            let label_values = format!(
                "SELECT key,value FROM (SELECT key,value,max(id) AS last_id FROM json_each({body},'$.metadata.labels') GROUP BY key)"
            );
            // Owner membership ignores controller flags. Deduplicate the projected
            // key, not the body, before an outer UPSERT can override OR REPLACE.
            let insert = format!("
                INSERT INTO {labels} SELECT {new_identity},key,value FROM ({label_values});
                INSERT INTO {owners} SELECT DISTINCT {new_identity},json_array(json_extract(value,'$.apiVersion'),json_extract(value,'$.kind'),json_extract(value,'$.name')) FROM json_each({body},'$.metadata.ownerReferences');
            ");
            if repair {
                for action in ["insert", "update", "delete"] {
                    tx.execute_batch(&format!("DROP TRIGGER IF EXISTS main.{table}_query_{action};"))
                        .map_err(|e| Self::map_sqlite(e, "replace metadata query trigger"))?;
                }
            }
            tx.execute_batch(&format!("
                CREATE TRIGGER IF NOT EXISTS {table}_query_insert AFTER INSERT ON {table} BEGIN {insert} END;
                CREATE TRIGGER IF NOT EXISTS {table}_query_update AFTER UPDATE ON {table} BEGIN
                    DELETE FROM {labels} WHERE {delete_identity}; DELETE FROM {owners} WHERE {delete_identity}; {insert}
                END;
                CREATE TRIGGER IF NOT EXISTS {table}_query_delete AFTER DELETE ON {table} BEGIN DELETE FROM {labels} WHERE {delete_identity}; DELETE FROM {owners} WHERE {delete_identity}; END;
            ")).map_err(|e| Self::map_sqlite(e, "install metadata query triggers"))?;
            if repair || !done {
                tx.execute_batch(&format!("
                DELETE FROM {labels}; DELETE FROM {owners};
                INSERT INTO {labels} SELECT group_name,version,kind,namespace,name,origin,key,value FROM (
                    SELECT o.group_name,o.version,o.kind,o.namespace,o.name,{origin} AS origin,j.key,j.value,max(j.id) AS last_id
                    FROM {table} o,json_each(CASE WHEN json_valid(o.body_json) THEN o.body_json ELSE '{{}}' END,'$.metadata.labels') j
                    GROUP BY o.group_name,o.version,o.kind,o.namespace,o.name,{origin},j.key
                );
                INSERT INTO {owners} SELECT DISTINCT o.group_name,o.version,o.kind,o.namespace,o.name,{origin},json_array(json_extract(j.value,'$.apiVersion'),json_extract(j.value,'$.kind'),json_extract(j.value,'$.name')) FROM {table} o,json_each(CASE WHEN json_valid(o.body_json) THEN o.body_json ELSE '{{}}' END,'$.metadata.ownerReferences') j;
            ")).map_err(|e| Self::map_sqlite(e, "repair metadata query indexes"))?;
            }
        }
        Self::log_open_step("query_indexes_metadata", started, Some(repair));
        let started = Instant::now();
        // Pre-release query schemas used persistent function-dependent objects.
        // Remove those automatically before installing connection-local triggers.
        let obsolete = {
            let mut statement = tx.prepare("SELECT type,name FROM main.sqlite_schema WHERE type IN ('trigger','index') AND instr(sql,'flotilla_declared_values')>0")
                .map_err(|e| Self::map_sqlite(e, "inspect obsolete query schema"))?;
            let objects = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                .map_err(|e| Self::map_sqlite(e, "list obsolete query schema"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| Self::map_sqlite(e, "read obsolete query schema"))?;
            objects
        };
        for (object_type, name) in obsolete {
            let quoted = name.replace('"', "\"\"");
            tx.execute_batch(&format!("DROP {object_type} main.\"{quoted}\""))
                .map_err(|e| Self::map_sqlite(e, "remove obsolete function-dependent schema"))?;
            if object_type == "index" {
                tx.execute("DELETE FROM resource_store_migrations WHERE name=?1", [&name])
                    .map_err(|e| Self::map_sqlite(e, "remove obsolete query migration marker"))?;
            }
        }
        for kind in crate::declared_resource_kinds() {
            let names = kind
                .queryable_fields
                .iter()
                .map(|path| Self::field_index("resource_objects", kind, path))
                .collect::<std::collections::BTreeSet<_>>();
            debug_assert_eq!(names.len(), kind.queryable_fields.len(), "declared field index names must be unique");
            for path in kind.queryable_fields {
                for table in ["resource_objects", "replica_objects"] {
                    let index = Self::field_index(table, kind, path);
                    let values = format!("{index}_values");
                    let done: bool = tx
                        .query_row("SELECT EXISTS(SELECT 1 FROM resource_store_migrations WHERE name=?1)", [&index], |row| row.get(0))
                        .map_err(|e| Self::map_sqlite(e, "inspect field migration"))?;
                    if !done {
                        tx.execute_batch(&format!("CREATE TABLE {values} (group_name TEXT,version TEXT,kind TEXT,namespace TEXT,name TEXT,origin TEXT,value TEXT,PRIMARY KEY(group_name,version,kind,namespace,name,origin,value)); CREATE INDEX {index} ON {values}(group_name,version,kind,namespace,value,name,origin);"))
                            .map_err(|e| Self::map_sqlite(e,"create field values index"))?;
                        tx.execute("INSERT INTO resource_store_migrations(name) VALUES (?1)", [&index])
                            .map_err(|e| Self::map_sqlite(e, "record field migration"))?;
                    }
                    let origin = if table == "replica_objects" { "origin_root" } else { "''" };
                    let new_origin = if table == "replica_objects" { "NEW.origin_root" } else { "''" };
                    let old_origin = if table == "replica_objects" { "OLD.origin_root" } else { "''" };
                    let delete=format!("DELETE FROM {values} WHERE group_name=OLD.group_name AND version=OLD.version AND kind=OLD.kind AND namespace=OLD.namespace AND name=OLD.name AND origin={old_origin};");
                    // Outer UPSERT conflict policies can override trigger OR IGNORE.
                    // Extract a set before insertion, including startup repair below.
                    let insert=format!("INSERT OR IGNORE INTO {values} SELECT DISTINCT NEW.group_name,NEW.version,NEW.kind,NEW.namespace,NEW.name,{new_origin},j.value FROM json_each(flotilla_declared_values(NEW.body_json,'{path}')) j WHERE j.type='text';");
                    // TEMP maintenance is transactional but never travels with the file.
                    for action in ["insert", "update", "delete"] {
                        tx.execute_batch(&format!("DROP TRIGGER IF EXISTS main.{index}_{action};"))
                            .map_err(|e| Self::map_sqlite(e, "remove legacy field trigger"))?;
                    }
                    tx.execute_batch(&format!("
                        CREATE TEMP TRIGGER IF NOT EXISTS {index}_insert AFTER INSERT ON main.{table} WHEN NEW.kind='{}' BEGIN {insert} END;
                        CREATE TEMP TRIGGER IF NOT EXISTS {index}_update AFTER UPDATE ON main.{table} WHEN NEW.kind='{}' BEGIN {delete} {insert} END;
                        CREATE TEMP TRIGGER IF NOT EXISTS {index}_delete AFTER DELETE ON main.{table} WHEN OLD.kind='{}' BEGIN {delete} END;
                    ",kind.paths.kind,kind.paths.kind,kind.paths.kind)).map_err(|e| Self::map_sqlite(e,"install temporary field triggers"))?;
                    if repair || !done {
                        tx.execute_batch(&format!("DELETE FROM {values}; INSERT OR IGNORE INTO {values} SELECT DISTINCT o.group_name,o.version,o.kind,o.namespace,o.name,{origin},j.value FROM main.{table} o,json_each(flotilla_declared_values(o.body_json,'{path}')) j WHERE o.kind='{}' AND j.type='text';",kind.paths.kind))
                        .map_err(|e| Self::map_sqlite(e,"repair declared field values"))?;
                    }
                }
            }
        }
        if repair {
            tx.execute("INSERT INTO resource_query_projection(singleton,schema) VALUES (1,?1) ON CONFLICT(singleton) DO UPDATE SET schema=excluded.schema", [&schema])
                .map_err(|e| Self::map_sqlite(e, "record query projection schema"))?;
        }
        tx.commit().map_err(|e| Self::map_sqlite(e, "commit query indexes"))?;
        Self::log_open_step("query_indexes_fields", started, Some(repair));
        Ok(())
    }
    fn field_index(table: &str, kind: ResourceKind, path: &str) -> String {
        format!(
            "{}_{}_{}{}",
            table,
            kind.paths.kind,
            path.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>(),
            // Existing scalar migrations used raw json_extract expressions.
            // A distinct additive migration is required for canonical/null semantics.
            if path.contains("[].") {
                "_collection_v2"
            } else if crate::array_field_path(path).is_some() {
                ""
            } else {
                "_scalar_v3"
            }
        )
    }
    // Generated SQL contains only static declared paths and bound user values;
    // never inline a literal containing a ?<digits> token into this SQL.
    fn shift_bindings(sql: &str, offset: usize) -> String {
        let mut shifted = String::new();
        let mut chars = sql.chars().peekable();
        let mut quote = None;
        let mut largest_binding = 0usize;
        while let Some(c) = chars.next() {
            shifted.push(c);
            if let Some(delimiter) = quote {
                if c == delimiter {
                    if chars.peek() == Some(&delimiter) {
                        shifted.push(chars.next().expect("peeked quote"));
                    } else {
                        quote = None;
                    }
                }
            } else if matches!(c, '\'' | '"' | '`' | '[') {
                quote = Some(if c == '[' { ']' } else { c });
            } else if c == '?' {
                let mut digits = String::new();
                while chars.peek().is_some_and(char::is_ascii_digit) {
                    digits.push(chars.next().expect("peeked digit"));
                }
                let binding = if digits.is_empty() { largest_binding.checked_add(1) } else { digits.parse::<usize>().ok() };
                if let Some(binding) = binding {
                    largest_binding = largest_binding.max(binding);
                    if let Some(number) = binding.checked_add(offset) {
                        shifted.push_str(&number.to_string());
                        continue;
                    }
                }
                // Invalid/overflowed bindings are left for SQLite to reject.
                shifted.push_str(&digits);
            }
        }
        shifted
    }
    fn shift_binds(sql: &str, offset: usize) -> String {
        Self::shift_bindings(sql, offset)
    }
    fn declared_sql(
        kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
        replicas: bool,
        root: &str,
        columns: QueryColumns,
    ) -> (String, Vec<rusqlite::types::Value>) {
        // HTTP query_raw and native entry points validate before SQL assembly.
        // Keep this assertion before hydration/recursive branches as well.
        debug_assert!(query.validate(kind).is_ok());
        if query.all_sources_for_matches {
            let selection = crate::StoreQuery { all_sources_for_matches: false, ..query.clone() };
            let outer = crate::StoreQuery { include_replicas: true, suppress_self_origin: false, ..Default::default() };
            let (_, mut args) = Self::declared_sql_with_names(kind, namespace, &outer, replicas, root, columns.clone(), None);
            let mut branches = Vec::new();
            for origin in [false, true] {
                let (branch, values) = Self::declared_sql(kind, namespace, &selection, origin, root, QueryColumns::Names);
                branches.push(format!("SELECT name FROM ({})", Self::shift_bindings(&branch, args.len())));
                args.extend(values);
            }
            let names = branches.join(" UNION ");
            let (sql, _) = Self::declared_sql_with_names(kind, namespace, &outer, replicas, root, columns, Some(&names));
            return (sql, args);
        }
        Self::declared_sql_with_names(kind, namespace, query, replicas, root, columns, None)
    }
    #[allow(clippy::too_many_arguments)]
    fn declared_sql_with_names(
        kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
        replicas: bool,
        root: &str,
        columns: QueryColumns,
        names: Option<&str>,
    ) -> (String, Vec<rusqlite::types::Value>) {
        // Identifiers come from registered static declarations; wire values are bound.
        use rusqlite::types::Value as SqlValue;
        let table = if replicas { "replica_objects" } else { "resource_objects" };
        let mut args = vec![
            SqlValue::Text(kind.paths.group.into()),
            SqlValue::Text(kind.paths.version.into()),
            SqlValue::Text(kind.paths.kind.into()),
            SqlValue::Text(namespace.into()),
        ];
        let mut predicates =
            vec!["o.group_name=?1 AND o.version=?2 AND o.kind=?3 AND o.namespace=?4".to_string(), format!("o.kind='{}'", kind.paths.kind)];
        if let Some(names) = names {
            predicates.push(format!("o.name IN ({names})"));
        }
        for (label, value) in &query.labels {
            args.push(SqlValue::Text(label.clone()));
            let key = args.len();
            args.push(SqlValue::Text(value.clone()));
            let val = args.len();
            let (candidate, columns, origin) =
                if replicas { ("(o.name,o.origin_root)", "l.name,l.origin", "") } else { ("o.name", "l.name", " AND l.origin='' ") };
            predicates.push(format!("{candidate} IN (SELECT {columns} FROM {table}_labels l INDEXED BY {table}_labels_lookup WHERE l.group_name=?1 AND l.version=?2 AND l.kind=?3 AND l.namespace=?4 AND l.key=?{key} AND l.value=?{val}{origin})"));
        }
        if !query.fields.is_empty()
            || !query.field_values.is_empty()
            || !query.field_prefixes.is_empty()
            || !query.present.is_empty()
            || !query.label_prefixes.is_empty()
            || !query.missing_fields.is_empty()
            || !query.field_matches_query.is_empty()
        {
            predicates.push("json_valid(o.body_json)".into());
        }
        let field_expression = |_path: &str| "l.value".to_string();
        let field_predicate = |path: &str, predicate: String| {
            let index = Self::field_index(table, kind, path);
            let candidate = if replicas { "(o.name,o.origin_root)" } else { "o.name" };
            let columns = if replicas { "l.name,l.origin" } else { "l.name" };
            format!("{candidate} IN (SELECT {columns} FROM {index}_values l INDEXED BY {index} WHERE l.group_name=?1 AND l.version=?2 AND l.kind=?3 AND l.namespace=?4 AND {predicate})")
        };
        for (path, value) in &query.fields {
            args.push(SqlValue::Text(value.clone()));
            predicates.push(field_predicate(path, format!("{}=?{}", field_expression(path), args.len())));
        }
        for (path, values) in &query.field_values {
            args.push(SqlValue::Text(serde_json::json!(values).to_string()));
            predicates.push(field_predicate(path, format!("{} IN (SELECT value FROM json_each(?{}))", field_expression(path), args.len())));
        }
        for (path, prefix) in &query.field_prefixes {
            // A range plus an exact prefix check, like label prefixes. Under the
            // query planner stability guarantee a bound GLOB pattern cannot
            // become an index range, but `value>=?` always can. Value tables
            // hold only TEXT (`j.type='text'`), so the comparison never meets a
            // numeric value that GLOB would have coerced.
            args.push(SqlValue::Text(prefix.clone()));
            let n = args.len();
            let value = field_expression(path);
            predicates.push(field_predicate(path, format!("{value}>=?{n} AND substr({value},1,length(?{n}))=?{n}")));
        }
        for path in &query.present {
            predicates.push(field_predicate(path, format!("{} IS NOT NULL", field_expression(path))));
        }
        for (label, prefix) in &query.label_prefixes {
            args.push(SqlValue::Text(label.clone()));
            let key = args.len();
            args.push(SqlValue::Text(prefix.clone()));
            let n = args.len();
            let identity = if replicas { "(o.name,o.origin_root)" } else { "o.name" };
            let projection = if replicas { "name,origin" } else { "name" };
            predicates.push(format!("{identity} IN (SELECT {projection} FROM {table}_labels INDEXED BY {table}_labels_lookup WHERE group_name=?1 AND version=?2 AND kind=?3 AND namespace=?4 AND key=?{key} AND value>=?{n} AND substr(value,1,length(?{n}))=?{n})"));
        }
        for path in &query.missing_fields {
            let index = Self::field_index(table, kind, path);
            let origin = if replicas { "o.origin_root" } else { "''" };
            predicates.push(format!("NOT EXISTS(SELECT 1 FROM {index}_values l WHERE l.group_name=o.group_name AND l.version=o.version AND l.kind=o.kind AND l.namespace=o.namespace AND l.name=o.name AND l.origin={origin})"));
        }
        for (path, selection) in &query.field_matches_query {
            let source_kind = selection.kind(kind).expect("validated source kind");
            let mut source = (*selection.query).clone();
            if let Some(scope) = query.replica_shadowing.as_ref().filter(|_| source_kind.paths.kind == kind.paths.kind) {
                source.replica_shadowing = Some(scope.clone());
                source.replicas_only = query.replicas_only;
            }
            let mut projections = Vec::new();
            for source_replicas in [false, true] {
                if (!source_replicas && source.replicas_only)
                    || (source_replicas && (!source.include_replicas || source_kind.replication == crate::ReplicationClass::None))
                {
                    continue;
                }
                let (sql, values) = Self::declared_sql(
                    source_kind,
                    namespace,
                    &source,
                    source_replicas,
                    root,
                    QueryColumns::Field(selection.field.clone()),
                );
                projections.push(format!("SELECT * FROM ({})", Self::shift_binds(&sql, args.len())));
                args.extend(values);
            }
            let sql = if projections.is_empty() { "SELECT NULL WHERE 0".into() } else { projections.join(" UNION ") };
            predicates.push(field_predicate(path, format!("{} IN ({sql})", field_expression(path))));
        }
        if let Some(queries) = &query.any_of {
            let mut alternatives = Vec::new();
            for alternative in queries {
                let mut alternative = alternative.clone();
                alternative.replica_shadowing = query.replica_shadowing.clone();
                alternative.suppress_self_origin = query.suppress_self_origin;
                let (sql, values) = Self::declared_sql(kind, namespace, &alternative, replicas, root, QueryColumns::Identity);
                let shifted = Self::shift_binds(&sql, args.len());
                alternatives.push(format!("SELECT * FROM ({shifted})"));
                args.extend(values);
            }
            let candidate = if replicas { "(o.name,o.origin_root)" } else { "o.name" };
            if alternatives.is_empty() {
                predicates.push("0".into());
            } else {
                predicates.push(format!("{candidate} IN ({})", alternatives.join(" UNION ")));
            }
        }
        if let Some(owner) = &query.owner {
            args.push(SqlValue::Text(owner.key()));
            let (candidate, columns, origin) =
                if replicas { ("(o.name,o.origin_root)", "l.name,l.origin", "") } else { ("o.name", "l.name", " AND l.origin='' ") };
            predicates.push(format!("{candidate} IN (SELECT {columns} FROM {table}_owners l INDEXED BY {table}_owners_lookup WHERE l.group_name=?1 AND l.version=?2 AND l.kind=?3 AND l.namespace=?4 AND l.owner_key=?{}{origin})",args.len()));
        }
        if replicas && query.suppress_self_origin {
            if let Some(scope) = &query.replica_shadowing {
                args.push(SqlValue::Text(scope.origin_root.to_string()));
                let root_arg = args.len();
                args.push(SqlValue::Text(serde_json::json!(scope.local_names).to_string()));
                predicates.push(format!("NOT (o.origin_root=?{root_arg} AND o.name IN (SELECT value FROM json_each(?{})))", args.len()));
            } else {
                args.push(SqlValue::Text(root.into()));
                predicates.push(format!("NOT (o.origin_root=?{} AND EXISTS(SELECT 1 FROM resource_objects local WHERE local.group_name=o.group_name AND local.version=o.version AND local.kind=o.kind AND local.namespace=o.namespace AND local.name=o.name))",args.len()));
            }
        }
        // Specialized Message indexes are kept as the established exemplar.
        let index = if let Some(message) = &query.message {
            let (predicate, argument) = message.sql_at(args.len() + 1);
            args.push(argument.map(SqlValue::Text).unwrap_or(SqlValue::Null));
            predicates.push(predicate);
            predicates.push(format!("(?{} IS NULL OR ?{} IS NOT NULL)", args.len(), args.len()));
            Some(message.sql_index(replicas).into())
        } else {
            None
        };
        let indexed = index.map(|index: String| format!(" INDEXED BY {index}")).unwrap_or_default();
        if let QueryColumns::Field(path) = &columns {
            let index = Self::field_index(table, kind, path);
            let (selection, values) = Self::declared_sql(kind, namespace, query, replicas, root, QueryColumns::Identity);
            let candidate = if replicas { "(l.name,l.origin)" } else { "l.name" };
            return (format!("SELECT l.value FROM {index}_values l INDEXED BY {index} WHERE l.group_name=?1 AND l.version=?2 AND l.kind=?3 AND l.namespace=?4 AND {candidate} IN ({selection})"),values);
        }
        let projection = match columns {
            QueryColumns::Names => "name",
            QueryColumns::Identity if replicas => "name,origin_root",
            QueryColumns::Identity => "name",
            QueryColumns::Field(_) => unreachable!("field projections return from their indexed relation"),
            QueryColumns::LocalList => "name,body_json",
            QueryColumns::ReadRows if replicas => "body_json,origin_root,last_synced_at",
            QueryColumns::ReadRows => "body_json,NULL,NULL",
        };
        (format!("SELECT {projection} FROM {table} o{indexed} WHERE {} ORDER BY name", predicates.join(" AND ")), args)
    }

    fn store_key(resource_kind: ResourceKind, namespace: &str) -> StoreKey {
        (
            resource_kind.paths.group.to_string(),
            resource_kind.paths.version.to_string(),
            resource_kind.paths.kind.to_string(),
            namespace.to_string(),
        )
    }

    /// Writer and reader timings since this store opened (#3206).
    pub fn load_metrics(&self) -> Arc<StoreLoadMetrics> {
        Arc::clone(&self.load_metrics)
    }

    fn lock_watchers(watchers: &Mutex<WatchersByStore>) -> Result<MutexGuard<'_, WatchersByStore>, ResourceError> {
        watchers.lock().map_err(|_| ResourceError::other("sqlite resource watch lock poisoned"))
    }

    async fn call<R, F>(&self, operation_name: &'static str, operation: F) -> Result<R, ResourceError>
    where
        R: Send + 'static,
        F: FnOnce(&mut RusqliteConnection) -> Result<R, ResourceError> + Send + 'static,
    {
        let started = Instant::now();
        let depth = DepthClaim::new(self.writer_depth.clone());
        let meter = Arc::clone(&self.writer_meter);
        let load_metrics = Arc::clone(&self.load_metrics);
        self.connection
            .call(move |connection| {
                let queue_wait = started.elapsed();
                let queue_depth = depth.depth.load(Ordering::Relaxed).saturating_sub(1);
                drop(depth);
                meter.record_operation();
                load_metrics.write_wait.record(queue_wait);
                load_metrics.write_queue_peak.observe(queue_depth as u64);
                let execution = Instant::now();
                let result = operation(connection);
                load_metrics.write_execution.record(execution.elapsed());
                if started.elapsed() >= SLOW_STORE_OPERATION {
                    tracing::warn!(
                        operation = operation_name,
                        queue_wait_ms = queue_wait.as_millis(),
                        execution_ms = execution.elapsed().as_millis(),
                        queue_depth,
                        "slow sqlite store operation"
                    );
                }
                result
            })
            .await
            .map_err(Self::map_connection_error)
    }

    async fn read_call<R, F>(&self, operation_name: &'static str, operation: F) -> Result<R, ResourceError>
    where
        R: Send + 'static,
        F: FnOnce(&mut RusqliteConnection) -> Result<R, ResourceError> + Send + 'static,
    {
        self.read_call_with_pool(operation_name, operation, false, ReadDeadline::Total).await
    }

    async fn read_call_with_pool<R, F>(
        &self,
        operation_name: &'static str,
        operation: F,
        discovery: bool,
        deadline: ReadDeadline,
    ) -> Result<R, ResourceError>
    where
        R: Send + 'static,
        F: FnOnce(&mut RusqliteConnection) -> Result<R, ResourceError> + Send + 'static,
    {
        let started = Instant::now();
        let alive = Arc::new(AtomicBool::new(true));
        let _caller = ReadCaller(alive.clone());
        let depth = DepthClaim::new(self.read_depth.clone());
        let execution_started = Arc::new(Mutex::new(None::<Instant>));
        let worker_execution_started = execution_started.clone();
        // In-memory SQLite cannot use separate WAL readers: its one private
        // connection uses the shared lease and the same shedding guard.
        // Reserve the fourth WAL worker for inventory: discovery cannot queue
        // behind either resource reads or the replication writer.
        let (slots, available) = if discovery && !self.read_connections.is_empty() {
            (&self.discovery_slots, &self.discovery_readers)
        } else {
            (&self.reader_slots, &self.available_readers)
        };
        let read = async {
            let permit = slots.clone().acquire_owned().await.map_err(|_| ResourceError::other("sqlite reader pool closed"))?;
            let index = available
                .lock()
                .map_err(|_| ResourceError::other("sqlite reader pool poisoned"))?
                .pop()
                .ok_or_else(|| ResourceError::other("sqlite reader pool empty"))?;
            let lease = ReaderLease { index, available: available.clone(), _permit: permit };
            let connection = self.read_connections.get(index).unwrap_or(&self.connection);
            let meter = Arc::clone(self.reader_meters.get(index).unwrap_or(&self.writer_meter));
            let load_metrics = Arc::clone(&self.load_metrics);
            let read_warnings = self.read_warnings.clone();
            connection
                .call(move |connection| {
                    let _lease = lease;
                    let queue_wait = started.elapsed();
                    let queue_depth = depth.depth.load(Ordering::Relaxed).saturating_sub(1);
                    drop(depth);
                    load_metrics.read_wait.record(queue_wait);
                    load_metrics.read_queue_peak.observe(queue_depth as u64);
                    if !alive.load(Ordering::Acquire) || queue_wait >= READ_DEADLINE {
                        load_metrics.reads_shed.increment();
                        if queue_wait >= SLOW_STORE_OPERATION {
                            ReadWarnings::warn(&read_warnings, 0, 1, 0, operation_name);
                        }
                        return Err(ResourceError::store_busy_read_deadline());
                    }
                    meter.record_operation();
                    let execution = Instant::now();
                    if let Ok(mut timing) = worker_execution_started.lock() {
                        *timing = Some(execution);
                    }
                    let result = operation(connection);
                    load_metrics.read_execution.record(execution.elapsed());
                    if started.elapsed() >= SLOW_STORE_OPERATION {
                        ReadWarnings::warn(&read_warnings, 1, 0, 0, operation_name);
                    }
                    result
                })
                .await
                .map_err(Self::map_connection_error)
        };
        tokio::pin!(read);
        // A queue deadline lets a read that has started run to completion. A
        // poisoned timing lock reads as "not started", which deliberately falls
        // back to the total deadline rather than waiting without a bound.
        let result = match tokio::time::timeout(READ_DEADLINE, &mut read).await {
            Err(_) if deadline == ReadDeadline::Queue && execution_started.lock().ok().and_then(|timing| *timing).is_some() => {
                Ok(read.await)
            }
            result => result,
        };
        match result {
            Ok(result) => {
                if result.as_ref().is_err_and(|error| error.is_store_busy()) {
                    self.load_metrics.read_contention.increment();
                }
                result
            }
            Err(_) => {
                self.load_metrics.read_contention.increment();
                ReadWarnings::warn(&self.read_warnings, 0, 0, 1, operation_name);
                Err(ResourceError::store_busy_read_deadline())
            }
        }
    }

    fn cleanup_call<F>(&self, store_key: StoreKey, operation_name: &'static str, operation: F)
    where
        F: FnOnce(&mut RusqliteConnection) -> Result<(), ResourceError> + Send + 'static,
    {
        let key = (store_key, operation_name);
        let Ok(mut pending) = self.pending_cleanups.lock() else {
            tracing::warn!(operation = operation_name, "sqlite cleanup queue lock poisoned");
            return;
        };
        if !pending.insert(key.clone()) {
            // A pending cleanup will recheck the row body before changing it.
            // A later read can schedule another pass after that task finishes.
            return;
        }
        drop(pending);
        let claim = CleanupClaim { key, pending: Arc::clone(&self.pending_cleanups) };
        let backend = self.clone();
        // Cleanup uses the writer, which may be waiting on fsync. Do not hold
        // up a successful read while the writer drains its queue.
        tokio::spawn(async move {
            let _claim = claim;
            if let Err(error) = backend.call(operation_name, operation).await {
                tracing::warn!(operation = operation_name, %error, "sqlite store cleanup failed");
            }
        });
    }

    fn map_connection_error(error: tokio_rusqlite::Error<ResourceError>) -> ResourceError {
        match error {
            tokio_rusqlite::Error::Error(error) => error,
            other => ResourceError::other(format!("sqlite connection thread failed: {other}")),
        }
    }

    pub async fn diagnostics(&self) -> Result<ResourceStoreDiagnostics, ResourceError> {
        let event_retention = self.event_retention;
        self.call("read diagnostics", move |connection| Self::diagnostics_from_connection(connection, event_retention)).await
    }

    fn diagnostics_from_connection(
        connection: &RusqliteConnection,
        event_retention: EventRetention,
    ) -> Result<ResourceStoreDiagnostics, ResourceError> {
        let object_count = connection
            .query_row_cached("SELECT COUNT(*) FROM resource_objects", [], |row| row.get::<_, u64>(0))
            .map_err(|err| Self::map_sqlite(err, "count sqlite resource objects"))?;
        let event_count = connection
            .query_row_cached("SELECT COUNT(*) FROM resource_events", [], |row| row.get::<_, u64>(0))
            .map_err(|err| Self::map_sqlite(err, "count sqlite resource events"))?;
        let resource_stream_count = connection
            .query_row_cached("SELECT COUNT(*) FROM resource_sequences", [], |row| row.get::<_, u64>(0))
            .map_err(|err| Self::map_sqlite(err, "count sqlite resource streams"))?;
        let mut statement = connection
            .prepare_cached(
                r#"
                SELECT kind, namespace, name, error, quarantined_at
                FROM resource_decode_quarantine
                ORDER BY kind, namespace, name
                "#,
            )
            .map_err(|err| Self::map_sqlite(err, "prepare sqlite resource decode quarantine diagnostics"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .map_err(|err| Self::map_sqlite(err, "query sqlite resource decode quarantine diagnostics"))?;
        let decode_quarantines = rows
            .map(|row| {
                let (resource_kind, namespace, name, error, quarantined_at) =
                    row.map_err(|err| Self::map_sqlite(err, "read sqlite resource decode quarantine diagnostic"))?;
                let quarantined_at = DateTime::parse_from_rfc3339(&quarantined_at)
                    .map_err(|err| ResourceError::decode(format!("decode resource quarantine timestamp: {err}")))?
                    .with_timezone(&Utc);
                Ok(ResourceDecodeQuarantine { kind: resource_kind, namespace, name, error, quarantined_at })
            })
            .collect::<Result<Vec<_>, ResourceError>>()?;
        let mut diagnostics = ResourceStoreDiagnostics::new(object_count, event_count, resource_stream_count, event_retention);
        diagnostics.decode_quarantines = decode_quarantines;
        let mut statement = connection.prepare("SELECT origin_root,kind,namespace,name,error,quarantined_at FROM replica_apply_quarantine ORDER BY origin_root,kind,namespace,name")
            .map_err(|err| Self::map_sqlite(err, "prepare replica quarantine diagnostics"))?;
        diagnostics.replica_quarantines = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|err| Self::map_sqlite(err, "query replica quarantine diagnostics"))?
            .map(|row| {
                let (origin, kind, namespace, name, error, at) =
                    row.map_err(|err| Self::map_sqlite(err, "read replica quarantine diagnostic"))?;
                let quarantined_at =
                    DateTime::parse_from_rfc3339(&at).map_err(|err| ResourceError::decode(err.to_string()))?.with_timezone(&Utc);
                Ok(flotilla_resources::ResourceReplicaQuarantine {
                    origin_root: NodeId::new(origin),
                    kind,
                    namespace,
                    name,
                    error,
                    quarantined_at,
                })
            })
            .collect::<Result<Vec<_>, ResourceError>>()?;

        let mut statement = connection
            .prepare_cached(
                r#"
                SELECT kind, namespace, name, event_version, error, quarantined_at
                FROM resource_event_decode_quarantine
                ORDER BY kind, namespace, event_version
                "#,
            )
            .map_err(|err| Self::map_sqlite(err, "prepare sqlite resource event decode quarantine diagnostics"))?;
        diagnostics.event_decode_quarantines = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|err| Self::map_sqlite(err, "query sqlite resource event decode quarantine diagnostics"))?
            .map(|row| {
                let (resource_kind, namespace, name, event_version, error, quarantined_at) =
                    row.map_err(|err| Self::map_sqlite(err, "read sqlite resource event decode quarantine diagnostic"))?;
                let quarantined_at = DateTime::parse_from_rfc3339(&quarantined_at)
                    .map_err(|err| ResourceError::decode(format!("decode resource event quarantine timestamp: {err}")))?
                    .with_timezone(&Utc);
                Ok(ResourceEventDecodeQuarantine { kind: resource_kind, namespace, name, event_version, error, quarantined_at })
            })
            .collect::<Result<Vec<_>, ResourceError>>()?;
        let cutoff = (Utc::now() - chrono::Duration::hours(FIELD_OWNERSHIP_VIOLATION_TTL_HOURS)).to_rfc3339();
        connection
            .execute_cached("DELETE FROM field_ownership_violations WHERE unixepoch(observed_at) < unixepoch(?1)", [cutoff])
            .map_err(|err| Self::map_sqlite(err, "expire field ownership violations"))?;
        let mut statement = connection
            .prepare_cached("SELECT body_json FROM field_ownership_violations ORDER BY id")
            .map_err(|err| Self::map_sqlite(err, "prepare field ownership violation diagnostics"))?;
        diagnostics.field_ownership_violations = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|err| Self::map_sqlite(err, "query field ownership violation diagnostics"))?
            .map(|row| {
                let body = row.map_err(|err| Self::map_sqlite(err, "read field ownership violation diagnostic"))?;
                serde_json::from_str(&body)
                    .map_err(|err| ResourceError::decode(format!("decode field ownership violation diagnostic: {err}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(diagnostics)
    }

    pub async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        let body = serde_json::to_string(&violation)
            .map_err(|error| ResourceError::decode(format!("encode field ownership violation: {error}")))?;
        let observed_at = violation.observed_at.to_rfc3339();
        self.call("record field ownership violation", move |connection| {
            connection
                .execute_cached(
                    "INSERT INTO field_ownership_violations (body_json, observed_at) VALUES (?1, ?2)",
                    rusqlite::params![body, observed_at],
                )
                .map_err(|error| Self::map_sqlite(error, "record field ownership violation"))?;
            connection
                .execute_cached(
                    "DELETE FROM field_ownership_violations
                     WHERE id NOT IN (
                         SELECT id FROM field_ownership_violations ORDER BY id DESC LIMIT ?1
                     )",
                    rusqlite::params![MAX_FIELD_OWNERSHIP_VIOLATIONS],
                )
                .map_err(|error| Self::map_sqlite(error, "compact field ownership violations"))?;
            Ok(())
        })
        .await
    }

    fn clone_through_serde<T>(value: &T) -> Result<T, ResourceError>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::from_value(serde_json::to_value(value).map_err(|err| ResourceError::decode(format!("serialize value: {err}")))?)
            .map_err(|err| ResourceError::decode(format!("deserialize value: {err}")))
    }

    fn encode_object(resource_kind: ResourceKind, object: &ResourceObject<SerializedResource>) -> Result<Value, ResourceError> {
        resource_kind.encode_object(object)
    }

    fn encode_tombstone(resource_kind: ResourceKind, tombstone: &ResourceTombstone) -> Value {
        serde_json::json!({
            "apiVersion": format!("{}/{}", resource_kind.paths.group, resource_kind.paths.version),
            "kind": resource_kind.paths.kind,
            "metadata": {
                "name": tombstone.name,
                "namespace": tombstone.namespace,
                "resourceVersion": tombstone.resource_version,
                "annotations": tombstone.annotations,
            },
        })
    }

    /// Decode one replayed event with `decode`, the only pass over a stored
    /// object body. A `Deleted` body that does not decode as an object is a
    /// name-only tombstone when it has no `spec`; only that failure is parsed
    /// again, and a present but malformed spec remains an error.
    fn replay_event(
        resource_kind: ResourceKind,
        event_kind: StoredEventKind,
        body: &str,
        decode: RowDecode<DecodedObject>,
    ) -> Result<SerializedWatchEvent, ResourceError> {
        let action = event_kind.into();
        let error = match decode(resource_kind, body) {
            Ok(object) => return Ok(SerializedWatchEvent { action, payload: WatchPayload::Decoded(object) }),
            Err(error) => error,
        };
        if !matches!(event_kind, StoredEventKind::Deleted) {
            return Err(error);
        }
        let value = serde_json::from_str::<Value>(body).map_err(|_| error.clone())?;
        if value.get("spec").is_some() {
            return Err(error);
        }
        decode_watch_tombstone(&value)?;
        Ok(SerializedWatchEvent { action, payload: WatchPayload::Owned(value) })
    }

    fn map_sqlite(err: rusqlite::Error, action: &str) -> ResourceError {
        ResourceError::other(format!("{action}: {err}"))
    }

    fn current_version(conn: &RusqliteConnection, key: &StoreKey) -> Result<u64, ResourceError> {
        let version = conn
            .query_row_cached(
                "SELECT next_version FROM resource_sequences WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4",
                params![key.0, key.1, key.2, key.3],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource sequence"))?;
        Ok(version.unwrap_or(1).saturating_sub(1))
    }

    fn allocate_version(tx: &rusqlite::Transaction<'_>, key: &StoreKey) -> Result<u64, ResourceError> {
        Self::allocate_version_after(tx, key, None)
    }

    fn allocate_version_after(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        minimum_resource_version: Option<u64>,
    ) -> Result<u64, ResourceError> {
        let stored_next = tx
            .query_row_cached(
                "SELECT next_version FROM resource_sequences WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4",
                params![key.0, key.1, key.2, key.3],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource sequence"))?
            .unwrap_or(1);
        let next = minimum_resource_version.map_or(stored_next, |minimum| stored_next.max(minimum.saturating_add(1)));
        tx.execute_cached(
            r#"
            INSERT INTO resource_sequences (group_name, version, kind, namespace, next_version)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(group_name, version, kind, namespace)
            DO UPDATE SET next_version = excluded.next_version
            "#,
            params![key.0, key.1, key.2, key.3, next + 1],
        )
        .map_err(|err| Self::map_sqlite(err, "write sqlite resource sequence"))?;
        Ok(next)
    }

    fn insert_event(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        event_version: u64,
        kind: StoredEventKind,
        body_json: &str,
        event_retention: EventRetention,
        event_history: crate::EventHistory,
    ) -> Result<(), ResourceError> {
        tx.execute_cached(
            r#"
            INSERT INTO resource_events (group_name, version, kind, namespace, event_version, event_type, body_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![key.0, key.1, key.2, key.3, event_version, kind.as_str(), body_json],
        )
        .map_err(|err| Self::map_sqlite(err, "insert sqlite resource event"))?;
        Self::compact_events_to_limit(
            tx,
            key,
            event_version,
            event_history.retained_events(event_retention.max_events_per_resource_stream()),
        )?;
        Ok(())
    }

    fn compact_events(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        current_version: u64,
        event_retention: EventRetention,
    ) -> Result<(), ResourceError> {
        Self::compact_events_to_limit(tx, key, current_version, event_retention.max_events_per_resource_stream())
    }

    fn compact_events_to_limit(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        current_version: u64,
        max_events: usize,
    ) -> Result<(), ResourceError> {
        let compacted_through = current_version.saturating_sub(max_events as u64);
        if compacted_through == 0 {
            return Ok(());
        }
        tx.execute_cached(
            r#"
            DELETE FROM resource_events
            WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND event_version <= ?5
            "#,
            params![key.0, key.1, key.2, key.3, compacted_through],
        )
        .map_err(|err| Self::map_sqlite(err, "compact sqlite resource events"))?;
        tx.execute_cached(
            r#"
            DELETE FROM resource_event_decode_quarantine
            WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND event_version <= ?5
            "#,
            params![key.0, key.1, key.2, key.3, compacted_through],
        )
        .map_err(|err| Self::map_sqlite(err, "compact sqlite resource event decode quarantine"))?;
        tx.execute_cached(
            r#"
            INSERT INTO resource_event_compaction (group_name, version, kind, namespace, compacted_through)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(group_name, version, kind, namespace)
            DO UPDATE SET compacted_through = MAX(compacted_through, excluded.compacted_through)
            "#,
            params![key.0, key.1, key.2, key.3, compacted_through],
        )
        .map_err(|err| Self::map_sqlite(err, "record sqlite resource event compaction"))?;
        Ok(())
    }

    fn compact_existing_events(connection: &mut RusqliteConnection, event_retention: EventRetention) -> Result<(), ResourceError> {
        let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource event startup compaction"))?;
        let stores = {
            let mut statement = tx
                .prepare("SELECT group_name, version, kind, namespace, next_version - 1 FROM resource_sequences")
                .map_err(|err| Self::map_sqlite(err, "prepare sqlite resource event startup compaction"))?;
            let rows = statement
                .query_map([], |row| Ok(((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?), row.get::<_, u64>(4)?)))
                .map_err(|err| Self::map_sqlite(err, "query sqlite resource event startup compaction"))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(|err| Self::map_sqlite(err, "read sqlite resource event startup compaction row"))?
        };
        for (key, current_version) in stores {
            Self::compact_events(&tx, &key, current_version, event_retention)?;
        }
        tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource event startup compaction"))
    }

    fn notify_watchers(watchers: &Mutex<WatchersByStore>, key: &StoreKey, event: StoredEvent) {
        if let Ok(mut watchers) = Self::lock_watchers(watchers) {
            if let Some(entries) = watchers.get_mut(key) {
                entries.send(event);
            }
        }
    }

    fn notify_replica_watchers(watchers: &Mutex<ReplicaWatchersByStore>, key: &StoreKey, event: StoredReplicaEvent) {
        if let Ok(mut watchers) = watchers.lock() {
            if let Some(entries) = watchers.get_mut(key) {
                entries.send(event);
            }
        }
    }

    pub async fn query_messages(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<crate::Message>>, ResourceError> {
        let resource_kind = ResourceKind::of::<crate::Message>();
        let key = Self::store_key(ResourceKind::of::<crate::Message>(), namespace);
        let (predicate, argument) = query.sql();
        let index = query.sql_index(false);
        let mut items = self.read_call("query Message inbox", move |connection| {
            let sql = format!("SELECT body_json FROM resource_objects INDEXED BY {index} WHERE group_name=?1 AND version=?2 AND kind=?3 AND namespace=?4 AND kind='Message' AND ({predicate}) AND (?5 IS NULL OR ?5 IS NOT NULL) ORDER BY name");
            let mut statement = connection.prepare_cached(&sql).map_err(|error| Self::map_sqlite(error, "prepare Message query"))?;
            let rows = statement.query_map(params![key.0, key.1, key.2, key.3, argument], |row| row.get::<_, String>(0)).map_err(|error| Self::map_sqlite(error, "query Message inbox"))?;
            rows.map(|row| {
                let body = row.map_err(|error| Self::map_sqlite(error, "read Message query"))?;
                Ok(ReadResourceObject { object: resource_kind.decode_typed_text(&body)?.into_typed()?, provenance: ResourceProvenance::Local })
            }).collect::<Result<Vec<_>, ResourceError>>()
        }).await?;
        if include_replicas {
            items.extend(self.query_message_replicas(namespace, query).await?);
        }
        Ok(items)
    }

    pub async fn query_message_replicas(
        &self,
        namespace: &str,
        query: &crate::MessageQuery,
    ) -> Result<Vec<ReadResourceObject<crate::Message>>, ResourceError> {
        let resource_kind = ResourceKind::of::<crate::Message>();
        let key = Self::store_key(ResourceKind::of::<crate::Message>(), namespace);
        let (predicate, argument) = query.sql();
        let index = query.sql_index(true);
        self.read_call("query Message replicas", move |connection| {
            let sql = format!("SELECT body_json, origin_root, last_synced_at FROM replica_objects INDEXED BY {index} WHERE group_name=?1 AND version=?2 AND kind=?3 AND namespace=?4 AND kind='Message' AND ({predicate}) AND (?5 IS NULL OR ?5 IS NOT NULL) ORDER BY origin_root,name");
            let mut statement = connection.prepare_cached(&sql).map_err(|error| Self::map_sqlite(error, "prepare Message replica query"))?;
            let rows = statement.query_map(params![key.0, key.1, key.2, key.3, argument], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))).map_err(|error| Self::map_sqlite(error, "query Message replicas"))?;
            rows.map(|row| {
                let (body, origin, synced_at) = row.map_err(|error| Self::map_sqlite(error, "read Message replica query"))?;
                let synced_at = chrono::DateTime::parse_from_rfc3339(&synced_at).map_err(|error| ResourceError::decode(error.to_string()))?.with_timezone(&Utc);
                Ok(ReadResourceObject { object: resource_kind.decode_typed_text(&body)?.into_typed()?, provenance: ResourceProvenance::Replica { origin_root: NodeId::new(origin), last_synced_at: synced_at } })
            }).collect::<Result<Vec<_>, ResourceError>>()
        }).await
    }

    pub async fn list_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        let rows = self.list_replica_rows(resource_kind, namespace, RowForm::serialized).await?;
        Ok(rows.into_iter().map(|(object, provenance)| ReadResourceObject { object, provenance }).collect())
    }

    pub async fn list_replicas_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<Vec<DecodedReadObject>, ResourceError> {
        let rows = self.list_replica_rows(resource_kind, namespace, RowForm::typed).await?;
        Ok(rows.into_iter().map(|(object, provenance)| DecodedReadObject { object, provenance }).collect())
    }

    async fn list_replica_rows<R: Send + 'static>(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        decode: RowDecode<R>,
    ) -> Result<Vec<(R, ResourceProvenance)>, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let cleanup_key = key.clone();
        let (items, invalid_partitions) = self
            .read_call("list replicas", move |connection| {
                let mut statement = connection
                    .prepare_cached(
                        r#"
                    SELECT o.origin_root, o.name, o.last_synced_at, o.body_json
                    FROM replica_objects o
                    WHERE o.group_name = ?1 AND o.version = ?2 AND o.kind = ?3 AND o.namespace = ?4
                    ORDER BY o.origin_root, o.name
                    "#,
                    )
                    .map_err(|err| Self::map_sqlite(err, "prepare sqlite replica list"))?;
                let rows = statement
                    .query_map(params![key.0, key.1, key.2, key.3], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?))
                    })
                    .map_err(|err| Self::map_sqlite(err, "query sqlite replica list"))?;
                let rows = rows.collect::<Result<Vec<_>, _>>().map_err(|err| Self::map_sqlite(err, "read sqlite replica list row"))?;
                drop(statement);

                let mut items = Vec::new();
                let mut invalid_partitions = BTreeMap::<String, Vec<(String, String, String)>>::new();
                for (origin_root, name, last_synced_at, body) in rows {
                    let identity = format!("kind={} namespace={} name={} origin={}", resource_kind.paths.kind, key.3, name, origin_root);
                    let decoded = decode(resource_kind, &body)
                        .map_err(|err| format!("decode cached replica object {identity}: {err}"))
                        .and_then(|object| {
                            DateTime::parse_from_rfc3339(&last_synced_at)
                                .map(|last_synced_at| (object, last_synced_at.with_timezone(&Utc)))
                                .map_err(|err| format!("decode cached replica sync timestamp {identity}: {err}"))
                        });
                    match decoded {
                        Ok((object, last_synced_at)) => items.push((
                            origin_root.clone(),
                            (object, ResourceProvenance::Replica { origin_root: NodeId::new(origin_root), last_synced_at }),
                        )),
                        Err(error) => invalid_partitions.entry(origin_root).or_default().push((name, body, error)),
                    }
                }
                Ok((items, invalid_partitions))
            })
            .await?;
        let items =
            items.into_iter().filter_map(|(origin_root, item)| (!invalid_partitions.contains_key(&origin_root)).then_some(item)).collect();
        if !invalid_partitions.is_empty() {
            self.cleanup_call(cleanup_key.clone(), "drop invalid replica partition", move |connection| {
                let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin invalid replica cache cleanup"))?;
                for (origin_root, failures) in &invalid_partitions {
                    // A changed bad row no longer justifies deleting its partition.
                    // If one unchanged bad row remains, dropping the whole origin
                    // matches the existing full-resync behavior.
                    let still_invalid = failures.iter().try_fold(false, |found, (name, body, _)| {
                        let unchanged = tx
                            .query_row_cached(
                                "SELECT 1 FROM replica_objects WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND name = ?6 AND body_json = ?7",
                                params![origin_root, cleanup_key.0, cleanup_key.1, cleanup_key.2, cleanup_key.3, name, body],
                                |_| Ok(()),
                            )
                            .optional()
                            .map_err(|err| Self::map_sqlite(err, "recheck invalid replica object"))?
                            .is_some();
                        Ok::<_, ResourceError>(found || unchanged)
                    })?;
                    if !still_invalid {
                        continue;
                    }
                    for sql in REPLICA_PARTITION_DELETE_SQL {
                        tx.execute_cached(sql, params![origin_root, cleanup_key.0, cleanup_key.1, cleanup_key.2, cleanup_key.3])
                            .map_err(|err| Self::map_sqlite(err, "drop undecodable sqlite replica partition"))?;
                    }
                    for (name, _, error) in failures {
                        tracing::warn!(
                            origin = %origin_root,
                            kind = resource_kind.paths.kind,
                            namespace = %cleanup_key.3,
                            %name,
                            %error,
                            "dropping undecodable cached replica partition; origin will be fully resynced"
                        );
                    }
                }
                tx.commit().map_err(|err| Self::map_sqlite(err, "commit invalid replica cache cleanup"))?;
                Ok(())
            });
        }
        Ok(items)
    }

    pub async fn get_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        let rows = self.get_replica_rows(resource_kind, namespace, name, RowForm::serialized).await?;
        Ok(rows.into_iter().map(|(object, provenance)| ReadResourceObject { object, provenance }).collect())
    }

    pub async fn get_replicas_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<DecodedReadObject>, ResourceError> {
        let rows = self.get_replica_rows(resource_kind, namespace, name, RowForm::typed).await?;
        Ok(rows.into_iter().map(|(object, provenance)| DecodedReadObject { object, provenance }).collect())
    }

    async fn get_replica_rows<R: Send + 'static>(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        decode: RowDecode<R>,
    ) -> Result<Vec<(R, ResourceProvenance)>, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let cleanup_key = key.clone();
        let requested_name = name.to_string();
        let (items, invalid) = self
            .read_call("get replicas", move |connection| {
                let rows = {
                    let mut statement = connection
                        .prepare_cached(
                            r#"
                        SELECT o.origin_root, o.last_synced_at, o.body_json
                        FROM replica_objects o
                        WHERE o.group_name = ?1 AND o.version = ?2 AND o.kind = ?3 AND o.namespace = ?4 AND o.name = ?5
                        ORDER BY o.origin_root
                        "#,
                        )
                        .map_err(|err| Self::map_sqlite(err, "prepare sqlite replica point lookup"))?;
                    let rows = statement
                        .query_map(params![key.0, key.1, key.2, key.3, requested_name], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                        })
                        .map_err(|err| Self::map_sqlite(err, "query sqlite replica point lookup"))?;
                    rows.collect::<Result<Vec<_>, _>>().map_err(|err| Self::map_sqlite(err, "read sqlite replica point lookup row"))?
                };

                let mut items = Vec::new();
                let mut invalid = Vec::new();
                for (origin_root, last_synced_at, body) in rows {
                    let identity =
                        format!("kind={} namespace={} name={} origin={}", resource_kind.paths.kind, key.3, requested_name, origin_root);
                    let decoded = decode(resource_kind, &body)
                        .map_err(|err| format!("decode cached replica object {identity}: {err}"))
                        .and_then(|object| {
                            DateTime::parse_from_rfc3339(&last_synced_at)
                                .map(|last_synced_at| (object, last_synced_at.with_timezone(&Utc)))
                                .map_err(|err| format!("decode cached replica sync timestamp {identity}: {err}"))
                        });
                    match decoded {
                        Ok((object, last_synced_at)) => {
                            items.push((object, ResourceProvenance::Replica { origin_root: NodeId::new(origin_root), last_synced_at }))
                        }
                        Err(error) => {
                            invalid.push((origin_root, body, error));
                        }
                    }
                }
                Ok((items, invalid))
            })
            .await?;
        if !invalid.is_empty() {
            let requested_name = name.to_string();
            self.cleanup_call(cleanup_key.clone(), "drop invalid replica partition", move |connection| {
                let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin invalid replica cache cleanup"))?;
                for (origin_root, body, error) in invalid {
                    // Preserve full-partition resync only while the bad row is unchanged.
                    let unchanged = tx
                        .query_row_cached(
                            "SELECT 1 FROM replica_objects WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND name = ?6 AND body_json = ?7",
                            params![origin_root, cleanup_key.0, cleanup_key.1, cleanup_key.2, cleanup_key.3, requested_name, body],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(|err| Self::map_sqlite(err, "recheck invalid replica object"))?
                        .is_some();
                    if !unchanged {
                        continue;
                    }
                    for sql in REPLICA_PARTITION_DELETE_SQL {
                        tx.execute_cached(sql, params![origin_root, cleanup_key.0, cleanup_key.1, cleanup_key.2, cleanup_key.3])
                            .map_err(|err| Self::map_sqlite(err, "drop undecodable sqlite replica partition"))?;
                    }
                    tracing::warn!(origin = %origin_root, kind = resource_kind.paths.kind, namespace = %cleanup_key.3, name = %requested_name, %error,
                        "dropping undecodable cached replica partition; origin will be fully resynced");
                }
                tx.commit().map_err(|err| Self::map_sqlite(err, "commit invalid replica cache cleanup"))
            });
        }
        Ok(items)
    }

    pub async fn watch_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<SerializedReadWatchEvent, ResourceError>>, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let rx = self
            .replica_watchers
            .lock()
            .map_err(|_| ResourceError::other("sqlite replica watch lock poisoned"))?
            .entry(key)
            .or_default()
            .subscribe(resource_kind.paths.kind, namespace);
        Ok(stream::unfold(rx, move |mut rx| async move {
            let event = match rx.next().await? {
                Ok(event) => event,
                Err(error) => return Some((Err(error), rx)),
            };
            let provenance = ResourceProvenance::Replica { origin_root: event.origin_root.clone(), last_synced_at: event.synced_at };
            let action = WatchAction::from(event.kind);
            let decoded =
                Ok(SerializedReadWatchEvent { event: SerializedWatchEvent { action, payload: WatchPayload::Shared(event) }, provenance });
            Some((decoded, rx))
        })
        .boxed())
    }

    pub async fn replace_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        listed: &ResourceList<SerializedResource>,
        synced_at: DateTime<Utc>,
        bucket: Option<u8>,
    ) -> Result<(), ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let operation_key = key.clone();
        let origin = origin_root.to_string();
        let generation = listed.generation.clone();
        let resource_version = listed.resource_version.clone();
        let synced = synced_at.to_rfc3339();
        let encoded = listed
            .items
            .iter()
            .map(|object| {
                Ok((
                    object.metadata.name.clone(),
                    serde_json::to_string(&Self::encode_object(resource_kind, object)?)
                        .map_err(|err| ResourceError::decode(format!("encode replica object JSON: {err}")))?,
                ))
            })
            .collect::<Result<Vec<_>, ResourceError>>()?;
        let events = self
            .call("replace replica snapshot", move |connection| {
                let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite replica replacement"))?;
                let old = {
                    let sql=if bucket.is_some() {
                        "SELECT o.name,o.body_json,o.last_synced_at FROM digest_entries d JOIN replica_objects o
                         ON o.origin_root=d.origin AND o.group_name=d.group_name AND o.version=d.version AND o.kind=d.kind AND o.namespace=d.namespace AND o.name=d.name
                         WHERE d.origin=?1 AND d.group_name=?2 AND d.version=?3 AND d.kind=?4 AND d.namespace=?5 AND d.bucket=?6"
                    } else {
                        "SELECT name,body_json,last_synced_at FROM replica_objects
                         WHERE origin_root=?1 AND group_name=?2 AND version=?3 AND kind=?4 AND namespace=?5 AND ?6 IS NULL"
                    };
                    let mut statement = tx.prepare_cached(sql)
                        .map_err(|err| Self::map_sqlite(err, "prepare old sqlite replicas"))?;
                    let rows = statement
                        .query_map(params![origin, key.0, key.1, key.2, key.3, bucket], |row| {
                            Ok((row.get::<_, String>(0)?, (row.get::<_, String>(1)?, row.get::<_, String>(2)?)))
                        })
                        .map_err(|err| Self::map_sqlite(err, "query old sqlite replicas"))?;
                    rows.collect::<Result<HashMap<_, _>, _>>().map_err(|err| Self::map_sqlite(err, "read old sqlite replica row"))?
                };
                let mut accepted = HashSet::new();
                for (name, body) in &encoded {
                    let mutation = ReplicaMutation { kind: StoredReplicaEventKind::Added, name: name.clone(), value: serde_json::from_str(body).map_err(|err| ResourceError::decode(err.to_string()))?, synced_at };
                    let applied = Self::apply_replica_record(&tx, resource_kind, &key, &origin, &mutation, || {
                    if old.get(name).is_some_and(|(old_body, _)| old_body == body) {
                        return Ok(true);
                    }
                    tx.execute_cached(
                        REPLICA_OBJECT_UPSERT_SQL,
                        params![origin, key.0, key.1, key.2, key.3, name, body, synced],
                    )
                    .map_err(|err| Self::map_replica_sqlite(err, "insert sqlite replica object"))?;
                    tx.execute_cached(
                        REPLICA_TOMBSTONE_CLEAR_SQL,
                        params![origin, key.0, key.1, key.2, key.3, name],
                    )
                    .map_err(|err| Self::map_replica_sqlite(err, "clear authoritative snapshot tombstone"))?;
                    Ok(true)
                    })?;
                    if applied { accepted.insert(name.clone()); }
                }
                let new_names = encoded.iter().map(|(name, _)| name.clone()).collect::<std::collections::HashSet<_>>();
                tx.execute_cached("DELETE FROM replica_apply_quarantine WHERE origin_root=?1 AND group_name=?2 AND version=?3 AND kind=?4 AND namespace=?5
                    AND (?6 IS NULL OR digest_bucket(name)=?6) AND name NOT IN (SELECT value FROM json_each(?7))",
                    params![origin,key.0,key.1,key.2,key.3,bucket,serde_json::json!(new_names).to_string()])
                    .map_err(|err| Self::map_sqlite(err, "prune snapshot-absent replica quarantines"))?;

                for name in old.keys().filter(|name| !new_names.contains(*name)) {
                    tx.execute_cached(
                        REPLICA_OBJECT_DELETE_SQL,
                        params![origin, key.0, key.1, key.2, key.3, name],
                    )
                    .map_err(|err| Self::map_sqlite(err, "remove snapshot-deleted replica"))?;
                    tx.execute_cached(
                        REPLICA_TOMBSTONE_UPSERT_SQL,
                        params![origin, key.0, key.1, key.2, key.3, name, synced],
                    )
                    .map_err(|err| Self::map_sqlite(err, "fence snapshot-deleted replica"))?;
                }
                if bucket.is_none() {
                    tx.execute_cached(
                    r#"
                    INSERT INTO replica_cursors
                        (origin_root, group_name, version, kind, namespace, resource_version, generation, last_synced_at, complete_prefix)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1)
                    ON CONFLICT(origin_root, group_name, version, kind, namespace)
                    DO UPDATE SET complete_prefix = 1, resource_version = excluded.resource_version,
                                  generation = excluded.generation,
                                  last_synced_at = excluded.last_synced_at
                    "#,
                    params![origin, key.0, key.1, key.2, key.3, resource_version, generation, synced],
                )
                    .map_err(|err| Self::map_sqlite(err, "write sqlite replica cursor"))?;
                }
                tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite replica replacement"))?;

                let mut events = encoded
                    .into_iter()
                    .filter(|(name, body)| accepted.contains(name) && old.get(name).is_none_or(|(old_body, _)| old_body != body))
                    .map(|(name, body)| {
                        let kind = if old.contains_key(&name) { StoredReplicaEventKind::Modified } else { StoredReplicaEventKind::Added };
                        Ok((
                            kind,
                            serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode replica event: {err}")))?,
                        ))
                    })
                    .collect::<Result<Vec<_>, ResourceError>>()?;
                for (name, (body, _)) in old {
                    if !new_names.contains(&name) {
                        events.push((
                            StoredReplicaEventKind::Deleted,
                            serde_json::from_str(&body)
                                .map_err(|err| ResourceError::decode(format!("decode deleted replica event: {err}")))?,
                        ));
                    }
                }
                Ok(events)
            })
            .await?;
        for (kind, object) in events {
            Self::notify_replica_watchers(
                &self.replica_watchers,
                &operation_key,
                StoredReplicaEvent { origin_root: origin_root.clone(), synced_at, kind, object },
            );
        }
        Ok(())
    }

    pub async fn apply_replica_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        kind: StoredReplicaEventKind,
        object: &ResourceObject<SerializedResource>,
        synced_at: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let name = object.metadata.name.clone();
        let value = Self::encode_object(resource_kind, object)?;
        self.apply_replica_value(
            resource_kind,
            origin_root,
            namespace,
            ReplicaMutation::builder().kind(kind).name(name).value(value).synced_at(synced_at).build(),
        )
        .await
    }

    pub async fn apply_replica_tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        tombstone: &ResourceTombstone,
        synced_at: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        self.apply_replica_value(
            resource_kind,
            origin_root,
            namespace,
            ReplicaMutation::builder()
                .kind(StoredReplicaEventKind::Deleted)
                .name(tombstone.name.clone())
                .value(Self::encode_tombstone(resource_kind, tombstone))
                .synced_at(synced_at)
                .build(),
        )
        .await
    }

    async fn apply_replica_value(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        mutation: ReplicaMutation,
    ) -> Result<(), ResourceError> {
        self.apply_replica_values(resource_kind, origin_root, namespace, vec![mutation]).await
    }

    fn map_replica_sqlite(err: rusqlite::Error, action: &str) -> ReplicaApplyError {
        let rejected = matches!(&err, rusqlite::Error::SqliteFailure(code, _) if code.code == rusqlite::ErrorCode::ConstraintViolation);
        let error = Self::map_sqlite(err, action);
        if rejected {
            ReplicaApplyError::Rejected(error)
        } else {
            ReplicaApplyError::Storage(error)
        }
    }

    fn write_replica_mutation(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        origin: &str,
        mutation: &ReplicaMutation,
    ) -> Result<bool, ReplicaApplyError> {
        let ReplicaMutation { kind, name, value, synced_at } = mutation;
        let body = value.to_string();
        let synced = synced_at.to_rfc3339();
        let existing = tx
            .query_row_cached(
                r#"
            SELECT body_json, last_synced_at
            FROM replica_objects
            WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND name = ?6
            "#,
                params![origin, key.0, key.1, key.2, key.3, name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|err| Self::map_replica_sqlite(err, "read existing sqlite replica event object"))?;
        let tombstone_synced_at = tx
            .query_row_cached(
                r#"
            SELECT last_synced_at
            FROM replica_tombstones
            WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND name = ?6
            "#,
                params![origin, key.0, key.1, key.2, key.3, name],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|err| Self::map_replica_sqlite(err, "read sqlite replica tombstone"))?;
        let unchanged = match kind {
            StoredReplicaEventKind::Added | StoredReplicaEventKind::Modified => {
                tombstone_synced_at.as_ref().is_some_and(|existing_synced| existing_synced >= &synced)
                    || existing.as_ref().is_some_and(|(existing_body, existing_synced)| {
                        existing_synced > &synced || (existing_synced == &synced && existing_body >= &body)
                    })
            }
            StoredReplicaEventKind::Deleted => {
                tombstone_synced_at.as_ref().is_some_and(|existing_synced| existing_synced >= &synced)
                    || existing.as_ref().is_some_and(|(_, existing_synced)| existing_synced > &synced)
            }
        };
        if unchanged {
            return Ok(false);
        }

        match kind {
            StoredReplicaEventKind::Added | StoredReplicaEventKind::Modified => {
                tx.execute_cached(REPLICA_OBJECT_UPSERT_SQL, params![origin, key.0, key.1, key.2, key.3, name, body, synced])
                    .map_err(|err| Self::map_replica_sqlite(err, "upsert sqlite replica event object"))?;
                tx.execute_cached(REPLICA_TOMBSTONE_CLEAR_SQL, params![origin, key.0, key.1, key.2, key.3, name])
                    .map_err(|err| Self::map_replica_sqlite(err, "clear sqlite replica tombstone"))?;
            }
            StoredReplicaEventKind::Deleted => {
                tx.execute_cached(REPLICA_OBJECT_DELETE_SQL, params![origin, key.0, key.1, key.2, key.3, name])
                    .map_err(|err| Self::map_replica_sqlite(err, "delete sqlite replica event object"))?;
                tx.execute_cached(REPLICA_TOMBSTONE_UPSERT_SQL, params![origin, key.0, key.1, key.2, key.3, name, synced])
                    .map_err(|err| Self::map_replica_sqlite(err, "write sqlite replica tombstone"))?;
            }
        }
        Ok(true)
    }

    fn apply_replica_record(
        tx: &rusqlite::Transaction<'_>,
        resource_kind: ResourceKind,
        key: &StoreKey,
        origin: &str,
        mutation: &ReplicaMutation,
        apply: impl FnOnce() -> Result<bool, ReplicaApplyError>,
    ) -> Result<bool, ResourceError> {
        tx.execute_cached("SAVEPOINT replica_record", []).map_err(|err| Self::map_sqlite(err, "begin replica record"))?;
        let result = if matches!(mutation.kind, StoredReplicaEventKind::Added | StoredReplicaEventKind::Modified) {
            resource_kind.validate_stored_object(&mutation.value).map_err(ReplicaApplyError::Rejected).and_then(|()| apply())
        } else {
            apply()
        };
        match result {
            Ok(changed) => {
                if changed {
                    tx.execute_cached("DELETE FROM replica_apply_quarantine WHERE origin_root=?1 AND group_name=?2 AND version=?3 AND kind=?4 AND namespace=?5 AND name=?6",
                    params![origin, key.0, key.1, key.2, key.3, mutation.name])
                    .map_err(|err| Self::map_sqlite(err, "clear replica quarantine"))?;
                }
                tx.execute_cached("RELEASE replica_record", []).map_err(|err| Self::map_sqlite(err, "commit replica record"))?;
                Ok(changed)
            }
            Err(ReplicaApplyError::Storage(error)) => Err(error),
            Err(ReplicaApplyError::Rejected(error)) => {
                tx.execute_cached("ROLLBACK TO replica_record", []).map_err(|err| Self::map_sqlite(err, "rollback rejected replica"))?;
                tx.execute_cached("RELEASE replica_record", []).map_err(|err| Self::map_sqlite(err, "release rejected replica"))?;
                // Preserve payload, stable event type and receipt time for operator
                // inspection. This is forensic evidence, not a replay queue;
                // compact diagnostics deliberately omit the payload.
                let event_type = match mutation.kind {
                    StoredReplicaEventKind::Added => "ADDED",
                    StoredReplicaEventKind::Modified => "MODIFIED",
                    StoredReplicaEventKind::Deleted => "DELETED",
                };
                let changed = tx.execute_cached("INSERT INTO replica_apply_quarantine
                    (origin_root,group_name,version,kind,namespace,name,body_json,event_type,last_synced_at,error,quarantined_at)
                    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                    ON CONFLICT(origin_root,group_name,version,kind,namespace,name) DO UPDATE SET
                    body_json=excluded.body_json,event_type=excluded.event_type,last_synced_at=excluded.last_synced_at,error=excluded.error,quarantined_at=CASE WHEN replica_apply_quarantine.body_json=excluded.body_json THEN replica_apply_quarantine.quarantined_at ELSE excluded.quarantined_at END
                    WHERE replica_apply_quarantine.body_json<>excluded.body_json OR replica_apply_quarantine.error<>excluded.error",
                    params![origin,key.0,key.1,key.2,key.3,mutation.name,mutation.value.to_string(),event_type,mutation.synced_at.to_rfc3339(),error.to_string(),Utc::now().to_rfc3339()])
                    .map_err(|err| Self::map_sqlite(err, "quarantine rejected replica"))?;
                if changed != 0 {
                    tracing::warn!(%origin, kind=%key.2, namespace=%key.3, name=%mutation.name, %error, "quarantined rejected replica; continuing batch");
                }
                Ok(false)
            }
        }
    }

    async fn apply_replica_values(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        mutations: Vec<ReplicaMutation>,
    ) -> Result<(), ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let origin_root = origin_root.clone();
        let origin = origin_root.to_string();
        let watchers = self.replica_watchers.clone();
        self.call("apply replica batch", move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin replica batch"))?;
            let mut changed = Vec::new();
            for mutation in mutations {
                let applied = Self::apply_replica_record(&tx, resource_kind, &key, &origin, &mutation, || {
                    Self::write_replica_mutation(&tx, &key, &origin, &mutation)
                })?;
                if applied {
                    changed.push(StoredReplicaEvent {
                        origin_root: origin_root.clone(),
                        synced_at: mutation.synced_at,
                        kind: mutation.kind,
                        object: mutation.value,
                    });
                }
            }
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit replica batch"))?;
            // Publish from the writer after commit, in exactly commit order.
            for event in changed {
                Self::notify_replica_watchers(&watchers, &key, event);
            }
            Ok(())
        })
        .await
    }

    pub async fn invalidate_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<(), ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let origin = origin_root.to_string();
        self.call("invalidate replica prefix", move |connection| {
            connection.execute_cached("UPDATE replica_cursors SET complete_prefix = 0 WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5", params![origin, key.0, key.1, key.2, key.3])
                .map_err(|err| Self::map_sqlite(err, "invalidate replica prefix"))?;
            Ok(())
        }).await
    }

    pub async fn advance_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        previous: &ReplicaCursor,
        next: &str,
    ) -> Result<(), ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let origin = origin_root.to_string();
        let previous = previous.clone();
        let next = next.to_string();
        self.call("advance proven replica prefix", move |connection| {
            let changed = connection.execute_cached(
                "UPDATE replica_cursors SET resource_version = ?6 WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND complete_prefix = 1 AND resource_version = ?7 AND generation IS ?8",
                params![origin, key.0, key.1, key.2, key.3, next, previous.resource_version, previous.generation],
            ).map_err(|err| Self::map_sqlite(err, "advance replica prefix"))?;
            if changed != 1 { return Err(ResourceError::invalid("replica prefix changed concurrently")); }
            Ok(())
        }).await
    }

    pub async fn replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let origin = origin_root.to_string();
        self.read_call("read replica cursor", move |connection| {
            connection
                .query_row_cached(
                    r#"
                    SELECT resource_version, generation FROM replica_cursors
                    WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND complete_prefix = 1
                    "#,
                    params![origin, key.0, key.1, key.2, key.3],
                    |row| Ok(ReplicaCursor { resource_version: row.get(0)?, generation: row.get(1)? }),
                )
                .optional()
                .map_err(|err| Self::map_sqlite(err, "read sqlite replica cursor"))
        })
        .await
    }

    pub async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.get_row(resource_kind, namespace, name, RowForm::serialized).await
    }

    /// One local object decoded once, straight into the kind's registered type.
    pub async fn get_decoded(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<DecodedObject, ResourceError> {
        self.get_row(resource_kind, namespace, name, RowForm::typed).await
    }

    async fn get_row<R: Send + 'static>(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        decode: RowDecode<R>,
    ) -> Result<R, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let name = name.to_string();
        self.read_call("get resource", move |connection| {
            let body: String = connection
                .query_row_cached(RESOURCE_OBJECT_BODY_SQL, params![key.0, key.1, key.2, key.3, name], |row| row.get(0))
                .optional()
                .map_err(|err| Self::map_sqlite(err, "read sqlite resource object"))?
                .ok_or_else(|| ResourceError::not_found(&name))?;
            decode(resource_kind, &body)
        })
        .await
    }

    pub async fn current_position_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<crate::ResourcePosition, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        self.read_call("read resource position", move |connection| {
            Ok(crate::ResourcePosition { resource_version: Self::current_version(connection, &key)?.to_string(), generation: None })
        })
        .await
    }

    pub async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let (items, resource_version) =
            self.list_local_query(resource_kind, namespace, &crate::StoreQuery::default(), RowForm::serialized).await?;
        Ok(ResourceList { items, resource_version, generation: None })
    }

    /// Every local object of a kind, each decoded once into its registered type.
    pub async fn list_decoded(&self, resource_kind: ResourceKind, namespace: &str) -> Result<DecodedList, ResourceError> {
        let (items, resource_version) =
            self.list_local_query(resource_kind, namespace, &crate::StoreQuery::default(), RowForm::typed).await?;
        Ok(DecodedList { items, resource_version, generation: None })
    }

    /// Rows that fail `decode` are excluded and queued for quarantine; the
    /// decode is the only pass over each body.
    async fn list_local_query<R: Send + 'static>(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
        decode: RowDecode<R>,
    ) -> Result<(Vec<R>, String), ResourceError> {
        query.validate(resource_kind)?;
        let (sql, args) = Self::declared_sql(resource_kind, namespace, query, false, "", QueryColumns::LocalList);

        let key = Self::store_key(resource_kind, namespace);
        let quarantine_key = key.clone();
        let namespace = namespace.to_string();
        let (listed, failures) = self
            .read_call("list resources", move |connection| {
                let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource list snapshot"))?;
                let rows = {
                    let mut statement = tx.prepare_cached(&sql).map_err(|err| Self::map_sqlite(err, "prepare sqlite resource list"))?;
                    let rows = statement
                        .query_map(rusqlite::params_from_iter(args), |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
                        .map_err(|err| Self::map_sqlite(err, "query sqlite resource list"))?;
                    rows.collect::<Result<Vec<_>, _>>().map_err(|err| Self::map_sqlite(err, "read sqlite resource list row"))?
                };
                let resource_version = Self::current_version(&tx, &key)?.to_string();
                tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource list snapshot"))?;
                let mut items = Vec::new();
                let mut failures = Vec::new();
                for (name, body) in rows {
                    match decode(resource_kind, &body) {
                        Ok(object) => items.push(object),
                        Err(error) => failures.push((name, body, error.to_string())),
                    }
                }
                let warnings: Vec<_> = failures.iter().map(|(name, _, error)| (name.clone(), error.clone())).collect();
                Ok(((items, resource_version), (failures, warnings)))
            })
            .await?;
        let (invalid, failures) = failures;
        if !invalid.is_empty() {
            self.cleanup_call(quarantine_key.clone(), "quarantine invalid resource", move |connection| {
                let quarantined_at = Utc::now();
                let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource decode quarantine"))?;
                for (name, body, error) in &invalid {
                    let unchanged = tx
                        .query_row_cached(
                            "SELECT 1 FROM resource_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5 AND body_json = ?6",
                            params![quarantine_key.0, quarantine_key.1, quarantine_key.2, quarantine_key.3, name, body],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(|err| Self::map_sqlite(err, "recheck invalid sqlite resource object"))?
                        .is_some();
                    if !unchanged {
                        continue;
                    }
                    tx.execute_cached(
                        "INSERT INTO resource_decode_quarantine
                         (group_name, version, kind, namespace, name, body_json, error, quarantined_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(group_name, version, kind, namespace, name)
                         DO UPDATE SET body_json = excluded.body_json, error = excluded.error, quarantined_at = excluded.quarantined_at",
                        params![quarantine_key.0, quarantine_key.1, quarantine_key.2, quarantine_key.3, name, body, error, quarantined_at.to_rfc3339()],
                    )
                    .map_err(|err| Self::map_sqlite(err, "persist sqlite resource decode quarantine"))?;
                    tx.execute_cached(
                        "DELETE FROM resource_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5 AND body_json = ?6",
                        params![quarantine_key.0, quarantine_key.1, quarantine_key.2, quarantine_key.3, name, body],
                    )
                    .map_err(|err| Self::map_sqlite(err, "remove quarantined sqlite resource object"))?;
                }
                tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource decode quarantine"))
            });
        }
        for (name, error) in failures {
            tracing::warn!(
                kind = resource_kind.paths.kind,
                namespace = %namespace,
                %name,
                %error,
                "excluded undecodable stored resource object; quarantine queued"
            );
        }
        Ok(listed)
    }

    pub async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let (items, resource_version) =
            self.list_local_query(resource_kind, namespace, &Self::label_query(required), RowForm::serialized).await?;
        Ok(ResourceList { items, resource_version, generation: None })
    }

    fn label_query(required: &BTreeMap<String, String>) -> crate::StoreQuery {
        if required.is_empty() {
            crate::StoreQuery::default()
        } else {
            crate::StoreQuery::labels(required, false, true)
        }
    }

    pub async fn list_decoded_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<DecodedList, ResourceError> {
        let (items, resource_version) =
            self.list_local_query(resource_kind, namespace, &Self::label_query(required), RowForm::typed).await?;
        Ok(DecodedList { items, resource_version, generation: None })
    }

    pub async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.create_typed_with_merge(resource_kind, namespace, meta, spec, None).await
    }

    pub async fn create_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.create_typed_with_merge(resource_kind, namespace, meta, spec, Some(merge)).await
    }

    async fn create_typed_with_merge(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
        merge: Option<MergeMetadata>,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        resource_kind.validate_spec(meta, spec)?;
        let initial_status = resource_kind.initial_status(meta)?;
        let key = Self::store_key(resource_kind, namespace);
        let namespace = namespace.to_string();
        let meta = meta.clone();
        let spec = spec.clone();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call("create resource", move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource create"))?;
            let exists = tx
                .query_row_cached(
                    r#"
                    SELECT 1 FROM resource_objects
                    WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                    "#,
                    params![key.0, key.1, key.2, key.3, meta.name],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|err| Self::map_sqlite(err, "check sqlite resource create conflict"))?
                .is_some();
            if exists {
                return Err(ResourceError::conflict(&meta.name, "resource already exists"));
            }

            Self::validate_namespace_spec(resource_kind, &tx, &key, &meta, &spec)?;
            let version = Self::allocate_version(&tx, &key)?;
            let object = ResourceObject::<SerializedResource> {
                metadata: ObjectMeta {
                    name: meta.name.clone(),
                    namespace,
                    resource_version: version.to_string(),
                    labels: meta.labels.clone(),
                    annotations: meta.annotations.clone(),
                    owner_references: meta.owner_references.clone(),
                    finalizers: meta.finalizers.clone(),
                    deletion_timestamp: meta.deletion_timestamp,
                    creation_timestamp: Utc::now(),
                    merge,
                },
                spec: Self::clone_through_serde(&spec)?,
                status: initial_status,
            };
            let encoded = Self::encode_object(resource_kind, &object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            tx.execute_cached(
                r#"
                DELETE FROM resource_tombstones
                WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                "#,
                params![key.0, key.1, key.2, key.3, meta.name],
            )
            .map_err(|err| Self::map_sqlite(err, "clear sqlite resource tombstone"))?;
            tx.execute_cached(
                r#"
                INSERT INTO resource_objects (group_name, version, kind, namespace, name, resource_version, body_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![key.0, key.1, key.2, key.3, meta.name, version, body_json],
            )
            .map_err(|err| Self::map_sqlite(err, "insert sqlite resource object"))?;
            Self::clear_decode_quarantine(&tx, &key, &meta.name)?;
            Self::insert_event(&tx, &key, version, StoredEventKind::Added, &body_json, event_retention, resource_kind.event_history)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource create"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: StoredEventKind::Added, object: encoded });
            Ok(object)
        })
        .await
    }

    pub async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.update_typed_with_merge(resource_kind, namespace, meta, resource_version, spec, None).await
    }

    pub async fn update_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        self.update_typed_with_merge(resource_kind, namespace, meta, resource_version, spec, Some(merge)).await
    }

    async fn update_typed_with_merge(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
        admitted_merge: Option<MergeMetadata>,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let meta = meta.clone();
        let resource_version = resource_version.to_string();
        let spec = spec.clone();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call("update resource", move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource update"))?;
            let existing = Self::select_existing(resource_kind, &tx, &key, &meta.name)?;
            let mut object = existing.ok_or_else(|| ResourceError::not_found(&meta.name))?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
            }
            resource_kind.validate_spec_update(&object.spec, &spec)?;
            resource_kind.validate_spec(&meta, &spec)?;
            Self::validate_namespace_spec(resource_kind, &tx, &key, &meta, &spec)?;
            if object.matches_update(&meta, &spec)?
                && admitted_merge.as_ref().is_none_or(|merge| object.metadata.merge.as_ref() == Some(merge))
            {
                return Ok(object);
            }

            let version = Self::allocate_version(&tx, &key)?;
            object.metadata.resource_version = version.to_string();
            object.metadata.labels = meta.labels.clone();
            object.metadata.annotations = meta.annotations.clone();
            object.metadata.owner_references = meta.owner_references.clone();
            object.metadata.finalizers = meta.finalizers.clone();
            object.metadata.deletion_timestamp = meta.deletion_timestamp;
            if let Some(merge) = admitted_merge {
                object.metadata.merge = Some(merge);
            }
            object.spec = Self::clone_through_serde(&spec)?;

            let encoded = Self::encode_object(resource_kind, &object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            let event_kind = if resource_kind.replication != crate::ReplicationClass::Definitions
                && object.metadata.deletion_timestamp.is_some()
                && object.metadata.finalizers.is_empty()
            {
                tx.execute_cached(RESOURCE_OBJECT_DELETE_SQL, params![key.0, key.1, key.2, key.3, meta.name])
                    .map_err(|err| Self::map_sqlite(err, "delete sqlite resource object"))?;
                StoredEventKind::Deleted
            } else {
                Self::upsert_object(&tx, &key, &meta.name, version, &body_json)?;
                StoredEventKind::Modified
            };
            Self::insert_event(&tx, &key, version, event_kind, &body_json, event_retention, resource_kind.event_history)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource update"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: event_kind, object: encoded });
            Ok(object)
        })
        .await
    }

    pub async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let name = name.to_string();
        let resource_version = resource_version.to_string();
        let status = status.clone();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call("update resource status", move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource status update"))?;
            let existing = Self::select_existing(resource_kind, &tx, &key, &name)?;
            let mut object = existing.ok_or_else(|| ResourceError::not_found(&name))?;
            if object.metadata.resource_version != resource_version {
                if resource_kind.paths.kind == Convoy::API_PATHS.kind {
                    return Err(ResourceError::status_version_conflict(&name, &resource_version, &object.metadata.resource_version));
                }
                return Err(ResourceError::conflict(&name, "stale resourceVersion"));
            }
            resource_kind.validate_status_update(object.status.as_ref(), &status)?;
            if object.matches_status(&status)? {
                return Ok(object);
            }

            let version = Self::allocate_version(&tx, &key)?;
            object.metadata.resource_version = version.to_string();
            object.status = Some(Self::clone_through_serde(&status)?);

            let encoded = Self::encode_object(resource_kind, &object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            Self::upsert_object(&tx, &key, &name, version, &body_json)?;
            Self::insert_event(&tx, &key, version, StoredEventKind::Modified, &body_json, event_retention, resource_kind.event_history)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource status update"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: StoredEventKind::Modified, object: encoded });
            Ok(object)
        })
        .await
    }

    pub async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let name = name.to_string();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call("delete resource", move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource delete"))?;
            let existing = Self::select_existing(resource_kind, &tx, &key, &name)?;
            let Some(mut object) = existing else {
                if Self::clear_decode_quarantine(&tx, &key, &name)? {
                    tx.commit().map_err(|err| Self::map_sqlite(err, "commit quarantined sqlite resource delete"))?;
                    return Ok(());
                }
                return Err(ResourceError::not_found(&name));
            };
            if object.metadata.is_pending_finalization() {
                return Ok(());
            }
            let version = Self::allocate_version(&tx, &key)?;
            object.metadata.resource_version = version.to_string();

            let (event_kind, encoded) = if !object.metadata.finalizers.is_empty() && object.metadata.deletion_timestamp.is_none() {
                object.metadata.deletion_timestamp = Some(Utc::now());
                let encoded = Self::encode_object(resource_kind, &object)?;
                let body_json =
                    serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
                Self::upsert_object(&tx, &key, &name, version, &body_json)?;
                Self::insert_event(
                    &tx,
                    &key,
                    version,
                    StoredEventKind::Modified,
                    &body_json,
                    event_retention,
                    resource_kind.event_history,
                )?;
                (StoredEventKind::Modified, encoded)
            } else {
                let encoded = Self::encode_object(resource_kind, &object)?;
                let body_json =
                    serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
                tx.execute_cached(RESOURCE_OBJECT_DELETE_SQL, params![key.0, key.1, key.2, key.3, name])
                    .map_err(|err| Self::map_sqlite(err, "delete sqlite resource object"))?;
                Self::insert_event(&tx, &key, version, StoredEventKind::Deleted, &body_json, event_retention, resource_kind.event_history)?;
                (StoredEventKind::Deleted, encoded)
            };
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource delete"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: event_kind, object: encoded });
            Ok(())
        })
        .await
    }

    pub async fn delete_decode_quarantine_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<bool, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let name = name.to_string();
        self.call("delete resource quarantine", move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource quarantine delete"))?;
            if Self::select_existing(resource_kind, &tx, &key, &name)?.is_some() {
                return Err(ResourceError::conflict(&name, "cannot delete quarantine while a live resource exists"));
            }
            let deleted = Self::clear_decode_quarantine(&tx, &key, &name)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource quarantine delete"))?;
            Ok(deleted)
        })
        .await
    }

    pub async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<crate::watch::TombstoneWrite, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let namespace = namespace.to_string();
        let name = name.to_string();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        let minimum_resource_version = minimum_resource_version
            .map(|version| {
                version.parse::<u64>().map_err(|error| {
                    ResourceError::invalid(format!("invalid replica cursor resourceVersion '{version}' while tombstoning: {error}"))
                })
            })
            .transpose()?;
        self.call("tombstone resource", move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource tombstone"))?;
            if Self::select_existing(resource_kind, &tx, &key, &name)?.is_some() {
                return Err(ResourceError::conflict(&name, "cannot tombstone an existing resource by name"));
            }
            let existing = tx
                .query_row_cached(
                    r#"
                    SELECT body_json FROM resource_tombstones
                    WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                    "#,
                    params![key.0, key.1, key.2, key.3, name],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|err| Self::map_sqlite(err, "read sqlite resource tombstone"))?;
            if let Some(body) = existing {
                Self::clear_decode_quarantine(&tx, &key, &name)?;
                let value = serde_json::from_str(&body)
                    .map_err(|err| ResourceError::decode(format!("decode stored resource tombstone JSON: {err}")))?;
                let tombstone = decode_watch_tombstone(&value)?;
                tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource quarantine cleanup"))?;
                return Ok(crate::watch::TombstoneWrite { tombstone, created: false });
            }
            let version = Self::allocate_version_after(&tx, &key, minimum_resource_version)?;
            let tombstone = ResourceTombstone {
                name: name.clone(),
                namespace: namespace.clone(),
                resource_version: version.to_string(),
                annotations: BTreeMap::new(),
            };
            let encoded = Self::encode_tombstone(resource_kind, &tombstone);
            let body_json =
                serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode tombstone JSON: {err}")))?;
            tx.execute_cached(
                r#"
                INSERT INTO resource_tombstones (group_name, version, kind, namespace, name, body_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                "#,
                params![key.0, key.1, key.2, key.3, name, body_json],
            )
            .map_err(|err| Self::map_sqlite(err, "write sqlite resource tombstone"))?;
            Self::clear_decode_quarantine(&tx, &key, &name)?;
            Self::insert_event(&tx, &key, version, StoredEventKind::Deleted, &body_json, event_retention, resource_kind.event_history)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource tombstone"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: StoredEventKind::Deleted, object: encoded });
            Ok(crate::watch::TombstoneWrite { tombstone, created: true })
        })
        .await
    }

    pub async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<SerializedWatchStream, ResourceError> {
        fn serialized(kind: ResourceKind, body: &str) -> Result<DecodedObject, ResourceError> {
            kind.decode_object_text(body).map(DecodedObject::serialized)
        }
        self.watch_events(resource_kind, namespace, start, serialized).await
    }

    /// [`Self::watch_raw`] whose replayed objects are decoded once, straight into
    /// the kind's registered type, for typed consumers only.
    pub async fn watch_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<SerializedWatchStream, ResourceError> {
        self.watch_events(resource_kind, namespace, start, RowForm::typed).await
    }

    async fn watch_events(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
        decode: RowDecode<DecodedObject>,
    ) -> Result<SerializedWatchStream, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        let replay_from = match &start {
            WatchStart::Now => None,
            WatchStart::FromVersion(version) => {
                Some(version.parse::<u64>().map_err(|err| ResourceError::invalid(format!("invalid resourceVersion '{version}': {err}")))?)
            }
            WatchStart::FromVersionInGeneration { .. } => {
                return Err(ResourceError::invalid("sqlite resource watches do not use generations"));
            }
        };
        // Register on the writer: no commit can land between reading the
        // stream's current version and subscribing, because mutations commit
        // and notify on the same thread. The live receiver gets exactly the
        // events after `replay_through`. Validation runs here too, so a bad
        // start fails before any reader work, with the same errors as before.
        let watchers = Arc::clone(&self.watchers);
        let register_key = key.clone();
        let (receiver, replay_through) = self
            .call("watch resource register", move |connection| {
                let replay_through = replay_from.map(|from| Self::replay_bounds(connection, &register_key, from)).transpose()?;
                let namespace = register_key.3.clone();
                let receiver =
                    Self::lock_watchers(&watchers)?.entry(register_key).or_default().subscribe(resource_kind.paths.kind, &namespace);
                Ok((receiver, replay_through))
            })
            .await?;
        self.replay_probe.registered().await;

        // Replay (from, through] on a reader. Its snapshot begins after the
        // registration returned, so it holds every event up to `through`;
        // later commits are bounded out and arrive live instead. Until the
        // stream is polled, live events wait in the stream's shared ring, and
        // a subscriber that falls more than a ring behind gets `WatchExpired`
        // (see `WatchChannel::subscribe`), never a silent gap.
        let replay = match (replay_from, replay_through) {
            (Some(from), Some(through)) if through > from => {
                let probe = self.replay_probe.clone();
                let replay_key = key.clone();
                self.read_call_with_pool(
                    "watch resource replay",
                    move |connection| Self::replay_events(resource_kind, connection, &replay_key, from, through, decode, &probe),
                    false,
                    ReadDeadline::Queue,
                )
                .await?
            }
            _ => ReplayedEvents::default(),
        };

        if !replay.failures.is_empty() {
            for failure in &replay.failures {
                tracing::warn!(
                    kind = resource_kind.paths.kind,
                    namespace,
                    name = %failure.name,
                    event_version = failure.event_version,
                    error = %failure.error,
                    "quarantined undecodable stored resource event"
                );
            }
            let failures = replay.failures;
            self.call("quarantine watch replay events", move |connection| Self::persist_replay_failures(connection, &key, &failures))
                .await?;
            return Err(ResourceError::WatchExpired {
                requested_version: "quarantined event leaves a sequence gap; snapshot required".to_string(),
                compacted_through: None,
            });
        }

        let replay_stream = stream::iter(replay.events.into_iter().map(Ok));
        let live_stream = stream::unfold(receiver, move |mut receiver| async move {
            receiver.next().await.map(|event| {
                (event.map(|event| SerializedWatchEvent { action: event.kind.into(), payload: WatchPayload::Shared(event) }), receiver)
            })
        });
        Ok(SerializedWatchStream::new(None, Box::pin(replay_stream.chain(live_stream))))
    }

    /// Declared-query rows decoded by `decode` on the reader, with the local
    /// position read in the same snapshot. Callers merge and sort.
    async fn query_rows<R: Send + 'static>(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
        decode: RowDecode<R>,
    ) -> Result<(Vec<(R, ResourceProvenance)>, crate::ResourcePosition), ResourceError> {
        query.validate(kind)?;
        let namespace = namespace.to_owned();
        let query = query.clone();
        let operation = query.read_operation();
        let root = self.local_root().to_string();
        let (items, position) = self
            .read_call("declared resource query", move |connection| {
                let tx = connection.transaction().map_err(|e| Self::map_sqlite(e, "begin query snapshot"))?;
                let mut items = Vec::new();
                for replicas in [false, true] {
                    if !replicas && query.replicas_only {
                        continue;
                    }
                    if replicas && (!query.include_replicas || kind.replication == crate::ReplicationClass::None) {
                        continue;
                    }
                    let (sql, args) = Self::declared_sql(kind, &namespace, &query, replicas, &root, QueryColumns::ReadRows);
                    let mut statement = tx.prepare_cached(&sql).map_err(|e| Self::map_sqlite(e, "prepare declared query"))?;
                    let rows = statement
                        .query_map(rusqlite::params_from_iter(args), |r| {
                            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?))
                        })
                        .map_err(|e| Self::map_sqlite(e, "run declared query"))?;
                    for row in rows {
                        let (body, origin, synced) = row.map_err(|e| Self::map_sqlite(e, "read declared query"))?;
                        let object = decode(kind, &body)?;
                        let provenance = if let Some(origin) = origin {
                            ResourceProvenance::Replica {
                                origin_root: NodeId::new(origin),
                                last_synced_at: DateTime::parse_from_rfc3339(&synced.unwrap_or_default())
                                    .map_err(|e| ResourceError::decode(e.to_string()))?
                                    .with_timezone(&Utc),
                            }
                        } else {
                            ResourceProvenance::Local
                        };
                        items.push((object, provenance));
                    }
                }
                let key = Self::store_key(kind, &namespace);
                let position =
                    crate::ResourcePosition { resource_version: Self::current_version(&tx, &key)?.to_string(), generation: None };
                tx.commit().map_err(|e| Self::map_sqlite(e, "commit query snapshot"))?;
                Ok((items, position))
            })
            .await?;
        if let Some(observer) = &self.read_observer {
            observer.read(kind.paths.kind, operation, items.len());
        }
        Ok((items, position))
    }

    fn validate_namespace_spec(
        resource_kind: ResourceKind,
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<(), ResourceError> {
        if !resource_kind.validate_namespace {
            return Ok(());
        }
        let mut statement = tx.prepare_cached("SELECT body_json FROM resource_objects WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name != ?5")
            .map_err(|err| Self::map_sqlite(err, "prepare namespace validation"))?;
        let rows = statement
            .query_map(params![key.0, key.1, key.2, key.3, meta.name], |row| row.get::<_, String>(0))
            .map_err(|err| Self::map_sqlite(err, "read namespace validation"))?;
        let mut siblings = Vec::new();
        for row in rows {
            let body = row.map_err(|err| Self::map_sqlite(err, "read namespace sibling"))?;
            let object = resource_kind.decode_object_text(&body)?;
            if object.metadata.deletion_timestamp.is_none() {
                siblings.push(object);
            }
        }
        resource_kind.validate_spec_with_named_siblings(meta, spec, &siblings)
    }

    fn select_existing(
        resource_kind: ResourceKind,
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        name: &str,
    ) -> Result<Option<ResourceObject<SerializedResource>>, ResourceError> {
        let body = tx
            .query_row_cached(RESOURCE_OBJECT_BODY_SQL, params![key.0, key.1, key.2, key.3, name], |row| row.get::<_, String>(0))
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource object"))?;
        body.map(|body| resource_kind.decode_object_text(&body)).transpose()
    }

    fn upsert_object(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        name: &str,
        resource_version: u64,
        body_json: &str,
    ) -> Result<(), ResourceError> {
        tx.execute_cached(
            r#"
            INSERT INTO resource_objects (group_name, version, kind, namespace, name, resource_version, body_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(group_name, version, kind, namespace, name)
            DO UPDATE SET resource_version = excluded.resource_version, body_json = excluded.body_json
            "#,
            params![key.0, key.1, key.2, key.3, name, resource_version, body_json],
        )
        .map_err(|err| Self::map_sqlite(err, "upsert sqlite resource object"))?;
        Self::clear_decode_quarantine(tx, key, name)?;
        Ok(())
    }

    fn clear_decode_quarantine(tx: &rusqlite::Transaction<'_>, key: &StoreKey, name: &str) -> Result<bool, ResourceError> {
        let deleted = tx
            .execute_cached(
                r#"
            DELETE FROM resource_decode_quarantine
            WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
            "#,
                params![key.0, key.1, key.2, key.3, name],
            )
            .map_err(|err| Self::map_sqlite(err, "clear sqlite resource decode quarantine"))?;
        Ok(deleted != 0)
    }

    fn compaction_floor(conn: &RusqliteConnection, key: &StoreKey) -> Result<u64, ResourceError> {
        Ok(conn
            .query_row_cached(
                r#"
                SELECT compacted_through FROM resource_event_compaction
                WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4
                "#,
                params![key.0, key.1, key.2, key.3],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource event compaction floor"))?
            .unwrap_or(0))
    }

    fn compacted_beyond(replay_from: u64, compacted_through: u64) -> Result<(), ResourceError> {
        if replay_from < compacted_through {
            return Err(ResourceError::WatchExpired {
                requested_version: replay_from.to_string(),
                compacted_through: Some(compacted_through.to_string()),
            });
        }
        Ok(())
    }

    /// Validate a watch start against the stream and return the version its
    /// replay runs through: the stream's current version.
    fn replay_bounds(conn: &RusqliteConnection, key: &StoreKey, replay_from: u64) -> Result<u64, ResourceError> {
        Self::compacted_beyond(replay_from, Self::compaction_floor(conn, key)?)?;
        let current_version = Self::current_version(conn, key)?;
        if replay_from > current_version {
            return Err(ResourceError::invalid(format!("resourceVersion {replay_from} is ahead of current version {current_version}")));
        }
        Ok(current_version)
    }

    /// Read and decode the events in `(replay_from, replay_through]` in one
    /// read transaction. Compaction may have advanced since registration, so
    /// the floor is checked again in the same snapshot as the events: a range
    /// it has started to delete is `WatchExpired`, never a silent gap.
    fn replay_events(
        resource_kind: ResourceKind,
        conn: &mut RusqliteConnection,
        key: &StoreKey,
        replay_from: u64,
        replay_through: u64,
        decode: RowDecode<DecodedObject>,
        probe: &ReplayProbe,
    ) -> Result<ReplayedEvents, ResourceError> {
        let tx = conn.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource event replay"))?;
        // The first read fixes the WAL snapshot for the rest of the transaction.
        let compacted_through = Self::compaction_floor(&tx, key)?;
        probe.in_snapshot();
        Self::compacted_beyond(replay_from, compacted_through)?;
        let mut statement = tx
            .prepare_cached(
                r#"
                SELECT event_version, event_type, body_json FROM resource_events
                WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND event_version > ?5 AND event_version <= ?6
                ORDER BY event_version
                "#,
            )
            .map_err(|err| Self::map_sqlite(err, "prepare sqlite resource event replay"))?;
        let rows = statement
            .query_map(params![key.0, key.1, key.2, key.3, replay_from, replay_through], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .map_err(|err| Self::map_sqlite(err, "query sqlite resource event replay"))?;
        let rows = rows.collect::<Result<Vec<_>, _>>().map_err(|err| Self::map_sqlite(err, "read sqlite resource event replay row"))?;
        drop(statement);
        drop(tx);
        let mut replayed = ReplayedEvents::default();
        for (event_version, event_type, body_json) in rows {
            // Replay quarantine uses the same decode the consumer receives.
            let decoded = StoredEventKind::from_str(&event_type)
                .and_then(|event_kind| Self::replay_event(resource_kind, event_kind, &body_json, decode));
            match decoded {
                Ok(event) => replayed.events.push(event),
                Err(error) => {
                    let name = serde_json::from_str::<Value>(&body_json)
                        .ok()
                        .and_then(|value| value.pointer("/metadata/name").and_then(Value::as_str).map(str::to_owned))
                        .unwrap_or_else(|| "<unknown>".to_string());
                    replayed.failures.push(ReplayFailure { event_version, name, body_json, error: error.to_string() })
                }
            }
        }
        Ok(replayed)
    }

    /// Record undecodable replayed events. An event compaction removed after
    /// the reader saw it is skipped: compaction also retires its quarantine.
    fn persist_replay_failures(conn: &mut RusqliteConnection, key: &StoreKey, failures: &[ReplayFailure]) -> Result<(), ResourceError> {
        let quarantined_at = Utc::now().to_rfc3339();
        let tx = conn.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource event decode quarantine"))?;
        let compacted_through = Self::compaction_floor(&tx, key)?;
        for failure in failures.iter().filter(|failure| failure.event_version > compacted_through) {
            tx.execute_cached(
                r#"
                INSERT INTO resource_event_decode_quarantine
                    (group_name, version, kind, namespace, event_version, name, body_json, error, quarantined_at)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(group_name, version, kind, namespace, event_version)
                DO UPDATE SET name = excluded.name,
                              body_json = excluded.body_json,
                              error = excluded.error,
                              quarantined_at = excluded.quarantined_at
                "#,
                params![key.0, key.1, key.2, key.3, failure.event_version, failure.name, failure.body_json, failure.error, quarantined_at],
            )
            .map_err(|err| Self::map_sqlite(err, "persist sqlite resource event decode quarantine"))?;
        }
        tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource event decode quarantine"))
    }
}

#[async_trait::async_trait]
impl Store for SqliteBackend {
    fn blocking_request_encoding(&self) -> bool {
        true
    }
    fn durable_local(&self) -> bool {
        true
    }
    fn with_local_root(&self, root: NodeId) -> std::sync::Arc<dyn Store> {
        std::sync::Arc::new(self.clone().with_local_root(root))
    }
    fn local_root(&self) -> Result<NodeId, ResourceError> {
        Ok(self.local_root())
    }
    fn hierarchy_admission(&self) -> Option<std::sync::Arc<tokio::sync::Mutex<()>>> {
        Some(self.hierarchy_admission.clone())
    }
    fn with_read_observer(&self, observer: std::sync::Arc<dyn ReadObserver>) -> std::sync::Arc<dyn Store> {
        std::sync::Arc::new(self.clone().with_read_observer(observer))
    }
    fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.read_observer()
    }
    async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        self.diagnostics().await.map(Some)
    }
    async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        self.record_field_ownership_violation(violation).await
    }
    async fn query_raw(&self, kind: ResourceKind, namespace: &str, query: &crate::StoreQuery) -> Result<crate::QueryResult, ResourceError> {
        let (items, position) = self.query_rows(kind, namespace, query, RowForm::serialized).await?;
        let items = items.into_iter().map(|(object, provenance)| ReadResourceObject { object, provenance }).collect();
        let mut items = crate::merge_query_definitions(query, items, &self.local_root())?;
        crate::sort_query_items(&mut items);
        Ok(crate::QueryResult { items, position, local_root: Some(self.local_root()) })
    }
    async fn query_decoded(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
    ) -> Result<crate::DecodedQueryResult, ResourceError> {
        // A causal merge of definition sources works on the stored trees.
        if query.merged_definitions {
            return Ok(crate::DecodedQueryResult::serialized(self.query_raw(kind, namespace, query).await?));
        }
        let (rows, position) = self.query_rows(kind, namespace, query, RowForm::typed).await?;
        let mut items: Vec<_> = rows.into_iter().map(|(object, provenance)| DecodedReadObject { object, provenance }).collect();
        crate::sort_query_rows(&mut items, |item| (item.object.name(), &item.provenance));
        Ok(crate::DecodedQueryResult { items, position, local_root: Some(self.local_root()) })
    }
    async fn query_values_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query: &crate::StoreQuery,
        field: &str,
    ) -> Result<std::collections::BTreeSet<String>, ResourceError> {
        query.validate(kind)?;
        if !kind.queryable_fields.contains(&field) {
            return Err(ResourceError::invalid("undeclared field projection"));
        }
        let namespace = namespace.to_owned();
        let query = query.clone();
        let field = field.to_owned();
        let root = self.local_root().to_string();
        self.read_call("indexed field projection", move |connection| {
            let mut values = std::collections::BTreeSet::new();
            for replicas in [false, true] {
                if (!replicas && query.replicas_only)
                    || (replicas && (!query.include_replicas || kind.replication == crate::ReplicationClass::None))
                {
                    continue;
                }
                let (sql, args) = Self::declared_sql(kind, &namespace, &query, replicas, &root, QueryColumns::Field(field.clone()));
                let mut statement = connection.prepare_cached(&sql).map_err(|e| Self::map_sqlite(e, "prepare field projection"))?;
                let rows = statement
                    .query_map(rusqlite::params_from_iter(args), |row| row.get::<_, Option<String>>(0))
                    .map_err(|e| Self::map_sqlite(e, "query field projection"))?;
                for row in rows {
                    if let Some(value) = row.map_err(|e| Self::map_sqlite(e, "read field projection"))? {
                        values.insert(value);
                    }
                }
            }
            Ok(values)
        })
        .await
    }
    async fn query_messages_raw(
        &self,
        namespace: &str,
        query: &MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        let result = self.query_messages(namespace, query, include_replicas).await?;
        if let Some(observer) = self.read_observer() {
            observer.read("Message", ReadOperation::Query, result.len());
        }
        result.into_iter().map(convert_read).collect()
    }
    async fn delete_decode_quarantine_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        SqliteBackend::delete_decode_quarantine_raw(self, resource_kind, namespace, name).await
    }
    async fn namespace_inventory(&self) -> Result<BTreeMap<String, Vec<String>>, ResourceError> {
        self.read_call_with_pool("discovery namespace inventory", Self::read_namespace_inventory, true, ReadDeadline::Total).await
    }
    async fn local_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        SqliteBackend::local_namespaces_raw(self, resource_kind).await
    }
    async fn stored_namespaces_raw(&self, resource_kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        SqliteBackend::stored_namespaces_raw(self, resource_kind).await
    }
    async fn list_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        let result = SqliteBackend::list_replicas_raw(self, resource_kind, namespace).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::List, result.len());
        }
        Ok(result)
    }
    async fn get_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        let result = SqliteBackend::get_replicas_raw(self, resource_kind, namespace, name).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::Get, result.len());
        }
        Ok(result)
    }
    async fn watch_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<SerializedReadWatchEvent, ResourceError>>, ResourceError> {
        SqliteBackend::watch_replicas_raw(self, resource_kind, namespace).await
    }
    async fn replace_replicas_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        listed: &ResourceList<SerializedResource>,
        synced_at: chrono::DateTime<Utc>,
        bucket: Option<u8>,
    ) -> Result<(), ResourceError> {
        SqliteBackend::replace_replicas_raw(self, resource_kind, origin_root, namespace, listed, synced_at, bucket).await
    }
    async fn apply_replica_batch_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        events: &[(WatchEvent<SerializedResource>, DateTime<Utc>)],
    ) -> Result<(), ResourceError> {
        for batch in events.chunks(crate::replica::REPLICA_BATCH_SIZE) {
            let mutations = batch
                .iter()
                .map(|(event, synced_at)| {
                    let (kind, name, value) = match event {
                        WatchEvent::Added(object) => {
                            (StoredReplicaEventKind::Added, object.metadata.name.clone(), Self::encode_object(resource_kind, object)?)
                        }
                        WatchEvent::Modified(object) => {
                            (StoredReplicaEventKind::Modified, object.metadata.name.clone(), Self::encode_object(resource_kind, object)?)
                        }
                        WatchEvent::Deleted(object) => {
                            (StoredReplicaEventKind::Deleted, object.metadata.name.clone(), Self::encode_object(resource_kind, object)?)
                        }
                        WatchEvent::DeletedByName(tombstone) => {
                            (StoredReplicaEventKind::Deleted, tombstone.name.clone(), Self::encode_tombstone(resource_kind, tombstone))
                        }
                    };
                    Ok(ReplicaMutation { kind, name, value, synced_at: *synced_at })
                })
                .collect::<Result<Vec<_>, ResourceError>>()?;
            self.apply_replica_values(resource_kind, origin_root, namespace, mutations).await?;
        }
        Ok(())
    }
    async fn apply_replica_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        kind: StoredReplicaEventKind,
        object: &ResourceObject<SerializedResource>,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        SqliteBackend::apply_replica_raw(self, resource_kind, origin_root, namespace, kind, object, synced_at).await
    }
    async fn list_replica_deletions_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
    ) -> Result<Vec<crate::replica::ReplicaDeletion>, ResourceError> {
        let key = Self::store_key(resource_kind, namespace);
        self.read_call("list replica deletions", move |connection| {
            let mut statement = connection
                .prepare_cached(REPLICA_DELETIONS_SELECT_SQL)
                .map_err(|error| Self::map_sqlite(error, "prepare replica deletions"))?;
            let rows = statement
                .query_map(params![key.0, key.1, key.2, key.3], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                })
                .map_err(|error| Self::map_sqlite(error, "query replica deletions"))?;
            rows.map(|row| {
                let (origin, name, synced_at) = row.map_err(|error| Self::map_sqlite(error, "read replica deletion"))?;
                let last_synced_at = DateTime::parse_from_rfc3339(&synced_at)
                    .map_err(|error| ResourceError::decode(format!("decode replica deletion timestamp: {error}")))?
                    .with_timezone(&Utc);
                Ok(crate::replica::ReplicaDeletion { origin_root: NodeId::new(origin), name, last_synced_at })
            })
            .collect()
        })
        .await
    }
    async fn apply_replica_tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        tombstone: &ResourceTombstone,
        synced_at: chrono::DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        SqliteBackend::apply_replica_tombstone_raw(self, resource_kind, origin_root, namespace, tombstone, synced_at).await
    }
    async fn invalidate_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<(), ResourceError> {
        SqliteBackend::invalidate_replica_cursor_raw(self, resource_kind, origin_root, namespace).await
    }
    async fn advance_replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
        previous: &ReplicaCursor,
        next: &str,
    ) -> Result<(), ResourceError> {
        SqliteBackend::advance_replica_cursor_raw(self, resource_kind, origin_root, namespace, previous, next).await
    }
    async fn replica_cursor_raw(
        &self,
        resource_kind: ResourceKind,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        SqliteBackend::replica_cursor_raw(self, resource_kind, origin_root, namespace).await
    }
    async fn get_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        let result = SqliteBackend::get_raw(self, resource_kind, namespace, name).await;
        if let Some(observer) = self.read_observer() {
            // Missing, storage and decode errors still cost a crossing, but
            // return zero objects to the caller.
            observer.read(resource_kind.paths.kind, ReadOperation::Get, usize::from(result.is_ok()));
        }
        result
    }
    async fn current_position_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourcePosition, ResourceError> {
        SqliteBackend::current_position_raw(self, resource_kind, namespace).await
    }
    async fn digest_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        query: &DigestQuery,
    ) -> Result<PartitionDigest, ResourceError> {
        SqliteBackend::digest_raw(self, resource_kind, namespace, query).await
    }
    async fn confirm_replica_digest_raw(
        &self,
        resource_kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        proof: &PartitionDigest,
        previous: &ReplicaCursor,
    ) -> Result<(), ResourceError> {
        SqliteBackend::confirm_replica_digest_raw(self, resource_kind, origin, namespace, proof, previous).await
    }
    async fn replica_digest_raw(
        &self,
        resource_kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        generation: Option<String>,
    ) -> Result<PartitionDigest, ResourceError> {
        SqliteBackend::replica_digest_raw(self, resource_kind, origin, namespace, generation).await
    }
    async fn list_raw(&self, resource_kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let result = SqliteBackend::list_raw(self, resource_kind, namespace).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::List, result.items.len());
        }
        Ok(result)
    }
    async fn list_raw_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        let result = SqliteBackend::list_raw_matching_labels(self, resource_kind, namespace, required).await?;
        if let Some(observer) = self.read_observer() {
            observer.read(resource_kind.paths.kind, ReadOperation::Query, result.items.len());
        }
        Ok(result)
    }
    async fn create_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        SqliteBackend::create_raw(self, resource_kind, namespace, meta, spec).await
    }
    async fn create_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        SqliteBackend::create_definition_raw(self, resource_kind, namespace, meta, spec, merge).await
    }
    async fn update_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        SqliteBackend::update_raw(self, resource_kind, namespace, meta, resource_version, spec).await
    }
    async fn update_definition_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        SqliteBackend::update_definition_raw(self, resource_kind, namespace, meta, resource_version, spec, merge).await
    }
    async fn update_status_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        SqliteBackend::update_status_raw(self, resource_kind, namespace, name, resource_version, status).await
    }
    async fn delete_raw(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        SqliteBackend::delete_raw(self, resource_kind, namespace, name).await
    }
    async fn tombstone_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<watch::TombstoneWrite, ResourceError> {
        SqliteBackend::tombstone_raw(self, resource_kind, namespace, name, minimum_resource_version).await
    }
    async fn watch_raw(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<SerializedWatchStream, ResourceError> {
        SqliteBackend::watch_raw(self, resource_kind, namespace, start).await
    }
    async fn get_decoded(&self, resource_kind: ResourceKind, namespace: &str, name: &str) -> Result<DecodedObject, ResourceError> {
        let result = SqliteBackend::get_decoded(self, resource_kind, namespace, name).await;
        // Missing, storage and decode errors still cost a crossing, but
        // return zero objects to the caller, as for `get_raw`.
        self.observe(resource_kind, ReadOperation::Get, usize::from(result.is_ok()));
        result
    }
    async fn list_decoded(&self, resource_kind: ResourceKind, namespace: &str) -> Result<DecodedList, ResourceError> {
        let result = SqliteBackend::list_decoded(self, resource_kind, namespace).await?;
        self.observe(resource_kind, ReadOperation::List, result.items.len());
        Ok(result)
    }
    async fn list_decoded_matching_labels(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<DecodedList, ResourceError> {
        let result = SqliteBackend::list_decoded_matching_labels(self, resource_kind, namespace, required).await?;
        self.observe(resource_kind, ReadOperation::Query, result.items.len());
        Ok(result)
    }
    async fn list_replicas_decoded(&self, resource_kind: ResourceKind, namespace: &str) -> Result<Vec<DecodedReadObject>, ResourceError> {
        let result = SqliteBackend::list_replicas_decoded(self, resource_kind, namespace).await?;
        self.observe(resource_kind, ReadOperation::List, result.len());
        Ok(result)
    }
    async fn get_replicas_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<DecodedReadObject>, ResourceError> {
        let result = SqliteBackend::get_replicas_decoded(self, resource_kind, namespace, name).await?;
        self.observe(resource_kind, ReadOperation::Get, result.len());
        Ok(result)
    }
    async fn query_messages_decoded(
        &self,
        namespace: &str,
        query: &MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<crate::Message>>, ResourceError> {
        let result = self.query_messages(namespace, query, include_replicas).await?;
        self.observe(ResourceKind::of::<crate::Message>(), ReadOperation::Query, result.len());
        Ok(result)
    }
    async fn query_message_replicas_decoded(
        &self,
        namespace: &str,
        query: &MessageQuery,
    ) -> Result<Vec<ReadResourceObject<crate::Message>>, ResourceError> {
        let result = self.query_message_replicas(namespace, query).await?;
        self.observe(ResourceKind::of::<crate::Message>(), ReadOperation::Query, result.len());
        Ok(result)
    }
    async fn watch_decoded(
        &self,
        resource_kind: ResourceKind,
        namespace: &str,
        start: WatchStart,
    ) -> Result<SerializedWatchStream, ResourceError> {
        SqliteBackend::watch_decoded(self, resource_kind, namespace, start).await
    }
}
#[cfg(test)]
mod latency_tests {
    use std::{sync::mpsc, time::Duration};

    use super::SqliteBackend;
    use crate::{Host, HostSpec, HostStatus, InputMeta, Resource, ResourceBackend};

    #[tokio::test]
    async fn read_completes_while_writer_is_stalled_in_fake_fsync() {
        let directory = tempfile::tempdir().expect("store directory");
        let backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
        let hosts = ResourceBackend::new(backend.clone()).using::<Host>("flotilla");
        hosts.create(&InputMeta::builder().name("feta".to_string()).build(), &HostSpec::default()).await.expect("create host");
        let writer_connection = backend.connection.clone();

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = tokio::spawn(async move {
            backend
                .connection
                .call(move |_| {
                    entered_tx.send(()).expect("announce fake fsync");
                    release_rx.recv().expect("release fake fsync");
                    Ok::<_, crate::ResourceError>(())
                })
                .await
                .expect("fake fsync completes");
        });
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("writer entered fake fsync")).await.expect("wait for writer");

        assert!(
            tokio::time::timeout(Duration::from_millis(100), writer_connection.call(|_| Ok::<_, crate::ResourceError>(()))).await.is_err(),
            "an operation queued on the writer connection must wait for fake fsync"
        );

        let read = tokio::time::timeout(Duration::from_millis(250), async {
            let host = hosts.get("feta").await?;
            let listed = hosts.list().await?;
            Ok::<_, crate::ResourceError>((host, listed))
        })
        .await;
        release_tx.send(()).expect("release writer");
        writer.await.expect("writer task");
        let (host, listed) = read.expect("reads must not queue behind writer durability").expect("read hosts");
        assert_eq!(host.metadata.name, "feta");
        assert_eq!(listed.items.len(), 1);
    }

    #[tokio::test]
    async fn busy_read_returns_an_error_before_client_timeout() {
        let backend = SqliteBackend::open_in_memory().expect("open in-memory store");
        let metrics = backend.load_metrics();
        let hosts = ResourceBackend::new(backend.clone()).using::<Host>("flotilla");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = tokio::spawn(async move {
            backend
                .connection
                .call(move |_| {
                    entered_tx.send(()).expect("announce blocked writer");
                    release_rx.recv().expect("release blocked writer");
                    Ok::<_, crate::ResourceError>(())
                })
                .await
                .expect("blocked writer completes");
        });
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("writer entered")).await.expect("wait for writer");

        let result = tokio::time::timeout(Duration::from_secs(3), hosts.get("feta")).await;
        release_tx.send(()).expect("release writer");
        writer.await.expect("writer task");
        let error = result.expect("read must answer before client timeout").expect_err("store must report busy");
        // Exercise the actual producer, not a separately spelled test error.
        // One refusal returned to the caller is one read_contention event.
        assert_eq!(metrics.read_contention.get(), 1);
        assert!(error.is_store_busy(), "SQLite's deadline must retain its retry category: {error}");
        assert_eq!(error.to_string(), crate::ResourceError::STORE_BUSY_READ_DEADLINE_MESSAGE);
    }

    #[tokio::test]
    async fn file_backed_read_reports_busy_when_its_read_worker_is_blocked() {
        let directory = tempfile::tempdir().expect("store directory");
        let backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
        let hosts = ResourceBackend::new(backend.clone()).using::<Host>("flotilla");
        let read_connection = backend.read_connections[super::READER_COUNT - 2].clone();
        let _other_readers = backend.reader_slots.acquire_many((super::READER_COUNT - 2) as u32).await.unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocked_read = tokio::spawn(async move {
            read_connection
                .call(move |_| {
                    entered_tx.send(()).expect("announce blocked read");
                    release_rx.recv().expect("release blocked read");
                    Ok::<_, crate::ResourceError>(())
                })
                .await
                .expect("blocked read completes");
        });
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("read entered")).await.expect("wait for read");

        let result = tokio::time::timeout(Duration::from_secs(3), hosts.list()).await;
        release_tx.send(()).expect("release read");
        blocked_read.await.expect("read task");
        assert!(result
            .expect("read must answer before client timeout")
            .expect_err("store must report busy")
            .to_string()
            .contains("store busy"));
    }

    // Position must consult only the stream counter, without decoding or
    // quarantining unrelated resource bodies.
    #[tokio::test]
    async fn current_position_does_not_decode_objects() {
        let backend = SqliteBackend::open_in_memory().expect("store");
        let hosts = ResourceBackend::new(backend.clone()).using::<Host>("flotilla");
        let created = hosts.create(&InputMeta::builder().name("broken".into()).build(), &HostSpec::default()).await.expect("create");
        backend
            .connection
            .call(|connection| {
                connection
                    .execute("UPDATE resource_objects SET body_json = 'invalid JSON'", [])
                    .map_err(|error| crate::ResourceError::other(error.to_string()))?;
                Ok::<_, crate::ResourceError>(())
            })
            .await
            .expect("corrupt body");
        let position = hosts.current_position().await.expect("position despite invalid body");
        assert_eq!(position.resource_version, created.metadata.resource_version);
        assert_eq!(position.generation, None);
        assert!(backend.diagnostics().await.expect("diagnostics").decode_quarantines.is_empty());
    }

    #[tokio::test]
    async fn unconfigured_writer_remains_compatible_and_native_open_repairs_values() {
        use crate::StoreExt;
        use chrono::Utc;
        use flotilla_protocol::NodeId;
        use flotilla_resources::{CharterPointer, Project, ProjectRepositorySpec, ProjectSpec, RepositoryKey};
        use serde_json::Value;
        use std::collections::BTreeMap;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resources.sqlite");
        let backend = ResourceBackend::new(SqliteBackend::open(&path).unwrap());
        let spec = ProjectSpec::builder()
            .display_name("External".into())
            .repositories(vec![ProjectRepositorySpec::builder().repo(RepositoryKey("old".into())).build()])
            .charter(CharterPointer::Repository { repo: "old".into(), branch: "main".into(), path: "".into() })
            .build();
        let object =
            backend.definitions::<Project>("flotilla").create(&InputMeta::builder().name("external".into()).build(), &spec).await.unwrap();
        backend
            .replace_replicas_typed::<Project>(
                &NodeId::new("foreign"),
                "flotilla",
                &crate::ResourceList { items: vec![object], resource_version: "1".into(), generation: None },
                Utc::now(),
                None,
            )
            .await
            .unwrap();
        drop(backend);
        // A plain connection neither registers functions nor installs TEMP triggers.
        let connection = rusqlite::Connection::open(&path).unwrap();
        let references: i64 = connection
            .query_row("SELECT count(*) FROM sqlite_schema WHERE instr(sql,'flotilla_declared_values')>0", [], |row| row.get(0))
            .unwrap();
        assert_eq!(references, 0, "the database file must not depend on our custom function");
        let charter = r#"{"repo":"new","path":"","kind":"repository","branch":"main"}"#;
        for table in ["resource_objects", "replica_objects"] {
            assert_eq!(connection.execute(&format!("UPDATE {table} SET body_json=json_set(body_json,'$.spec.repositories',json('[{{\"repo\":\"new\"}}]'),'$.spec.charter',json(?1)) WHERE kind='Project' AND name='external'"),[charter]).unwrap(),1);
            let index = SqliteBackend::field_index(table, crate::ResourceKind::of::<Project>(), "spec.repositories[].repo");
            let old: i64 =
                connection.query_row(&format!("SELECT count(*) FROM {index}_values WHERE value='old'"), [], |row| row.get(0)).unwrap();
            assert_eq!(old, 1, "unknown writer deliberately leaves the derived table stale");
        }
        let before: String =
            connection.query_row("SELECT body_json FROM resource_objects WHERE kind='Project'", [], |row| row.get(0)).unwrap();
        drop(connection);
        for asynchronous in [false, true] {
            let native = if asynchronous { SqliteBackend::open_async(&path).await.unwrap() } else { SqliteBackend::open(&path).unwrap() };
            let backend = ResourceBackend::new(native);
            for (field, value) in
                [("spec.repositories[].repo", "new".into()), ("spec.charter", serde_json::from_str::<Value>(charter).unwrap().to_string())]
            {
                let rows = backend
                    .query_declared::<Project>(
                        "flotilla",
                        &crate::StoreQuery {
                            fields: BTreeMap::from([(field.into(), value)]),
                            include_replicas: true,
                            ..Default::default()
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(rows.items.len(), 2, "both local and replica projections repair on open");
            }
            let old = backend
                .query_declared::<Project>(
                    "flotilla",
                    &crate::StoreQuery {
                        fields: BTreeMap::from([("spec.repositories[].repo".into(), "old".into())]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert!(old.items.is_empty());
            let connection = rusqlite::Connection::open(&path).unwrap();
            let after: String =
                connection.query_row("SELECT body_json FROM resource_objects WHERE kind='Project'", [], |row| row.get(0)).unwrap();
            assert_eq!(before, after, "repair must not rewrite resource bodies");
        }
        // Deletion by an old writer must also be reflected on the next open.
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute("DELETE FROM replica_objects WHERE kind='Project'", []).unwrap();
        drop(connection);
        let backend = ResourceBackend::new(SqliteBackend::open(&path).unwrap());
        let rows = backend
            .query_declared::<Project>(
                "flotilla",
                &crate::StoreQuery {
                    fields: BTreeMap::from([("spec.repositories[].repo".into(), "new".into())]),
                    include_replicas: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(rows.items.len(), 1, "repair removes orphaned derived values");
    }

    #[tokio::test]
    async fn list_returns_valid_items_without_waiting_or_queueing_duplicate_cleanups() {
        let directory = tempfile::tempdir().expect("store directory");
        let path = directory.path().join("resources.sqlite");
        let backend = SqliteBackend::open(&path).expect("open store");
        let hosts = ResourceBackend::new(backend.clone()).using::<Host>("flotilla");
        for name in ["healthy", "repaired"] {
            hosts.create(&InputMeta::builder().name(name.to_string()).build(), &HostSpec::default()).await.expect("create host");
        }
        let original_body = backend
            .connection
            .call(|connection| {
                connection
                    .query_row(
                        "SELECT body_json FROM resource_objects WHERE kind = ?1 AND name = 'repaired'",
                        [Host::API_PATHS.kind],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|error| crate::ResourceError::other(error.to_string()))
            })
            .await
            .expect("read original body");
        backend
            .connection
            .call(|connection| {
                connection
                    .execute("UPDATE resource_objects SET body_json = '{}' WHERE kind = ?1 AND name = 'repaired'", [Host::API_PATHS.kind])
                    .map_err(|error| crate::ResourceError::other(error.to_string()))?;
                Ok::<_, crate::ResourceError>(())
            })
            .await
            .expect("corrupt row");

        let writer_connection = backend.connection.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let blocked_writer = tokio::spawn(async move {
            writer_connection
                .call(move |_| {
                    entered_tx.send(()).expect("announce blocked writer");
                    release_rx.recv().expect("release blocked writer");
                    Ok::<_, crate::ResourceError>(())
                })
                .await
                .expect("blocked writer completes");
        });
        tokio::task::spawn_blocking(move || entered_rx.recv().expect("writer entered")).await.expect("wait for writer");

        let listed = tokio::time::timeout(Duration::from_secs(1), hosts.list())
            .await
            .expect("list returns without waiting for writer cleanup")
            .expect("valid item survives queued cleanup");
        assert_eq!(listed.items.len(), 1);
        assert_eq!(listed.items[0].metadata.name, "healthy");
        for _ in 0..3 {
            assert_eq!(hosts.list().await.expect("repeat list").items.len(), 1);
        }
        assert_eq!(backend.pending_cleanups.lock().expect("cleanup set").len(), 1);

        let repair = rusqlite::Connection::open(&path).expect("open independent writer");
        SqliteBackend::register_query_functions(&repair).expect("configure independent writer");
        repair
            .execute(
                "UPDATE resource_objects SET body_json = ?1 WHERE kind = ?2 AND name = 'repaired'",
                rusqlite::params![original_body, Host::API_PATHS.kind],
            )
            .expect("repair row before queued cleanup");
        release_tx.send(()).expect("release writer");
        blocked_writer.await.expect("writer task");
        tokio::time::timeout(Duration::from_secs(3), async {
            while !backend.pending_cleanups.lock().expect("cleanup set").is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("queued cleanup finishes");
        assert!(backend.diagnostics().await.expect("diagnostics").decode_quarantines.is_empty());
        assert_eq!(hosts.get("repaired").await.expect("repaired row remains").metadata.name, "repaired");
    }

    #[tokio::test]
    async fn list_version_matches_its_object_snapshot_during_status_writes() {
        let directory = tempfile::tempdir().expect("store directory");
        let backend = SqliteBackend::open(directory.path().join("resources.sqlite")).expect("open store");
        let readers = ResourceBackend::new(backend.clone()).using::<Host>("flotilla");
        let writers = ResourceBackend::new(backend).using::<Host>("flotilla");
        let created =
            readers.create(&InputMeta::builder().name("feta".to_string()).build(), &HostSpec::default()).await.expect("create host");
        let writer = tokio::spawn(async move {
            let mut current = created;
            for iteration in 0..100 {
                current = writers
                    .update_status(
                        "feta",
                        &current.metadata.resource_version,
                        &HostStatus { ready: iteration % 2 == 0, ..Default::default() },
                    )
                    .await
                    .expect("update status");
            }
        });
        for _ in 0..100 {
            let listed = readers.list().await.expect("list host");
            assert_eq!(listed.items.len(), 1);
            assert_eq!(listed.resource_version, listed.items[0].metadata.resource_version);
        }
        writer.await.expect("writer task");
    }
}

#[cfg(test)]
mod tests {
    // Suppressed pressure warnings are counted, with one publication per 30 s.
    #[test]
    fn pressure_warnings_aggregate_instead_of_writing_per_read() {
        let mut warnings = super::ReadWarnings::default();
        let now = std::time::Instant::now();
        assert!(warnings.record(1, 0, 0, now));
        for _ in 0..1000 {
            assert!(!warnings.record(1, 1, 1, now + std::time::Duration::from_secs(29)));
        }
        assert_eq!((warnings.slow, warnings.abandoned, warnings.deadlines), (1001, 1000, 1000));
        assert!(warnings.record(0, 0, 0, now + std::time::Duration::from_secs(30)));
    }

    use flotilla_resources::Resource;
    use tempfile::tempdir;

    use super::*;
    use crate::{Environment, EnvironmentSpec, InputMeta};

    // Transient engine failures fail the whole batch and never create a bad-data
    // quarantine or expose preceding writes. A SQLite callback injects real
    // busy/full/I/O codes at the engine boundary during the second object write.
    #[rstest::rstest]
    #[case(rusqlite::ffi::SQLITE_BUSY)]
    #[case(rusqlite::ffi::SQLITE_FULL)]
    #[case(rusqlite::ffi::SQLITE_IOERR)]
    #[tokio::test]
    async fn transient_replica_error_aborts_batch(#[case] code: i32) {
        let backend = SqliteBackend::open_in_memory().unwrap();
        backend.call("install transient error injection", move |connection| {
            connection.create_scalar_function("transient_replica_failure", 0, rusqlite::functions::FunctionFlags::SQLITE_UTF8, move |_| {
                Err::<i64, _>(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some("injected transient failure".into())))
            }).map_err(|err| SqliteBackend::map_sqlite(err, "install transient callback"))?;
            connection.execute_batch("CREATE TEMP TRIGGER transient_replica_failure AFTER INSERT ON replica_objects WHEN NEW.name='bad' BEGIN SELECT transient_replica_failure(); END;")
                .map_err(|err| SqliteBackend::map_sqlite(err, "install transient trigger"))?;
            Ok(())
        }).await.unwrap();
        let kind = ResourceKind::of::<Environment>();
        let at = Utc::now();
        let mut events = Vec::new();
        for name in ["before", "bad", "after"] {
            let object = backend
                .create_raw(
                    kind,
                    "flotilla",
                    &InputMeta::builder().name(name.into()).build(),
                    &serde_json::to_value(EnvironmentSpec { cloud_sandbox: None, provider_address: None, host_direct: None, docker: None })
                        .unwrap(),
                )
                .await
                .unwrap();
            events.push((WatchEvent::Added(object), at));
        }
        let error = backend
            .apply_replica_batch_raw(kind, &NodeId::new("peer"), "flotilla", &events)
            .await
            .expect_err("engine failures must fail the batch");
        assert!(error.to_string().contains("injected transient failure"), "preserve original engine failure: {error}");
        assert!(backend.list_replicas_raw(kind, "flotilla").await.unwrap().is_empty(), "preceding writes must roll back");
        assert!(backend.diagnostics().await.unwrap().replica_quarantines.is_empty());
    }

    // Opening an N-generation database backfills local/replica relation indexes
    // without rewriting stored bodies; reopening the migrated schema is idempotent.
    #[tokio::test]
    async fn query_migrations_backfill_existing_bodies() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("store.sqlite");
        let backend = crate::ResourceBackend::new(SqliteBackend::open(&path).unwrap());
        let meta = InputMeta::builder().name("stored".into()).labels(BTreeMap::from([("team".into(), "target".into())])).build();
        let local = backend
            .using::<crate::Convoy>("flotilla")
            .create(
                &meta,
                &crate::ConvoySpec::builder()
                    .workflow_ref("workflow".into())
                    .repositories(vec![crate::ConvoyRepositorySpec::builder()
                        .url("https://example.com/repo".into())
                        .repo_ref(crate::RepositoryKey("repository".into()))
                        .source_ref("main".into())
                        .target_ref("main".into())
                        .workspace_slug("repo".into())
                        .subpaths(Vec::new())
                        .build()])
                    .build(),
            )
            .await
            .unwrap();
        backend
            .replace_replicas_typed::<crate::Convoy>(
                &NodeId::new("remote"),
                "flotilla",
                &ResourceList { items: vec![local], resource_version: "1".into(), generation: None },
                Utc::now(),
                None,
            )
            .await
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        let before: String = connection.query_row("SELECT body_json FROM resource_objects", [], |r| r.get(0)).unwrap();
        for table in ["resource_objects", "replica_objects"] {
            for action in ["insert", "update", "delete"] {
                connection.execute_batch(&format!("DROP TRIGGER {table}_query_{action}")).unwrap();
            }
            connection.execute_batch(&format!("DROP TABLE {table}_labels; DROP TABLE {table}_owners;")).unwrap();
        }
        connection.execute("DELETE FROM resource_store_migrations WHERE name IN ('resource-labels-v1','replica-labels-v1')", []).unwrap();
        let kind = crate::ResourceKind::of::<crate::Convoy>();
        for table in ["resource_objects", "replica_objects"] {
            let index = SqliteBackend::field_index(table, kind, "spec.repositories.*.repo_ref");
            for action in ["insert", "update", "delete"] {
                connection.execute_batch(&format!("DROP TRIGGER IF EXISTS {index}_{action}")).unwrap();
            }
            connection.execute_batch(&format!("DROP TABLE {index}_values")).unwrap();
            connection.execute("DELETE FROM resource_store_migrations WHERE name=?1", [index]).unwrap();
        }

        drop(connection);
        for _ in 0..2 {
            let reopened = crate::ResourceBackend::new(SqliteBackend::open(&path).unwrap());
            let selected = reopened
                .query_declared::<crate::Convoy>(
                    "flotilla",
                    &crate::StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), true, true),
                )
                .await
                .unwrap();
            assert_eq!(selected.items.len(), 2);
            let indexed = reopened
                .query_declared::<crate::Convoy>(
                    "flotilla",
                    &crate::StoreQuery {
                        fields: BTreeMap::from([("spec.repositories.*.repo_ref".into(), "repository".into())]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(indexed.items.len(), 2, "named array migration backfills both partitions");

            let connection = rusqlite::Connection::open(&path).unwrap();
            let after: String = connection.query_row("SELECT body_json FROM resource_objects", [], |r| r.get(0)).unwrap();
            assert_eq!(before, after);
        }
    }

    // SQL literals/quoted identifiers keep question marks; bare and numbered
    // bindings preserve their shared positional identity without panicking.
    #[test]
    fn nested_sql_binding_shift_preserves_literals() {
        assert_eq!(
            SqliteBackend::shift_bindings("SELECT '?1', 'it''s ?', \"?2\", [?3], `?4`, ?1, ?, ?2", 5),
            "SELECT '?1', 'it''s ?', \"?2\", [?3], `?4`, ?6, ?7, ?7"
        );
        assert_eq!(SqliteBackend::shift_bindings("SELECT ?", 0), "SELECT ?1");
        assert_eq!(SqliteBackend::shift_bindings("SELECT ?9999999999999999999999999999", 5), "SELECT ?9999999999999999999999999999");
    }

    // Every exposed field equality and normalized relation must use its declared
    // index for both authoritative rows and replicas, before decoding any body.
    #[tokio::test]
    async fn additive_collection_migration_backfills_existing_c1_index() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("upgrade.sqlite");
        let backend = crate::ResourceBackend::new(SqliteBackend::open(&path).unwrap());
        let project = backend
            .definitions::<flotilla_resources::Project>("flotilla")
            .create(
                &crate::InputMeta::builder().name("existing".into()).build(),
                &flotilla_resources::ProjectSpec::builder()
                    .display_name("Existing".into())
                    .repositories(vec![flotilla_resources::ProjectRepositorySpec::builder()
                        .repo(flotilla_resources::RepositoryKey("target".into()))
                        .build()])
                    .build(),
            )
            .await
            .unwrap();
        backend
            .replace_replicas_typed::<flotilla_resources::Project>(
                &NodeId::new("foreign"),
                "flotilla",
                &crate::ResourceList { items: vec![project], resource_version: "1".into(), generation: None },
                Utc::now(),
                None,
            )
            .await
            .unwrap();
        let connection = rusqlite::Connection::open(&path).unwrap();
        SqliteBackend::register_query_functions(&connection).unwrap();
        let before: String =
            connection.query_row("SELECT body_json FROM resource_objects WHERE kind='Project'", [], |row| row.get(0)).unwrap();
        let kind = crate::ResourceKind::of::<flotilla_resources::Project>();
        for table in ["resource_objects", "replica_objects"] {
            let current = SqliteBackend::field_index(table, kind, "spec.repositories[].repo");
            let legacy = current.strip_suffix("_collection_v2").unwrap();
            // Retain a C1-shaped relation and its migration record. The new
            // migration must not mistake that members table for its values table.
            connection.execute_batch(&format!("CREATE TABLE {legacy}_members AS SELECT * FROM {current}_values; CREATE INDEX {legacy} ON {legacy}_members(group_name,version,kind,namespace,value,name,origin);")).unwrap();
            connection.execute("INSERT INTO resource_store_migrations(name) VALUES (?1)", [legacy]).unwrap();
            for action in ["insert", "update", "delete"] {
                connection.execute_batch(&format!("DROP TRIGGER IF EXISTS {current}_{action}")).unwrap();
            }
            connection.execute_batch(&format!("DROP TABLE {current}_values")).unwrap();
            connection.execute("DELETE FROM resource_store_migrations WHERE name=?1", [&current]).unwrap();
        }
        drop(connection);
        for _ in 0..2 {
            let reopened = crate::ResourceBackend::new(SqliteBackend::open(&path).unwrap());
            let selected = reopened
                .query_declared::<flotilla_resources::Project>(
                    "flotilla",
                    &crate::StoreQuery {
                        fields: BTreeMap::from([("spec.repositories[].repo".into(), "target".into())]),
                        include_replicas: true,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(selected.items.len(), 2);
            let connection = rusqlite::Connection::open(&path).unwrap();
            let after: String =
                connection.query_row("SELECT body_json FROM resource_objects WHERE kind='Project'", [], |row| row.get(0)).unwrap();
            assert_eq!(before, after, "collection migration must preserve stored bodies");
        }
    }

    #[tokio::test]
    async fn declared_query_plans_use_indexes() {
        let backend = SqliteBackend::open_in_memory().unwrap();
        // This inspects the complete plan corpus, not one production request.
        // Keep every plan assertion independent of the request-read deadline.
        backend
            .call("explain declared queries", |connection| {
                let mut migration_names = std::collections::BTreeSet::new();
                for kind in crate::declared_resource_kinds() {
                    for replicas in [false, true] {
                        let table = if replicas { "replica_objects" } else { "resource_objects" };
                        let mut cases = Vec::new();
                        for path in kind.queryable_fields {
                            let name = SqliteBackend::field_index(table, kind, path);
                            assert!(migration_names.insert(name.clone()), "colliding index migration {name}: {} {path}", kind.paths.kind);
                        }
                        for path in kind.queryable_fields {
                            cases.push((
                                crate::StoreQuery {
                                    field_values: BTreeMap::from([(path.to_string(), ["first".into(), "second".into()].into())]),
                                    ..Default::default()
                                },
                                SqliteBackend::field_index(table, kind, path),
                            ));
                            cases.push((
                                crate::StoreQuery { fields: BTreeMap::from([(path.to_string(), "value".into())]), ..Default::default() },
                                SqliteBackend::field_index(table, kind, path),
                            ));
                        }
                        for path in kind.queryable_fields {
                            cases.push((
                                crate::StoreQuery { present: vec![(*path).into()], ..Default::default() },
                                SqliteBackend::field_index(table, kind, path),
                            ));
                            let alternative =
                                crate::StoreQuery { fields: BTreeMap::from([((*path).into(), "value".into())]), ..Default::default() };
                            cases.push((
                                crate::StoreQuery { any_of: Some(vec![alternative.clone(), alternative]), ..Default::default() },
                                SqliteBackend::field_index(table, kind, path),
                            ));
                        }
                        cases.push((
                            crate::StoreQuery { label_prefixes: BTreeMap::from([("team".into(), "pre".into())]), ..Default::default() },
                            format!("{table}_labels_lookup"),
                        ));
                        for path in kind.queryable_fields {
                            cases.push((
                                crate::StoreQuery {
                                    field_prefixes: BTreeMap::from([((*path).into(), "prefix".into())]),
                                    ..Default::default()
                                },
                                SqliteBackend::field_index(table, kind, path),
                            ));
                            cases.push((
                                crate::StoreQuery { missing_fields: [(*path).into()].into(), ..Default::default() },
                                format!("sqlite_autoindex_{}_values_1", SqliteBackend::field_index(table, kind, path)),
                            ));
                            let source = crate::StoreQuery::names(["source".into()], true);
                            let related = crate::StoreQuery {
                                any_of: Some(vec![crate::StoreQuery::related(path, path, source)]),
                                ..Default::default()
                            };
                            // Source identities use their name index; array target membership
                            // also searches the additive relation index.
                            cases.push((related, SqliteBackend::field_index("resource_objects", kind, "metadata.name")));
                        }
                        if kind.paths.kind == "Convoy" {
                            let source = crate::ResourceKind::of::<crate::TerminalSession>();
                            cases.push((
                                crate::StoreQuery::related_kind(
                                    "metadata.name",
                                    "TerminalSession",
                                    "metadata.labels.\"flotilla.work/convoy\"",
                                    crate::StoreQuery {
                                        fields: BTreeMap::from([("status.crew.id".into(), "crew".into())]),
                                        ..Default::default()
                                    },
                                ),
                                SqliteBackend::field_index("resource_objects", source, "status.crew.id"),
                            ));
                        }
                        cases.push((
                            crate::StoreQuery::labels(&BTreeMap::from([("team".into(), "target".into())]), true, true),
                            format!("{table}_labels_lookup"),
                        ));
                        cases.push((
                            crate::StoreQuery {
                                owner: Some(crate::OwnerQuery {
                                    api_version: "flotilla.work/v1".into(),
                                    kind: "Project".into(),
                                    name: "project".into(),
                                }),
                                ..Default::default()
                            },
                            format!("{table}_owners_lookup"),
                        ));
                        if kind.paths.kind == "Message" {
                            for message in [
                                crate::MessageQuery::active(),
                                crate::MessageQuery::Active { receiver: Some("r".into()) },
                                crate::MessageQuery::ReplyTo { name: "n".into() },
                                crate::MessageQuery::Batch { id: "b".into() },
                                crate::MessageQuery::AuditBefore { before: Utc::now() },
                            ] {
                                let index = message.sql_index(replicas).to_string();
                                cases.push((crate::StoreQuery { message: Some(message), ..Default::default() }, index));
                            }
                        }
                        if kind.paths.kind == "Message" {
                            let combined = crate::StoreQuery {
                                fields: BTreeMap::from([
                                    ("spec.receiver".into(), "receiver".into()),
                                    ("status.phase".into(), "accepted".into()),
                                ]),
                                labels: BTreeMap::from([("team".into(), "target".into())]),
                                owner: Some(crate::OwnerQuery {
                                    api_version: "flotilla.work/v1".into(),
                                    kind: "Project".into(),
                                    name: "project".into(),
                                }),
                                ..Default::default()
                            };
                            cases.push((combined.clone(), SqliteBackend::field_index(table, kind, "spec.receiver")));
                            cases.push((combined.clone(), format!("{table}_labels_lookup")));
                            cases.push((combined, format!("{table}_owners_lookup")));
                        }
                        if kind.replication == crate::ReplicationClass::Definitions {
                            for path in kind.queryable_fields {
                                cases.push((
                                    crate::StoreQuery {
                                        fields: BTreeMap::from([(path.to_string(), "value".into())]),
                                        all_sources_for_matches: true,
                                        include_replicas: true,
                                        suppress_self_origin: false,
                                        ..Default::default()
                                    },
                                    SqliteBackend::field_index(table, kind, path),
                                ));
                            }
                        }
                        // Exhaust every declared-field subset and relation combination.
                        // Cover individual fields, pairs and the full conjunction.
                        // Forced partial indexes must remain usable even when additional
                        // fields/relations/Message variants are residual predicates.
                        // Each field, each pair, and the complete conjunction cover
                        // index choice without an exponential plan corpus.
                        let mut masks = std::collections::BTreeSet::from([0, (1usize << kind.queryable_fields.len()) - 1]);
                        for a in 0..kind.queryable_fields.len() {
                            masks.insert(1 << a);
                            for b in a + 1..kind.queryable_fields.len() {
                                masks.insert((1 << a) | (1 << b));
                            }
                        }
                        for mask in masks {
                            let fields: BTreeMap<String, String> = kind
                                .queryable_fields
                                .iter()
                                .enumerate()
                                .filter(|(bit, _)| mask & (1 << bit) != 0)
                                .map(|(_, path)| ((*path).into(), "value".into()))
                                .collect();
                            for relations in 0..4 {
                                let query = crate::StoreQuery {
                                    fields: fields.clone(),
                                    labels: if relations & 1 != 0 {
                                        BTreeMap::from([("team".into(), "target".into())])
                                    } else {
                                        BTreeMap::new()
                                    },
                                    owner: (relations & 2 != 0).then(|| crate::OwnerQuery {
                                        api_version: "flotilla.work/v1".into(),
                                        kind: "Project".into(),
                                        name: "project".into(),
                                    }),
                                    ..Default::default()
                                };
                                if let Some(path) =
                                    fields.keys().find(|path| crate::array_field_path(path).is_none()).or_else(|| fields.keys().next())
                                {
                                    cases.push((query.clone(), SqliteBackend::field_index(table, kind, path)));
                                }
                                if kind.paths.kind == "Message" {
                                    for message in [
                                        crate::MessageQuery::active(),
                                        crate::MessageQuery::Active { receiver: Some("r".into()) },
                                        crate::MessageQuery::ReplyTo { name: "n".into() },
                                        crate::MessageQuery::Batch { id: "b".into() },
                                        crate::MessageQuery::AuditBefore { before: Utc::now() },
                                    ] {
                                        let index = message.sql_index(replicas).to_string();
                                        cases.push((crate::StoreQuery { message: Some(message), ..query.clone() }, index));
                                    }
                                }
                            }
                        }
                        for (query, index) in cases {
                            query.validate(kind)?;
                            let (sql, args) =
                                SqliteBackend::declared_sql(kind, "flotilla", &query, replicas, "local", QueryColumns::ReadRows);
                            let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
                            let plans = statement
                                .query_map(rusqlite::params_from_iter(args), |row| row.get::<_, String>(3))
                                .unwrap()
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap();
                            assert!(plans.iter().any(|plan| plan.contains(&index) && plan.contains("SEARCH")), "{index}: {plans:?}");
                            if !query.field_prefixes.is_empty() {
                                // The planner stability guarantee rules out bound GLOB ranges.
                                assert!(
                                    plans.iter().any(|plan| plan.contains(&index) && plan.contains("value>?")),
                                    "prefix matches must search a value range: {plans:?}"
                                );
                            }
                            if index.starts_with("sqlite_autoindex_") {
                                assert!(
                                    plans.iter().any(|plan| plan.contains(&index) && plan.contains("name=?") && plan.contains("origin=?")),
                                    "absence must probe the complete row identity: {plans:?}"
                                );
                            }
                        }
                    }
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    // WORKLOAD.md: relay catch-up reads only the requested kind/namespace's
    // deletion metadata. One case covers this SQL shape for all bound keys.
    #[tokio::test]
    async fn relay_deletion_snapshot_searches_covering_partition_index() {
        let backend = SqliteBackend::open_in_memory().expect("open relay store");
        let plans = backend
            .read_call("explain relay deletion snapshot", |connection| {
                let mut statement = connection
                    .prepare(&format!("EXPLAIN QUERY PLAN {REPLICA_DELETIONS_SELECT_SQL}"))
                    .map_err(|error| SqliteBackend::map_sqlite(error, "explain relay deletion snapshot"))?;
                let rows = statement
                    .query_map(params!["flotilla.work", "v1", "Convoy", "flotilla"], |row| row.get::<_, String>(3))
                    .map_err(|error| SqliteBackend::map_sqlite(error, "query relay deletion plan"))?;
                rows.collect::<Result<Vec<_>, _>>().map_err(|error| SqliteBackend::map_sqlite(error, "read relay deletion plan"))
            })
            .await
            .expect("relay deletion query plan");
        assert!(
            plans.iter().any(|plan| plan.contains("SEARCH replica_tombstones USING COVERING INDEX replica_tombstones_relay_snapshot")),
            "{plans:?}"
        );
        assert!(!plans.iter().any(|plan| plan.contains("USE TEMP B-TREE")), "partition index also supplies relay ordering: {plans:?}");
    }

    #[tokio::test]
    async fn field_ownership_diagnostics_expire_old_violations() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("resources.sqlite");
        let backend = SqliteBackend::open(&path).expect("sqlite backend");
        let old = Utc::now() - chrono::Duration::days(2);
        for (name, observed_at) in [("old", old), ("recent", Utc::now())] {
            backend
                .record_field_ownership_violation(
                    FieldOwnershipViolation::builder()
                        .kind("PlacementPolicy".to_string())
                        .namespace("flotilla".to_string())
                        .name(name.to_string())
                        .writer(crate::WriterIdentity::operator())
                        .field("spec.docker_per_vessel.host_ref".to_string())
                        .attempted_value(serde_json::json!("wrong"))
                        .rule("owned by ReconcileLoop".to_string())
                        .observed_at(observed_at)
                        .build(),
                )
                .await
                .expect("record violation");
        }

        let diagnostics = backend.diagnostics().await.expect("diagnostics");
        assert_eq!(diagnostics.field_ownership_violations.len(), 1);
        assert_eq!(diagnostics.field_ownership_violations[0].name, "recent");
        let connection = RusqliteConnection::open(&path).expect("sqlite connection");
        let count: i64 = connection.query_row("SELECT COUNT(*) FROM field_ownership_violations", [], |row| row.get(0)).expect("count rows");
        assert_eq!(count, 1, "expired rows should be removed from SQLite");
    }

    #[tokio::test]
    async fn quarantine_only_delete_refuses_a_concurrently_created_live_object() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("resources.sqlite");
        let backend = SqliteBackend::open(&path).expect("sqlite backend should open");
        backend
            .create_typed::<Environment>(
                "flotilla",
                &InputMeta::builder().name("contended".to_string()).build(),
                &EnvironmentSpec { cloud_sandbox: None, provider_address: None, host_direct: None, docker: None },
            )
            .await
            .expect("create live resource");

        let connection = RusqliteConnection::open(&path).expect("open raw sqlite connection");
        connection
            .execute(
                r#"
                INSERT INTO resource_decode_quarantine
                    (group_name, version, kind, namespace, name, body_json, error, quarantined_at)
                VALUES (?1, ?2, ?3, ?4, ?5, '{}', 'stale diagnosis', ?6)
                "#,
                params![
                    Environment::API_PATHS.group,
                    Environment::API_PATHS.version,
                    Environment::API_PATHS.kind,
                    "flotilla",
                    "contended",
                    Utc::now().to_rfc3339()
                ],
            )
            .expect("seed stale quarantine diagnosis beside live object");
        drop(connection);

        let error = backend
            .delete_decode_quarantine_typed::<Environment>("flotilla", "contended")
            .await
            .expect_err("quarantine-only deletion must not delete a live resource");
        assert!(matches!(error, ResourceError::Conflict { .. }), "unexpected quarantine-delete error: {error}");
        assert!(backend.get_typed::<Environment>("flotilla", "contended").await.is_ok(), "the concurrent live resource must remain intact");
        assert_eq!(backend.diagnostics().await.expect("read diagnostics").decode_quarantines.len(), 1);
    }
}

#[cfg(test)]
mod message_query_plan_tests {
    use super::*;

    // Bounded inbox work requires SQLite to select indexed names before decoding,
    // rather than visiting every terminal audit body in the namespace.
    #[tokio::test]
    async fn message_queries_use_history_independent_indexes() {
        let backend = SqliteBackend::open_in_memory().unwrap();
        backend.read_call("inspect Message query plans", |connection| {
            for (query, expected) in [
                (crate::MessageQuery::active(), "resource_objects_message_inbox"),
                (crate::MessageQuery::Active { receiver: Some("project/role".into()) }, "resource_objects_message_active"),
                (crate::MessageQuery::ReplyTo { name: "request".into() }, "resource_objects_message_reply"),
                (crate::MessageQuery::Batch { id: "batch".into() }, "resource_objects_message_batch"),
                (crate::MessageQuery::AuditBefore { before: Utc::now() }, "message_audit_age"),
            ] {
                let (predicate, argument) = query.sql();
                let index = query.sql_index(false);
                let sql = format!("EXPLAIN QUERY PLAN SELECT body_json FROM resource_objects INDEXED BY {index} WHERE group_name=?1 AND version=?2 AND kind=?3 AND namespace=?4 AND kind='Message' AND ({predicate}) AND (?5 IS NULL OR ?5 IS NOT NULL) ORDER BY name");
                let mut statement = connection.prepare(&sql).unwrap();
                let rows = statement.query_map(params!["flotilla.work", "v1", "Message", "flotilla", argument], |row| row.get::<_, String>(3)).unwrap();
                let plan = rows.collect::<Result<Vec<_>, _>>().unwrap().join("\n");
                assert!(plan.contains(expected), "{query:?} should use {expected}: {plan}");
            }
            Ok(())
        }).await.unwrap();
    }
}

impl WatchObject for StoredEvent {
    fn object(&self) -> &Value {
        &self.object
    }
}
impl From<StoredEventKind> for WatchAction {
    fn from(kind: StoredEventKind) -> Self {
        match kind {
            StoredEventKind::Added => Self::Added,
            StoredEventKind::Modified => Self::Modified,
            StoredEventKind::Deleted => Self::Deleted,
        }
    }
}

#[cfg(test)]
mod queue_contract {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Discovery owns reserved inventory capacity and must answer while every
    // general resource reader is deliberately stalled.
    #[tokio::test]
    async fn discovery_bypasses_saturated_resource_readers() {
        let directory = tempfile::tempdir().unwrap();
        let backend = SqliteBackend::open(directory.path().join("queue.sqlite")).unwrap();
        let mut releases = Vec::new();
        let mut tasks = Vec::new();
        for connection in &backend.read_connections[..READER_COUNT - 1] {
            let connection = connection.clone();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            tasks.push(tokio::spawn(async move {
                connection
                    .call(move |_| {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                        Ok::<_, ResourceError>(())
                    })
                    .await
                    .unwrap();
            }));
            entered_rx.await.unwrap();
            releases.push(release_tx);
        }
        let result = tokio::time::timeout(READ_DEADLINE, backend.namespace_inventory()).await;
        for release in releases {
            release.send(()).unwrap();
        }
        for task in tasks {
            task.await.unwrap();
        }
        assert!(!result.unwrap().unwrap().is_empty());
    }

    // Behaviour (#3206): every write and read records its queue wait and
    // execution, and the writer's queue peak counts writes behind a running one.
    #[tokio::test]
    async fn load_metrics_time_the_writer_queue_and_reads() {
        let backend = SqliteBackend::open_in_memory().unwrap();
        let before = backend.load_metrics().snapshot();
        let blocked = backend.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::spawn(async move {
            blocked
                .call("blocking write", move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                })
                .await
        });
        entered_rx.await.unwrap();
        let queued = (0..3).map(|_| {
            let backend = backend.clone();
            tokio::spawn(async move { backend.call("queued write", |_| Ok(())).await })
        });
        let queued = queued.collect::<Vec<_>>();
        // The running write released its claim when it started; three are queued.
        while backend.writer_depth.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
        release_tx.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        for write in queued {
            write.await.unwrap().unwrap();
        }
        backend.read_call("read", |_| Ok(())).await.unwrap();
        let phase = backend.load_metrics().snapshot().since(&before);
        assert_eq!(phase.write_wait.count(), 4);
        assert_eq!(phase.write_execution.count(), 4);
        assert!(phase.write_queue_peak >= 2, "writes queued behind the blocked one: {}", phase.write_queue_peak);
        assert!(phase.read_execution.count() >= 1);
        assert_eq!(phase.reads_shed, 0);
    }

    // Deadlines expire queued reads before they execute, even when the caller
    // waits for the error rather than cancelling its future.
    #[tokio::test]
    async fn expired_reads_do_not_execute() {
        let backend = SqliteBackend::open_in_memory().unwrap();
        let blocked = backend.connection.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::spawn(async move {
            blocked
                .call(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok::<_, ResourceError>(())
                })
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        let executed = Arc::new(AtomicUsize::new(0));
        let count = executed.clone();
        let result = backend
            .read_call("expired read", move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        backend.read_call("recovered read", |_| Ok(())).await.unwrap();
        assert!(result.unwrap_err().to_string().contains("read deadline exceeded"));
        // The load metrics count the shed read once and time only reads that ran.
        let metrics = backend.load_metrics().snapshot();
        assert_eq!(metrics.reads_shed, 1);
        assert!(metrics.read_execution.count() >= 1);
        assert!(metrics.read_wait.summary().max_micros >= READ_DEADLINE.as_micros() as u64);
        assert_eq!(executed.load(Ordering::SeqCst), 0);
    }

    // A read abandoned while queued must never execute, and subsequent reads
    // must recover as soon as the deliberately blocked worker is released.
    #[tokio::test]
    async fn abandoned_reads_are_shed() {
        let backend = SqliteBackend::open_in_memory().unwrap();
        let blocked = backend.connection.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocker = tokio::spawn(async move {
            blocked
                .call(move |_| {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok::<_, ResourceError>(())
                })
                .await
                .unwrap();
        });
        entered_rx.await.unwrap();
        let executed = Arc::new(AtomicUsize::new(0));
        let count = executed.clone();
        let read_backend = backend.clone();
        let abandoned = tokio::spawn(async move {
            read_backend
                .read_call("abandoned read", move |_| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
        });
        tokio::time::timeout(READ_DEADLINE, async {
            while backend.read_depth.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        abandoned.abort();
        let _ = abandoned.await;
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        tokio::time::timeout(READ_DEADLINE, backend.read_call("recovered read", |_| Ok(()))).await.unwrap().unwrap();
        assert_eq!(executed.load(Ordering::SeqCst), 0);
    }
}
