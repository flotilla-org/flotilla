use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use futures::{stream, StreamExt};
use rusqlite::{params, Connection as RusqliteConnection, OptionalExtension};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_rusqlite::Connection;

use crate::{
    error::ResourceError,
    replica::{ReadResourceObject, ReadWatchEvent, ReplicaCursor, ResourceProvenance, StoredReplicaEvent, StoredReplicaEventKind},
    resource::{InputMeta, K8sResourceObject, ObjectMeta, Resource, ResourceObject},
    retention::{EventRetention, ResourceStoreDiagnostics},
    watch::{ResourceList, WatchEvent, WatchStart, WatchStream},
};

type StoreKey = (String, String, String, String);
type WatchSender = mpsc::UnboundedSender<StoredEvent>;
type WatchersByStore = HashMap<StoreKey, Vec<WatchSender>>;
type ReplicaWatchSender = mpsc::UnboundedSender<StoredReplicaEvent>;
type ReplicaWatchersByStore = HashMap<StoreKey, Vec<ReplicaWatchSender>>;

#[derive(Debug, Clone, bon::Builder)]
#[builder(builder_type(vis = "pub(in crate::sqlite)"))]
pub struct SqliteBackend {
    connection: Connection,
    // Mutations notify and watches register from the connection thread so a
    // committed event cannot land between replay and live delivery.
    watchers: Arc<Mutex<WatchersByStore>>,
    replica_watchers: Arc<Mutex<ReplicaWatchersByStore>>,
    event_retention: EventRetention,
}

#[derive(Debug, Clone)]
struct StoredEvent {
    kind: StoredEventKind,
    object: Value,
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

impl SqliteBackend {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ResourceError> {
        Self::open_with_event_retention(path, EventRetention::default())
    }

    pub fn open_with_event_retention(path: impl AsRef<Path>, event_retention: EventRetention) -> Result<Self, ResourceError> {
        let connection =
            RusqliteConnection::open(path).map_err(|err| ResourceError::other(format!("open sqlite resource store: {err}")))?;
        Self::from_connection(connection, event_retention)
    }

    pub async fn open_async(path: impl AsRef<Path>) -> Result<Self, ResourceError> {
        let connection = Connection::open(path).await.map_err(|err| ResourceError::other(format!("open sqlite resource store: {err}")))?;
        Self::from_async_connection(connection, EventRetention::default()).await
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
        Self::initialize_connection(&mut connection, event_retention)?;
        Ok(Self {
            connection: connection.into(),
            watchers: Arc::new(Mutex::new(HashMap::new())),
            replica_watchers: Arc::new(Mutex::new(HashMap::new())),
            event_retention,
        })
    }

    async fn from_async_connection(connection: Connection, event_retention: EventRetention) -> Result<Self, ResourceError> {
        connection
            .call(move |connection| Self::initialize_connection(connection, event_retention))
            .await
            .map_err(Self::map_connection_error)?;
        Ok(Self {
            connection,
            watchers: Arc::new(Mutex::new(HashMap::new())),
            replica_watchers: Arc::new(Mutex::new(HashMap::new())),
            event_retention,
        })
    }

    fn initialize_connection(connection: &mut RusqliteConnection, event_retention: EventRetention) -> Result<(), ResourceError> {
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

                CREATE TABLE IF NOT EXISTS resource_event_compaction (
                    group_name TEXT NOT NULL,
                    version TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    namespace TEXT NOT NULL,
                    compacted_through INTEGER NOT NULL,
                    PRIMARY KEY (group_name, version, kind, namespace)
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
                "#,
            )
            .map_err(|err| ResourceError::other(format!("initialize sqlite resource store: {err}")))?;
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
        let startup_diagnostics = Self::diagnostics_from_connection(connection, event_retention)?;
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
        Self::compact_existing_events(connection, event_retention)?;
        Ok(())
    }

    fn store_key<T: Resource>(namespace: &str) -> StoreKey {
        (T::API_PATHS.group.to_string(), T::API_PATHS.version.to_string(), T::API_PATHS.kind.to_string(), namespace.to_string())
    }

    fn lock_watchers(watchers: &Mutex<WatchersByStore>) -> Result<MutexGuard<'_, WatchersByStore>, ResourceError> {
        watchers.lock().map_err(|_| ResourceError::other("sqlite resource watch lock poisoned"))
    }

    async fn call<R, F>(&self, operation: F) -> Result<R, ResourceError>
    where
        R: Send + 'static,
        F: FnOnce(&mut RusqliteConnection) -> Result<R, ResourceError> + Send + 'static,
    {
        self.connection.call(operation).await.map_err(Self::map_connection_error)
    }

    fn map_connection_error(error: tokio_rusqlite::Error<ResourceError>) -> ResourceError {
        match error {
            tokio_rusqlite::Error::Error(error) => error,
            other => ResourceError::other(format!("sqlite connection thread failed: {other}")),
        }
    }

    pub(crate) async fn diagnostics(&self) -> Result<ResourceStoreDiagnostics, ResourceError> {
        let event_retention = self.event_retention;
        self.call(move |connection| Self::diagnostics_from_connection(connection, event_retention)).await
    }

    fn diagnostics_from_connection(
        connection: &RusqliteConnection,
        event_retention: EventRetention,
    ) -> Result<ResourceStoreDiagnostics, ResourceError> {
        let object_count = connection
            .query_row("SELECT COUNT(*) FROM resource_objects", [], |row| row.get::<_, u64>(0))
            .map_err(|err| Self::map_sqlite(err, "count sqlite resource objects"))?;
        let event_count = connection
            .query_row("SELECT COUNT(*) FROM resource_events", [], |row| row.get::<_, u64>(0))
            .map_err(|err| Self::map_sqlite(err, "count sqlite resource events"))?;
        let resource_stream_count = connection
            .query_row("SELECT COUNT(*) FROM resource_sequences", [], |row| row.get::<_, u64>(0))
            .map_err(|err| Self::map_sqlite(err, "count sqlite resource streams"))?;
        Ok(ResourceStoreDiagnostics::new(object_count, event_count, resource_stream_count, event_retention))
    }

    fn clone_through_serde<T>(value: &T) -> Result<T, ResourceError>
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::from_value(serde_json::to_value(value).map_err(|err| ResourceError::decode(format!("serialize value: {err}")))?)
            .map_err(|err| ResourceError::decode(format!("deserialize value: {err}")))
    }

    fn decode_object<T: Resource>(value: Value) -> Result<ResourceObject<T>, ResourceError> {
        let object: K8sResourceObject<T> =
            serde_json::from_value(value).map_err(|err| ResourceError::decode(format!("decode stored object: {err}")))?;
        ResourceObject::from_k8s_object(object)
    }

    fn encode_object<T: Resource>(object: &ResourceObject<T>) -> Result<Value, ResourceError> {
        serde_json::to_value(object.to_k8s_object()).map_err(|err| ResourceError::decode(format!("encode object: {err}")))
    }

    fn decode_event<T: Resource>(event: StoredEvent) -> Result<WatchEvent<T>, ResourceError> {
        let object = Self::decode_object::<T>(event.object)?;
        Ok(match event.kind {
            StoredEventKind::Added => WatchEvent::Added(object),
            StoredEventKind::Modified => WatchEvent::Modified(object),
            StoredEventKind::Deleted => WatchEvent::Deleted(object),
        })
    }

    fn map_sqlite(err: rusqlite::Error, action: &str) -> ResourceError {
        ResourceError::other(format!("{action}: {err}"))
    }

    fn current_version(conn: &RusqliteConnection, key: &StoreKey) -> Result<u64, ResourceError> {
        let version = conn
            .query_row(
                "SELECT next_version FROM resource_sequences WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4",
                params![key.0, key.1, key.2, key.3],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource sequence"))?;
        Ok(version.unwrap_or(1).saturating_sub(1))
    }

    fn allocate_version(tx: &rusqlite::Transaction<'_>, key: &StoreKey) -> Result<u64, ResourceError> {
        let next = tx
            .query_row(
                "SELECT next_version FROM resource_sequences WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4",
                params![key.0, key.1, key.2, key.3],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource sequence"))?
            .unwrap_or(1);
        tx.execute(
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
    ) -> Result<(), ResourceError> {
        tx.execute(
            r#"
            INSERT INTO resource_events (group_name, version, kind, namespace, event_version, event_type, body_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![key.0, key.1, key.2, key.3, event_version, kind.as_str(), body_json],
        )
        .map_err(|err| Self::map_sqlite(err, "insert sqlite resource event"))?;
        Self::compact_events(tx, key, event_version, event_retention)?;
        Ok(())
    }

    fn compact_events(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        current_version: u64,
        event_retention: EventRetention,
    ) -> Result<(), ResourceError> {
        let compacted_through = current_version.saturating_sub(event_retention.max_events_per_resource_stream() as u64);
        if compacted_through == 0 {
            return Ok(());
        }
        tx.execute(
            r#"
            DELETE FROM resource_events
            WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND event_version <= ?5
            "#,
            params![key.0, key.1, key.2, key.3, compacted_through],
        )
        .map_err(|err| Self::map_sqlite(err, "compact sqlite resource events"))?;
        tx.execute(
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
                entries.retain(|watcher| watcher.send(event.clone()).is_ok());
            }
        }
    }

    fn notify_replica_watchers(watchers: &Mutex<ReplicaWatchersByStore>, key: &StoreKey, event: StoredReplicaEvent) {
        if let Ok(mut watchers) = watchers.lock() {
            if let Some(entries) = watchers.get_mut(key) {
                entries.retain(|watcher| watcher.send(event.clone()).is_ok());
            }
        }
    }

    pub(crate) async fn list_replicas_typed<T: Resource>(&self, namespace: &str) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        self.call(move |connection| {
            let mut statement = connection
                .prepare(
                    r#"
                    SELECT o.origin_root, o.last_synced_at, o.body_json
                    FROM replica_objects o
                    WHERE o.group_name = ?1 AND o.version = ?2 AND o.kind = ?3 AND o.namespace = ?4
                    ORDER BY o.origin_root, o.name
                    "#,
                )
                .map_err(|err| Self::map_sqlite(err, "prepare sqlite replica list"))?;
            let rows = statement
                .query_map(params![key.0, key.1, key.2, key.3], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                })
                .map_err(|err| Self::map_sqlite(err, "query sqlite replica list"))?;
            let mut items = Vec::new();
            for row in rows {
                let (origin_root, last_synced_at, body) = row.map_err(|err| Self::map_sqlite(err, "read sqlite replica list row"))?;
                let value =
                    serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode replica object JSON: {err}")))?;
                let last_synced_at = DateTime::parse_from_rfc3339(&last_synced_at)
                    .map_err(|err| ResourceError::decode(format!("decode replica sync timestamp: {err}")))?
                    .with_timezone(&Utc);
                items.push(ReadResourceObject {
                    object: Self::decode_object(value)?,
                    provenance: ResourceProvenance::Replica { origin_root: NodeId::new(origin_root), last_synced_at },
                });
            }
            Ok(items)
        })
        .await
    }

    pub(crate) async fn watch_replicas_typed<T: Resource>(
        &self,
        namespace: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let (tx, rx) = mpsc::unbounded_channel();
        self.replica_watchers
            .lock()
            .map_err(|_| ResourceError::other("sqlite replica watch lock poisoned"))?
            .entry(key)
            .or_default()
            .push(tx);
        Ok(stream::unfold(rx, |mut rx| async {
            let event = rx.recv().await?;
            let decoded = Self::decode_object::<T>(event.object).map(|object| {
                let object = ReadResourceObject {
                    object,
                    provenance: ResourceProvenance::Replica { origin_root: event.origin_root, last_synced_at: event.synced_at },
                };
                match event.kind {
                    StoredReplicaEventKind::Added => ReadWatchEvent::Added(object),
                    StoredReplicaEventKind::Modified => ReadWatchEvent::Modified(object),
                    StoredReplicaEventKind::Deleted => ReadWatchEvent::Deleted(object),
                }
            });
            Some((decoded, rx))
        })
        .boxed())
    }

    pub(crate) async fn replace_replicas_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        listed: &ResourceList<T>,
        synced_at: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let key = Self::store_key::<T>(namespace);
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
                    serde_json::to_string(&Self::encode_object(object)?)
                        .map_err(|err| ResourceError::decode(format!("encode replica object JSON: {err}")))?,
                ))
            })
            .collect::<Result<Vec<_>, ResourceError>>()?;
        let events = self
            .call(move |connection| {
                let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite replica replacement"))?;
                let old = {
                    let mut statement = tx
                        .prepare(
                            r#"
                            SELECT name, body_json FROM replica_objects
                            WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5
                            "#,
                        )
                        .map_err(|err| Self::map_sqlite(err, "prepare old sqlite replicas"))?;
                    let rows = statement
                        .query_map(params![origin, key.0, key.1, key.2, key.3], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(|err| Self::map_sqlite(err, "query old sqlite replicas"))?;
                    rows.collect::<Result<HashMap<_, _>, _>>().map_err(|err| Self::map_sqlite(err, "read old sqlite replica row"))?
                };
                tx.execute(
                    r#"
                    DELETE FROM replica_objects
                    WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5
                    "#,
                    params![origin, key.0, key.1, key.2, key.3],
                )
                .map_err(|err| Self::map_sqlite(err, "clear sqlite replica partition"))?;
                for (name, body) in &encoded {
                    tx.execute(
                        r#"
                        INSERT INTO replica_objects
                            (origin_root, group_name, version, kind, namespace, name, body_json, last_synced_at)
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                        "#,
                        params![origin, key.0, key.1, key.2, key.3, name, body, synced],
                    )
                    .map_err(|err| Self::map_sqlite(err, "insert sqlite replica object"))?;
                }
                tx.execute(
                    r#"
                    INSERT INTO replica_cursors
                        (origin_root, group_name, version, kind, namespace, resource_version, generation, last_synced_at)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                    ON CONFLICT(origin_root, group_name, version, kind, namespace)
                    DO UPDATE SET resource_version = excluded.resource_version,
                                  generation = excluded.generation,
                                  last_synced_at = excluded.last_synced_at
                    "#,
                    params![origin, key.0, key.1, key.2, key.3, resource_version, generation, synced],
                )
                .map_err(|err| Self::map_sqlite(err, "write sqlite replica cursor"))?;
                tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite replica replacement"))?;

                let new_names = encoded.iter().map(|(name, _)| name.clone()).collect::<std::collections::HashSet<_>>();
                let mut events = encoded
                    .into_iter()
                    .map(|(name, body)| {
                        let kind = if old.contains_key(&name) { StoredReplicaEventKind::Modified } else { StoredReplicaEventKind::Added };
                        Ok((
                            kind,
                            serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode replica event: {err}")))?,
                        ))
                    })
                    .collect::<Result<Vec<_>, ResourceError>>()?;
                for (name, body) in old {
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
            Self::notify_replica_watchers(&self.replica_watchers, &operation_key, StoredReplicaEvent {
                origin_root: origin_root.clone(),
                synced_at,
                kind,
                object,
            });
        }
        Ok(())
    }

    pub(crate) async fn apply_replica_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
        kind: StoredReplicaEventKind,
        object: &ResourceObject<T>,
        synced_at: DateTime<Utc>,
    ) -> Result<(), ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let operation_key = key.clone();
        let origin = origin_root.to_string();
        let name = object.metadata.name.clone();
        let resource_version = object.metadata.resource_version.clone();
        let value = Self::encode_object(object)?;
        let body =
            serde_json::to_string(&value).map_err(|err| ResourceError::decode(format!("encode sqlite replica event JSON: {err}")))?;
        let synced = synced_at.to_rfc3339();
        self.call(move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite replica event"))?;
            match kind {
                StoredReplicaEventKind::Added | StoredReplicaEventKind::Modified => {
                    tx.execute(
                        r#"
                        INSERT INTO replica_objects
                            (origin_root, group_name, version, kind, namespace, name, body_json, last_synced_at)
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                        ON CONFLICT(origin_root, group_name, version, kind, namespace, name)
                        DO UPDATE SET body_json = excluded.body_json,
                                      last_synced_at = excluded.last_synced_at
                        "#,
                        params![origin, key.0, key.1, key.2, key.3, name, body, synced],
                    )
                    .map_err(|err| Self::map_sqlite(err, "upsert sqlite replica event object"))?;
                }
                StoredReplicaEventKind::Deleted => {
                    tx.execute(
                        r#"
                        DELETE FROM replica_objects
                        WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5 AND name = ?6
                        "#,
                        params![origin, key.0, key.1, key.2, key.3, name],
                    )
                    .map_err(|err| Self::map_sqlite(err, "delete sqlite replica event object"))?;
                }
            }
            tx.execute(
                r#"
                UPDATE replica_cursors
                SET resource_version = ?6, last_synced_at = ?7
                WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5
                "#,
                params![origin, key.0, key.1, key.2, key.3, resource_version, synced],
            )
            .map_err(|err| Self::map_sqlite(err, "advance sqlite replica cursor"))?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite replica event"))
        })
        .await?;
        Self::notify_replica_watchers(&self.replica_watchers, &operation_key, StoredReplicaEvent {
            origin_root: origin_root.clone(),
            synced_at,
            kind,
            object: value,
        });
        Ok(())
    }

    pub(crate) async fn replica_cursor_typed<T: Resource>(
        &self,
        origin_root: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let origin = origin_root.to_string();
        self.call(move |connection| {
            connection
                .query_row(
                    r#"
                    SELECT resource_version, generation FROM replica_cursors
                    WHERE origin_root = ?1 AND group_name = ?2 AND version = ?3 AND kind = ?4 AND namespace = ?5
                    "#,
                    params![origin, key.0, key.1, key.2, key.3],
                    |row| Ok(ReplicaCursor { resource_version: row.get(0)?, generation: row.get(1)? }),
                )
                .optional()
                .map_err(|err| Self::map_sqlite(err, "read sqlite replica cursor"))
        })
        .await
    }

    pub(crate) async fn get_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let name = name.to_string();
        self.call(move |connection| {
            let body: String = connection
                .query_row(
                    r#"
                    SELECT body_json FROM resource_objects
                    WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                    "#,
                    params![key.0, key.1, key.2, key.3, name],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|err| Self::map_sqlite(err, "read sqlite resource object"))?
                .ok_or_else(|| ResourceError::not_found(&name))?;
            let value = serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode stored object JSON: {err}")))?;
            Self::decode_object::<T>(value)
        })
        .await
    }

    pub(crate) async fn list_typed<T: Resource>(&self, namespace: &str) -> Result<ResourceList<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        self.call(move |connection| {
            let mut statement = connection
                .prepare(
                    r#"
                    SELECT body_json FROM resource_objects
                    WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4
                    ORDER BY name
                    "#,
                )
                .map_err(|err| Self::map_sqlite(err, "prepare sqlite resource list"))?;
            let rows = statement
                .query_map(params![key.0, key.1, key.2, key.3], |row| row.get::<_, String>(0))
                .map_err(|err| Self::map_sqlite(err, "query sqlite resource list"))?;
            let mut items = Vec::new();
            for row in rows {
                let body = row.map_err(|err| Self::map_sqlite(err, "read sqlite resource list row"))?;
                let value =
                    serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode stored object JSON: {err}")))?;
                items.push(Self::decode_object::<T>(value)?);
            }
            Ok(ResourceList { items, resource_version: Self::current_version(connection, &key)?.to_string(), generation: None })
        })
        .await
    }

    pub(crate) async fn list_typed_matching_labels<T: Resource>(
        &self,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<T>, ResourceError> {
        if required.is_empty() {
            return self.list_typed::<T>(namespace).await;
        }

        let listed = self.list_typed::<T>(namespace).await?;
        let items = listed
            .items
            .into_iter()
            .filter(|object| required.iter().all(|(key, expected)| object.metadata.labels.get(key) == Some(expected)))
            .collect();
        Ok(ResourceList { items, resource_version: listed.resource_version, generation: None })
    }

    pub(crate) async fn create_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let namespace = namespace.to_string();
        let meta = meta.clone();
        let spec = spec.clone();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call(move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource create"))?;
            let exists = tx
                .query_row(
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

            let version = Self::allocate_version(&tx, &key)?;
            let object = ResourceObject::<T> {
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
                },
                spec: Self::clone_through_serde(&spec)?,
                status: None,
            };
            let encoded = Self::encode_object(&object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            tx.execute(
                r#"
                INSERT INTO resource_objects (group_name, version, kind, namespace, name, resource_version, body_json)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                "#,
                params![key.0, key.1, key.2, key.3, meta.name, version, body_json],
            )
            .map_err(|err| Self::map_sqlite(err, "insert sqlite resource object"))?;
            Self::insert_event(&tx, &key, version, StoredEventKind::Added, &body_json, event_retention)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource create"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: StoredEventKind::Added, object: encoded });
            Ok(object)
        })
        .await
    }

    pub(crate) async fn update_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let meta = meta.clone();
        let resource_version = resource_version.to_string();
        let spec = spec.clone();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call(move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource update"))?;
            let existing = Self::select_existing::<T>(&tx, &key, &meta.name)?;
            let mut object = existing.ok_or_else(|| ResourceError::not_found(&meta.name))?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
            }
            T::validate_spec_update(&object.spec, &spec)?;
            if object.matches_update(&meta, &spec)? {
                return Ok(object);
            }

            let version = Self::allocate_version(&tx, &key)?;
            object.metadata.resource_version = version.to_string();
            object.metadata.labels = meta.labels.clone();
            object.metadata.annotations = meta.annotations.clone();
            object.metadata.owner_references = meta.owner_references.clone();
            object.metadata.finalizers = meta.finalizers.clone();
            object.metadata.deletion_timestamp = meta.deletion_timestamp;
            object.spec = Self::clone_through_serde(&spec)?;

            let encoded = Self::encode_object(&object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            let event_kind = if object.metadata.deletion_timestamp.is_some() && object.metadata.finalizers.is_empty() {
                tx.execute(
                    r#"
                    DELETE FROM resource_objects
                    WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                    "#,
                    params![key.0, key.1, key.2, key.3, meta.name],
                )
                .map_err(|err| Self::map_sqlite(err, "delete sqlite resource object"))?;
                StoredEventKind::Deleted
            } else {
                Self::upsert_object(&tx, &key, &meta.name, version, &body_json)?;
                StoredEventKind::Modified
            };
            Self::insert_event(&tx, &key, version, event_kind, &body_json, event_retention)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource update"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: event_kind, object: encoded });
            Ok(object)
        })
        .await
    }

    pub(crate) async fn update_status_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &T::Status,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let name = name.to_string();
        let resource_version = resource_version.to_string();
        let status = status.clone();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call(move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource status update"))?;
            let existing = Self::select_existing::<T>(&tx, &key, &name)?;
            let mut object = existing.ok_or_else(|| ResourceError::not_found(&name))?;
            if object.metadata.resource_version != resource_version {
                return Err(ResourceError::conflict(&name, "stale resourceVersion"));
            }
            if object.matches_status(&status)? {
                return Ok(object);
            }

            let version = Self::allocate_version(&tx, &key)?;
            object.metadata.resource_version = version.to_string();
            object.status = Some(Self::clone_through_serde(&status)?);

            let encoded = Self::encode_object(&object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            Self::upsert_object(&tx, &key, &name, version, &body_json)?;
            Self::insert_event(&tx, &key, version, StoredEventKind::Modified, &body_json, event_retention)?;
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource status update"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: StoredEventKind::Modified, object: encoded });
            Ok(object)
        })
        .await
    }

    pub(crate) async fn delete_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<(), ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let name = name.to_string();
        let watchers = Arc::clone(&self.watchers);
        let event_retention = self.event_retention;
        self.call(move |connection| {
            let tx = connection.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource delete"))?;
            let existing = Self::select_existing::<T>(&tx, &key, &name)?;
            let mut object = existing.ok_or_else(|| ResourceError::not_found(&name))?;
            if object.metadata.is_pending_finalization() {
                return Ok(());
            }
            let version = Self::allocate_version(&tx, &key)?;
            object.metadata.resource_version = version.to_string();

            let (event_kind, encoded) = if !object.metadata.finalizers.is_empty() && object.metadata.deletion_timestamp.is_none() {
                object.metadata.deletion_timestamp = Some(Utc::now());
                let encoded = Self::encode_object(&object)?;
                let body_json =
                    serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
                Self::upsert_object(&tx, &key, &name, version, &body_json)?;
                Self::insert_event(&tx, &key, version, StoredEventKind::Modified, &body_json, event_retention)?;
                (StoredEventKind::Modified, encoded)
            } else {
                let encoded = Self::encode_object(&object)?;
                let body_json =
                    serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
                tx.execute(
                    r#"
                    DELETE FROM resource_objects
                    WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                    "#,
                    params![key.0, key.1, key.2, key.3, name],
                )
                .map_err(|err| Self::map_sqlite(err, "delete sqlite resource object"))?;
                Self::insert_event(&tx, &key, version, StoredEventKind::Deleted, &body_json, event_retention)?;
                (StoredEventKind::Deleted, encoded)
            };
            tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource delete"))?;
            Self::notify_watchers(&watchers, &key, StoredEvent { kind: event_kind, object: encoded });
            Ok(())
        })
        .await
    }

    pub(crate) async fn watch_typed<T: Resource>(&self, namespace: &str, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let replay_from = match &start {
            WatchStart::Now => None,
            WatchStart::FromVersion(version) => {
                Some(version.parse::<u64>().map_err(|err| ResourceError::invalid(format!("invalid resourceVersion '{version}': {err}")))?)
            }
            WatchStart::FromVersionInGeneration { .. } => {
                return Err(ResourceError::invalid("sqlite resource watches do not use generations"));
            }
        };
        let (sender, receiver) = mpsc::unbounded_channel();
        let watchers = Arc::clone(&self.watchers);
        let replay = self
            .call(move |connection| {
                let replay = match replay_from {
                    Some(version) => Self::replay_events(connection, &key, version)?,
                    None => Vec::new(),
                };
                Self::lock_watchers(&watchers)?.entry(key).or_default().push(sender);
                Ok(replay)
            })
            .await?;

        let replay_stream = stream::iter(replay.into_iter().map(Self::decode_event::<T>));
        let live_stream = stream::unfold(receiver, |mut receiver| async {
            receiver.recv().await.map(|event| (Self::decode_event::<T>(event), receiver))
        });
        Ok(WatchStream::new(None, Box::pin(replay_stream.chain(live_stream))))
    }

    fn select_existing<T: Resource>(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        name: &str,
    ) -> Result<Option<ResourceObject<T>>, ResourceError> {
        let body = tx
            .query_row(
                r#"
                SELECT body_json FROM resource_objects
                WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                "#,
                params![key.0, key.1, key.2, key.3, name],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource object"))?;
        body.map(|body| {
            let value = serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode stored object JSON: {err}")))?;
            Self::decode_object::<T>(value)
        })
        .transpose()
    }

    fn upsert_object(
        tx: &rusqlite::Transaction<'_>,
        key: &StoreKey,
        name: &str,
        resource_version: u64,
        body_json: &str,
    ) -> Result<(), ResourceError> {
        tx.execute(
            r#"
            INSERT INTO resource_objects (group_name, version, kind, namespace, name, resource_version, body_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(group_name, version, kind, namespace, name)
            DO UPDATE SET resource_version = excluded.resource_version, body_json = excluded.body_json
            "#,
            params![key.0, key.1, key.2, key.3, name, resource_version, body_json],
        )
        .map_err(|err| Self::map_sqlite(err, "upsert sqlite resource object"))?;
        Ok(())
    }

    fn replay_events(conn: &RusqliteConnection, key: &StoreKey, replay_from: u64) -> Result<Vec<StoredEvent>, ResourceError> {
        let compacted_through = conn
            .query_row(
                r#"
                SELECT compacted_through FROM resource_event_compaction
                WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4
                "#,
                params![key.0, key.1, key.2, key.3],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .map_err(|err| Self::map_sqlite(err, "read sqlite resource event compaction floor"))?
            .unwrap_or(0);
        if replay_from < compacted_through {
            return Err(ResourceError::WatchExpired {
                requested_version: replay_from.to_string(),
                compacted_through: Some(compacted_through.to_string()),
            });
        }
        let mut statement = conn
            .prepare(
                r#"
                SELECT event_type, body_json FROM resource_events
                WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND event_version > ?5
                ORDER BY event_version
                "#,
            )
            .map_err(|err| Self::map_sqlite(err, "prepare sqlite resource event replay"))?;
        let rows = statement
            .query_map(params![key.0, key.1, key.2, key.3, replay_from], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|err| Self::map_sqlite(err, "query sqlite resource event replay"))?;
        let mut events = Vec::new();
        for row in rows {
            let (event_type, body_json) = row.map_err(|err| Self::map_sqlite(err, "read sqlite resource event replay row"))?;
            let object =
                serde_json::from_str(&body_json).map_err(|err| ResourceError::decode(format!("decode stored event JSON: {err}")))?;
            events.push(StoredEvent { kind: StoredEventKind::from_str(&event_type)?, object });
        }
        Ok(events)
    }
}
