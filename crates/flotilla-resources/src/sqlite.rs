use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use chrono::Utc;
use futures::{stream, StreamExt};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::{
    error::ResourceError,
    resource::{InputMeta, K8sResourceObject, ObjectMeta, Resource, ResourceObject},
    retention::{EventRetention, ResourceStoreDiagnostics},
    watch::{ResourceList, WatchEvent, WatchStart, WatchStream},
};

type StoreKey = (String, String, String, String);
type WatchSender = mpsc::UnboundedSender<StoredEvent>;
type WatchersByStore = HashMap<StoreKey, Vec<WatchSender>>;

#[derive(Debug, Clone)]
pub struct SqliteBackend {
    // rusqlite is synchronous. The embedded daemon currently serializes SQLite
    // access behind one mutex; move this behind spawn_blocking or tokio-rusqlite
    // if controller contention shows up in practice.
    connection: Arc<Mutex<Connection>>,
    watchers: Arc<Mutex<WatchersByStore>>,
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
        let connection = Connection::open(path).map_err(|err| ResourceError::other(format!("open sqlite resource store: {err}")))?;
        Self::from_connection(connection, event_retention)
    }

    pub fn open_in_memory() -> Result<Self, ResourceError> {
        Self::open_in_memory_with_event_retention(EventRetention::default())
    }

    pub fn open_in_memory_with_event_retention(event_retention: EventRetention) -> Result<Self, ResourceError> {
        let connection = Connection::open_in_memory().map_err(|err| ResourceError::other(format!("open sqlite resource store: {err}")))?;
        Self::from_connection(connection, event_retention)
    }

    fn from_connection(mut connection: Connection, event_retention: EventRetention) -> Result<Self, ResourceError> {
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
                "#,
            )
            .map_err(|err| ResourceError::other(format!("initialize sqlite resource store: {err}")))?;
        let startup_diagnostics = Self::diagnostics_from_connection(&connection, event_retention)?;
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
        Self::compact_existing_events(&mut connection, event_retention)?;
        Ok(Self { connection: Arc::new(Mutex::new(connection)), watchers: Arc::new(Mutex::new(HashMap::new())), event_retention })
    }

    fn store_key<T: Resource>(namespace: &str) -> StoreKey {
        (T::API_PATHS.group.to_string(), T::API_PATHS.version.to_string(), T::API_PATHS.kind.to_string(), namespace.to_string())
    }

    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, ResourceError> {
        self.connection.lock().map_err(|_| ResourceError::other("sqlite resource store lock poisoned"))
    }

    fn lock_watchers(&self) -> Result<MutexGuard<'_, WatchersByStore>, ResourceError> {
        self.watchers.lock().map_err(|_| ResourceError::other("sqlite resource watch lock poisoned"))
    }

    pub(crate) async fn diagnostics(&self) -> Result<ResourceStoreDiagnostics, ResourceError> {
        let connection = self.lock_connection()?;
        Self::diagnostics_from_connection(&connection, self.event_retention)
    }

    fn diagnostics_from_connection(
        connection: &Connection,
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

    fn current_version(conn: &Connection, key: &StoreKey) -> Result<u64, ResourceError> {
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

    fn compact_existing_events(connection: &mut Connection, event_retention: EventRetention) -> Result<(), ResourceError> {
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

    fn notify_watchers(&self, key: &StoreKey, event: StoredEvent) {
        if let Ok(mut watchers) = self.lock_watchers() {
            if let Some(entries) = watchers.get_mut(key) {
                entries.retain(|watcher| watcher.send(event.clone()).is_ok());
            }
        }
    }

    pub(crate) async fn get_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let conn = self.lock_connection()?;
        let body: String = conn
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
            .ok_or_else(|| ResourceError::not_found(name))?;
        let value = serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode stored object JSON: {err}")))?;
        Self::decode_object::<T>(value)
    }

    pub(crate) async fn list_typed<T: Resource>(&self, namespace: &str) -> Result<ResourceList<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let conn = self.lock_connection()?;
        let mut statement = conn
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
            let value = serde_json::from_str(&body).map_err(|err| ResourceError::decode(format!("decode stored object JSON: {err}")))?;
            items.push(Self::decode_object::<T>(value)?);
        }
        Ok(ResourceList { items, resource_version: Self::current_version(&conn, &key)?.to_string(), generation: None })
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
        let mut conn = self.lock_connection()?;
        let tx = conn.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource create"))?;
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
                namespace: namespace.to_string(),
                resource_version: version.to_string(),
                labels: meta.labels.clone(),
                annotations: meta.annotations.clone(),
                owner_references: meta.owner_references.clone(),
                finalizers: meta.finalizers.clone(),
                deletion_timestamp: meta.deletion_timestamp,
                creation_timestamp: Utc::now(),
            },
            spec: Self::clone_through_serde(spec)?,
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
        Self::insert_event(&tx, &key, version, StoredEventKind::Added, &body_json, self.event_retention)?;
        tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource create"))?;
        self.notify_watchers(&key, StoredEvent { kind: StoredEventKind::Added, object: encoded });
        Ok(object)
    }

    pub(crate) async fn update_typed<T: Resource>(
        &self,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let mut conn = self.lock_connection()?;
        let tx = conn.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource update"))?;
        let existing = Self::select_existing::<T>(&tx, &key, &meta.name)?;
        let mut object = existing.ok_or_else(|| ResourceError::not_found(&meta.name))?;
        if object.metadata.resource_version != resource_version {
            return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
        }
        if object.matches_update(meta, spec)? {
            return Ok(object);
        }

        let version = Self::allocate_version(&tx, &key)?;
        object.metadata.resource_version = version.to_string();
        object.metadata.labels = meta.labels.clone();
        object.metadata.annotations = meta.annotations.clone();
        object.metadata.owner_references = meta.owner_references.clone();
        object.metadata.finalizers = meta.finalizers.clone();
        object.metadata.deletion_timestamp = meta.deletion_timestamp;
        object.spec = Self::clone_through_serde(spec)?;

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
        Self::insert_event(&tx, &key, version, event_kind, &body_json, self.event_retention)?;
        tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource update"))?;
        self.notify_watchers(&key, StoredEvent { kind: event_kind, object: encoded });
        Ok(object)
    }

    pub(crate) async fn update_status_typed<T: Resource>(
        &self,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &T::Status,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let mut conn = self.lock_connection()?;
        let tx = conn.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource status update"))?;
        let existing = Self::select_existing::<T>(&tx, &key, name)?;
        let mut object = existing.ok_or_else(|| ResourceError::not_found(name))?;
        if object.metadata.resource_version != resource_version {
            return Err(ResourceError::conflict(name, "stale resourceVersion"));
        }
        if object.matches_status(status)? {
            return Ok(object);
        }

        let version = Self::allocate_version(&tx, &key)?;
        object.metadata.resource_version = version.to_string();
        object.status = Some(Self::clone_through_serde(status)?);

        let encoded = Self::encode_object(&object)?;
        let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
        Self::upsert_object(&tx, &key, name, version, &body_json)?;
        Self::insert_event(&tx, &key, version, StoredEventKind::Modified, &body_json, self.event_retention)?;
        tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource status update"))?;
        self.notify_watchers(&key, StoredEvent { kind: StoredEventKind::Modified, object: encoded });
        Ok(object)
    }

    pub(crate) async fn delete_typed<T: Resource>(&self, namespace: &str, name: &str) -> Result<(), ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let mut conn = self.lock_connection()?;
        let tx = conn.transaction().map_err(|err| Self::map_sqlite(err, "begin sqlite resource delete"))?;
        let existing = Self::select_existing::<T>(&tx, &key, name)?;
        let mut object = existing.ok_or_else(|| ResourceError::not_found(name))?;
        if object.metadata.is_pending_finalization() {
            return Ok(());
        }
        let version = Self::allocate_version(&tx, &key)?;
        object.metadata.resource_version = version.to_string();

        let (event_kind, encoded) = if !object.metadata.finalizers.is_empty() && object.metadata.deletion_timestamp.is_none() {
            object.metadata.deletion_timestamp = Some(Utc::now());
            let encoded = Self::encode_object(&object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            Self::upsert_object(&tx, &key, name, version, &body_json)?;
            Self::insert_event(&tx, &key, version, StoredEventKind::Modified, &body_json, self.event_retention)?;
            (StoredEventKind::Modified, encoded)
        } else {
            let encoded = Self::encode_object(&object)?;
            let body_json = serde_json::to_string(&encoded).map_err(|err| ResourceError::decode(format!("encode object JSON: {err}")))?;
            tx.execute(
                r#"
                DELETE FROM resource_objects
                WHERE group_name = ?1 AND version = ?2 AND kind = ?3 AND namespace = ?4 AND name = ?5
                "#,
                params![key.0, key.1, key.2, key.3, name],
            )
            .map_err(|err| Self::map_sqlite(err, "delete sqlite resource object"))?;
            Self::insert_event(&tx, &key, version, StoredEventKind::Deleted, &body_json, self.event_retention)?;
            (StoredEventKind::Deleted, encoded)
        };
        tx.commit().map_err(|err| Self::map_sqlite(err, "commit sqlite resource delete"))?;
        self.notify_watchers(&key, StoredEvent { kind: event_kind, object: encoded });
        Ok(())
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
        let replay = {
            let conn = self.lock_connection()?;
            let replay = match replay_from {
                Some(version) => Self::replay_events(&conn, &key, version)?,
                None => Vec::new(),
            };
            self.lock_watchers()?.entry(key.clone()).or_default().push(sender);
            replay
        };

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

    fn replay_events(conn: &Connection, key: &StoreKey, replay_from: u64) -> Result<Vec<StoredEvent>, ResourceError> {
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
                compacted_through: compacted_through.to_string(),
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
