use rusqlite::functions::FunctionFlags;

use super::*;
use crate::{
    digest::{hash_entries, DIGEST_FANOUT},
    DigestQuery, PartitionDigest,
};

const BOOTSTRAP_DIGEST_INDEX_SQL: &str = "INSERT OR REPLACE INTO digest_entries SELECT '',group_name,version,kind,namespace,digest_bucket(name),name,CAST(resource_version AS TEXT)
                FROM resource_objects;
                INSERT OR REPLACE INTO digest_entries SELECT origin_root,group_name,version,kind,namespace,digest_bucket(name),name,CASE WHEN json_valid(body_json) THEN COALESCE(json_extract(body_json,'$.metadata.resourceVersion'),'invalid') ELSE 'invalid' END
                FROM replica_objects;
                INSERT OR REPLACE INTO digest_nodes SELECT origin,group_name,version,kind,namespace,bucket,digest_hash(json_group_array(json_array(name,resource_version)))
                FROM digest_entries GROUP BY origin,group_name,version,kind,namespace,bucket;
                INSERT INTO resource_store_migrations(name) VALUES('digest-index-v1');";

impl SqliteBackend {
    pub(super) fn register_digest_functions(connection: &RusqliteConnection) -> Result<(), ResourceError> {
        let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
        connection
            .create_scalar_function("digest_bucket", 1, flags, |context| Ok(crate::digest_bucket(&context.get::<String>(0)?)))
            .map_err(|error| Self::map_sqlite(error, "register digest bucket"))?;
        connection
            .create_scalar_function("digest_hash", 1, flags, |context| {
                let entries: Vec<(String, String)> = serde_json::from_str(&context.get::<String>(0)?)
                    .map_err(|error| rusqlite::Error::UserFunctionError(Box::new(error)))?;
                Ok(hash_entries(&entries.into_iter().collect()))
            })
            .map_err(|error| Self::map_sqlite(error, "register digest hash"))?;
        Ok(())
    }

    /// Every production writer must be opened through SqliteBackend and run
    /// this initialization on its write connection. TEMP triggers intentionally
    /// avoid requiring custom functions on administrative/read-only connections.
    /// Direct SQL object writes bypass this invariant and are unsupported; any
    /// future write pool must initialize each connection before admitting writes.
    pub(super) fn initialize_digests(connection: &mut RusqliteConnection) -> Result<(), ResourceError> {
        Self::register_digest_functions(connection)?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS digest_entries (
            origin TEXT NOT NULL, group_name TEXT NOT NULL, version TEXT NOT NULL, kind TEXT NOT NULL, namespace TEXT NOT NULL,
            bucket INTEGER NOT NULL, name TEXT NOT NULL, resource_version TEXT NOT NULL,
            PRIMARY KEY(origin, group_name, version, kind, namespace, bucket, name));
            CREATE TABLE IF NOT EXISTS digest_nodes (
            origin TEXT NOT NULL, group_name TEXT NOT NULL, version TEXT NOT NULL, kind TEXT NOT NULL, namespace TEXT NOT NULL,
            bucket INTEGER NOT NULL, hash TEXT NOT NULL,
            PRIMARY KEY(origin, group_name, version, kind, namespace, bucket));",
            )
            .map_err(|error| Self::map_sqlite(error, "create digest index"))?;
        // Triggers update only the affected leaf, in the same transaction as the
        // object write. Failed/rolled-back writes cannot publish a digest.
        for action in ["INSERT", "UPDATE", "DELETE"] {
            let row = if action == "DELETE" { "OLD" } else { "NEW" };
            let refresh = format!(
                "INSERT INTO digest_nodes SELECT {row}.origin, {row}.group_name, {row}.version, {row}.kind, {row}.namespace, {row}.bucket,
                digest_hash(json_group_array(json_array(name,resource_version)))
                FROM digest_entries
                WHERE origin={row}.origin
                AND group_name={row}.group_name
                AND version={row}.version
                AND kind={row}.kind
                AND namespace={row}.namespace
                AND bucket={row}.bucket
                ON CONFLICT(origin,group_name,version,kind,namespace,bucket) DO UPDATE SET hash=excluded.hash;"
            );
            connection
                .execute_batch(&format!(
                    "CREATE TEMP TRIGGER IF NOT EXISTS digest_entries_{action} AFTER {action} ON digest_entries BEGIN {refresh} END;"
                ))
                .map_err(|error| Self::map_sqlite(error, "create digest leaf trigger"))?;
            for (table, origin, version) in [
                ("resource_objects", "''".to_string(), format!("CAST({row}.resource_version AS TEXT)")),
                ("replica_objects", format!("{row}.origin_root"), format!("CASE WHEN json_valid({row}.body_json) THEN COALESCE(json_extract({row}.body_json,'$.metadata.resourceVersion'),'invalid') ELSE 'invalid' END")),
            ] {
                let mutation = if action == "DELETE" {
                    format!("DELETE
                FROM digest_entries WHERE origin={origin}
                AND group_name={row}.group_name
                AND version={row}.version
                AND kind={row}.kind
                AND namespace={row}.namespace
                AND bucket=digest_bucket({row}.name)
                AND name={row}.name;")
                } else {
                    format!("INSERT INTO digest_entries VALUES({origin},{row}.group_name,{row}.version,{row}.kind,{row}.namespace,digest_bucket({row}.name),{row}.name,{version})
                ON CONFLICT(origin,group_name,version,kind,namespace,bucket,name) DO UPDATE SET resource_version=excluded.resource_version;")
                };
                connection.execute_batch(&format!("CREATE TEMP TRIGGER IF NOT EXISTS {table}_digest_{action} AFTER {action} ON {table} BEGIN {mutation} END;"))
                    .map_err(|error| Self::map_sqlite(error, "create object digest trigger"))?;
            }
        }
        // Bootstrap the derived index once, including empty stores. It is not a
        // recurring scan and does not decode or transfer resource bodies.
        let initialized: bool = connection
            .query_row("SELECT EXISTS(SELECT 1 FROM resource_store_migrations WHERE name='digest-index-v1')", [], |row| row.get(0))
            .map_err(|error| Self::map_sqlite(error, "read digest migration"))?;
        if !initialized {
            let tx = connection.transaction().map_err(|error| Self::map_sqlite(error, "begin digest bootstrap"))?;
            // Avoid per-row trigger hashing during the bulk backfill.
            for action in ["INSERT", "UPDATE", "DELETE"] {
                tx.execute_batch(&format!("DROP TRIGGER digest_entries_{action};"))
                    .map_err(|error| Self::map_sqlite(error, "suspend digest trigger"))?;
            }
            tx.execute_batch(BOOTSTRAP_DIGEST_INDEX_SQL).map_err(|error| Self::map_sqlite(error, "bootstrap digest index"))?;
            tx.commit().map_err(|error| Self::map_sqlite(error, "commit digest bootstrap"))?;
            // Install the leaf triggers now that the bootstrap marker exists.
            Self::initialize_digests(connection)?;
        }
        Ok(())
    }

    fn digest_hashes(connection: &RusqliteConnection, key: &StoreKey, origin: &str) -> Result<Vec<String>, ResourceError> {
        let mut hashes = vec![hash_entries(&BTreeMap::new()); DIGEST_FANOUT];
        let mut statement = connection
            .prepare(
                "SELECT bucket,hash FROM digest_nodes WHERE origin=?1
                AND group_name=?2
                AND version=?3
                AND kind=?4
                AND namespace=?5",
            )
            .map_err(|error| Self::map_sqlite(error, "prepare digest nodes"))?;
        let rows = statement
            .query_map(params![origin, key.0, key.1, key.2, key.3], |row| Ok((row.get::<_, usize>(0)?, row.get::<_, String>(1)?)))
            .map_err(|error| Self::map_sqlite(error, "read digest nodes"))?;
        for row in rows {
            let (bucket, hash) = row.map_err(|error| Self::map_sqlite(error, "decode digest node"))?;
            *hashes.get_mut(bucket).ok_or_else(|| ResourceError::invalid("invalid cached digest bucket"))? = hash;
        }
        Ok(hashes)
    }

    pub(crate) async fn digest_typed<T: Resource>(&self, namespace: &str, query: &DigestQuery) -> Result<PartitionDigest, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let query = query.clone();
        let origin = self.local_root();
        self.read_call("read authoritative digest", move |connection| {
            let tx = connection.transaction().map_err(|error| Self::map_sqlite(error, "begin consistent digest read"))?;
            let quarantined: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM resource_decode_quarantine WHERE group_name=?1
                AND version=?2
                AND kind=?3
                AND namespace=?4)",
                    params![key.0, key.1, key.2, key.3],
                    |row| row.get(0),
                )
                .map_err(|error| Self::map_sqlite(error, "check digest quarantine"))?;
            if quarantined {
                return Err(ResourceError::invalid("quarantined objects cannot prove authoritative absence"));
            }
            let mut response = PartitionDigest::new::<T>(
                origin,
                &key.3,
                None,
                Self::current_version(&tx, &key)?.to_string(),
                Self::digest_hashes(&tx, &key, "")?,
            )
            .select(&query)?;
            if let DigestQuery::Snapshot { bucket, .. } = query {
                let mut statement = tx
                    .prepare(
                        "SELECT o.body_json
                FROM digest_entries d JOIN resource_objects o USING(group_name,version,kind,namespace,name) WHERE d.origin=''
                AND d.group_name=?1
                AND d.version=?2
                AND d.kind=?3
                AND d.namespace=?4
                AND d.bucket=?5 ORDER BY d.name",
                    )
                    .map_err(|error| Self::map_sqlite(error, "prepare digest snapshot"))?;
                let rows = statement
                    .query_map(params![key.0, key.1, key.2, key.3, bucket], |row| row.get::<_, String>(0))
                    .map_err(|error| Self::map_sqlite(error, "read digest snapshot"))?;
                let mut items = Vec::new();
                for row in rows {
                    let value: Value = serde_json::from_str(&row.map_err(|error| Self::map_sqlite(error, "read digest body"))?)
                        .map_err(|error| ResourceError::decode(error.to_string()))?;
                    Self::decode_object::<T>(value.clone())?;
                    items.push(value);
                }
                response.bucket = Some(bucket);
                response.items = Some(items);
            }
            Ok(response)
        })
        .await
    }

    pub(crate) async fn confirm_replica_digest_typed<T: Resource>(
        &self,
        origin: &NodeId,
        namespace: &str,
        proof: &PartitionDigest,
        previous: &ReplicaCursor,
    ) -> Result<(), ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let origin = origin.clone();
        let proof = proof.clone();
        let previous = previous.clone();
        self.call("confirm repaired digest prefix", move |connection| {
            let tx = connection.transaction().map_err(|error| Self::map_sqlite(error, "begin digest prefix confirmation"))?;
            let digest = PartitionDigest::new::<T>(
                origin.clone(),
                &key.3,
                previous.generation.clone(),
                String::new(),
                Self::digest_hashes(&tx, &key, origin.as_str())?,
            );
            if digest.root != proof.root {
                return Err(ResourceError::invalid("replica set changed during digest repair"));
            }
            let changed = tx
                .execute(
                    "UPDATE replica_cursors SET resource_version=?6 WHERE origin_root=?1
                AND group_name=?2
                AND version=?3
                AND kind=?4
                AND namespace=?5
                AND complete_prefix=1
                AND resource_version=?7
                AND generation IS ?8",
                    params![
                        origin.as_str(),
                        key.0,
                        key.1,
                        key.2,
                        key.3,
                        proof.resource_version,
                        previous.resource_version,
                        previous.generation
                    ],
                )
                .map_err(|error| Self::map_sqlite(error, "confirm digest prefix"))?;
            if changed != 1 {
                return Err(ResourceError::invalid("replica prefix changed during digest repair"));
            }
            tx.commit().map_err(|error| Self::map_sqlite(error, "commit digest prefix confirmation"))?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn replica_digest_typed<T: Resource>(
        &self,
        origin: &NodeId,
        namespace: &str,
        generation: Option<String>,
    ) -> Result<PartitionDigest, ResourceError> {
        let key = Self::store_key::<T>(namespace);
        let origin = origin.clone();
        self.read_call("read replica digest", move |connection| {
            Ok(PartitionDigest::new::<T>(
                origin.clone(),
                &key.3,
                generation,
                String::new(),
                Self::digest_hashes(connection, &key, origin.as_str())?,
            ))
        })
        .await
    }
}
