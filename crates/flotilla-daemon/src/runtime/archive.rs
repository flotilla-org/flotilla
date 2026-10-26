//! Ships ended engine resources to the object store, then removes them locally (#3939, ADR 0064
//! Archive).
//!
//! Each pass brings the [`Archiver`] up to date with the decided store and works what is due:
//! a resource a week past its end is bundled and uploaded to every `[[blob_stores]]` store, each
//! upload confirmed by reading the bundle back, and only then deleted; a pass later it is
//! expunged. A failed upload leaves the resource in place for the next pass. A bundle still
//! unshipped a day after it was due raises the `Archive` host condition, which fleet health
//! shows to the `infra` role (ADR 0054), and is logged for that role.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use flotilla_engine_store::archive::{
    bundle_key, ArchiveError, ArchiveKinds, ArchiveNote, Archiver, Bundle, Due, Prepared, Stage, BUNDLE_MEDIA_TYPE,
};
use flotilla_resources::{Clock, ConditionValue, HostCondition};
use flotilla_schema::{Identity, KindRegistry, Timestamp};
use flotilla_schema_sqlite::Store;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::blob_store::S3BlobStore;

#[cfg(test)]
mod tests;

/// How often the shipper looks for due work.
pub(crate) const PASS_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// How long a delete stands before its expunge, so followers tail both (see
/// `flotilla_engine_store::archive`).
pub(crate) const EXPUNGE_GRACE: Duration = PASS_INTERVAL;
/// How long past due a bundle may stay unshipped before the `infra` role is alerted.
pub(crate) const ALERT_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// The host condition a stuck bundle raises.
pub(crate) const ARCHIVE_CONDITION: &str = "Archive";

fn micros(duration: Duration) -> i64 {
    i64::try_from(duration.as_micros()).unwrap_or(i64::MAX)
}

/// One object store bundles are uploaded to, by name.
#[async_trait]
pub(crate) trait ArchiveTarget: Send + Sync {
    /// Where the store is, for errors.
    fn location(&self) -> String;
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String>;
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
}

#[async_trait]
impl ArchiveTarget for S3BlobStore {
    fn location(&self) -> String {
        S3BlobStore::location(self)
    }
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        self.put_named(key, bytes, BUNDLE_MEDIA_TYPE).await
    }
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_named(key).await
    }
}

/// The decided store's kinds whose ended resources archive: every machine it holds.
pub(crate) fn archive_kinds() -> ArchiveKinds {
    use flotilla_provisioning::stored::{
        checkout::Checkout, clone::Clone, environment::Environment, image_build::ImageBuild, terminal_session::TerminalSession,
    };
    use flotilla_state_machines::machines::{
        agent_session::AgentSession, awaiting_reply::AwaitingReply, convoy::Convoy, ensure::Ensure, message::Message, vessel::Vessel,
    };
    ArchiveKinds::new()
        .with::<Message>()
        .with::<AgentSession>()
        .with::<Convoy>()
        .with::<AwaitingReply>()
        .with::<Environment>()
        .with::<ImageBuild>()
        .with::<Clone>()
        .with::<Checkout>()
        .with::<TerminalSession>()
        .with::<Vessel>()
        .with::<Ensure>()
}

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PassReport {
    pub(crate) shipped: Vec<Identity>,
    pub(crate) expunged: Vec<Identity>,
    pub(crate) failed: Vec<(Identity, String)>,
    /// Due, but left for a later pass after a store failed this one.
    pub(crate) deferred: Vec<Identity>,
    /// Ended resources unshipped past [`ALERT_AFTER`].
    pub(crate) overdue: usize,
}

type Report = Arc<dyn Fn(Option<HostCondition>) + Send + Sync>;

/// Why a resource did not ship: its store failed, or its home did.
enum ShipError {
    Store(String),
    Local(String),
}

impl ShipError {
    fn into_message(self) -> String {
        match self {
            Self::Store(message) | Self::Local(message) => message,
        }
    }
}

pub(crate) struct ArchiveShipper {
    store: Arc<Mutex<Store>>,
    archiver: Arc<Mutex<Archiver>>,
    targets: Vec<Arc<dyn ArchiveTarget>>,
    clock: Arc<dyn Clock>,
    /// The last failure of each candidate, for the alert.
    failures: BTreeMap<Identity, String>,
    alerting: bool,
    report: Report,
}

impl ArchiveShipper {
    /// `report` receives the `Archive` condition after every pass: present while a bundle is
    /// overdue, absent otherwise.
    pub(crate) fn new(
        store: Arc<Mutex<Store>>,
        kinds: ArchiveKinds,
        targets: Vec<Arc<dyn ArchiveTarget>>,
        clock: Arc<dyn Clock>,
        report: Report,
    ) -> Self {
        let home = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).home().to_owned();
        Self {
            store,
            archiver: Arc::new(Mutex::new(Archiver::new(home, kinds))),
            targets,
            clock,
            failures: BTreeMap::new(),
            alerting: false,
            report,
        }
    }

    pub(crate) async fn run(mut self, interval: Duration) {
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            if let Err(error) = self.pass().await {
                tracing::warn!(%error, "archive pass failed");
            }
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        op: impl FnOnce(&mut Archiver, &mut Store) -> Result<T, ArchiveError> + Send + 'static,
    ) -> Result<T, String> {
        let (archiver, store) = (Arc::clone(&self.archiver), Arc::clone(&self.store));
        tokio::task::spawn_blocking(move || {
            let mut archiver = archiver.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut store = store.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            op(&mut archiver, &mut store)
        })
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
    }

    pub(crate) async fn pass(&mut self) -> Result<PassReport, String> {
        let now = Timestamp::from_datetime(&self.clock.now());
        let grace = micros(EXPUNGE_GRACE);
        let due = self
            .blocking(move |archiver, store| {
                archiver.refresh(store)?;
                Ok(archiver.due(now, grace))
            })
            .await?;
        let mut report = PassReport::default();
        // A store that refused one bundle is not asked for the rest of this pass's: an outage
        // costs one failed request per pass, not one per ended resource.
        let mut store_down = false;
        for due in due {
            match due {
                Due::Ship(identity) if store_down => report.deferred.push(identity),
                Due::Ship(identity) => match self.ship(&identity, now).await {
                    Ok(shipped) => {
                        self.failures.remove(&identity);
                        if shipped {
                            report.shipped.push(identity);
                        }
                    }
                    Err(error) => {
                        store_down |= matches!(error, ShipError::Store(_));
                        let error = error.into_message();
                        tracing::warn!(kind = %identity.kind, name = %identity.name, %error, "archive shipping failed; the resource stays");
                        self.failures.insert(identity.clone(), error.clone());
                        report.failed.push((identity, error));
                    }
                },
                Due::Expunge(identity, note) => {
                    let target = identity.clone();
                    match self.blocking(move |archiver, store| archiver.expunge(store, &target, &note, now)).await {
                        Ok(true) => report.expunged.push(identity),
                        // Still referenced: the identity stays as the tombstone, retried next pass.
                        Ok(false) => {}
                        Err(error) => report.failed.push((identity, error)),
                    }
                }
            }
        }
        report.overdue = self.alert(now).await?;
        Ok(report)
    }

    /// Bundles, uploads and confirms, then deletes. `false` when the resource is no longer there
    /// to ship.
    async fn ship(&self, identity: &Identity, now: Timestamp) -> Result<bool, ShipError> {
        let target = identity.clone();
        let prepared = self.blocking(move |archiver, store| archiver.prepare(store, &target)).await.map_err(ShipError::Local)?;
        let Prepared::Ready { bundle, version } = prepared else { return Ok(false) };
        let bytes = bundle.encode().map_err(|error| ShipError::Local(error.to_string()))?;
        let key = bundle_key(identity);
        let digest = Sha256::digest(&bytes);
        for store in &self.targets {
            store.put(&key, &bytes).await.map_err(|error| ShipError::Store(format!("{}: upload: {error}", store.location())))?;
            let held = store.get(&key).await.map_err(|error| ShipError::Store(format!("{}: confirm: {error}", store.location())))?;
            if held.map(|held| Sha256::digest(&held)) != Some(digest) {
                return Err(ShipError::Store(format!("{}: confirm: the store does not hold the uploaded bundle", store.location())));
            }
        }
        let note = ArchiveNote { key, digest: format!("{digest:x}") };
        let target = identity.clone();
        self.blocking(move |archiver, store| archiver.delete(store, &target, version, &note, now)).await.map_err(ShipError::Local)?;
        Ok(true)
    }

    /// Raises or clears the `Archive` condition; the number of overdue bundles.
    async fn alert(&mut self, now: Timestamp) -> Result<usize, String> {
        let alert_after = micros(ALERT_AFTER);
        let mut overdue: Vec<_> = self
            .blocking(move |archiver, store| {
                // Take in this pass's own deletes first.
                archiver.refresh(store)?;
                Ok(archiver
                    .candidates()
                    .filter(|candidate| candidate.stage == Stage::Ended && now.since(candidate.due()) >= alert_after)
                    .cloned()
                    .collect::<Vec<_>>())
            })
            .await?;
        self.failures.retain(|identity, _| overdue.iter().any(|candidate| &candidate.identity == identity));
        overdue.sort_by_key(|candidate| candidate.due());
        let Some(oldest) = overdue.first() else {
            if std::mem::take(&mut self.alerting) {
                tracing::info!(role = "infra", "archive backlog cleared");
            }
            (self.report)(None);
            return Ok(0);
        };
        let identity = &oldest.identity;
        let due = oldest.due().to_datetime().map_or_else(|| oldest.due().to_string(), |time| time.to_rfc3339());
        let reason = self.failures.get(identity).map_or_else(String::new, |error| format!(": {error}"));
        let message = format!(
            "{} ended resource(s) unshipped more than {}h past due; oldest {}/{}/{} due {due}{reason}",
            overdue.len(),
            ALERT_AFTER.as_secs() / 3600,
            identity.home,
            identity.kind,
            identity.name
        );
        if !std::mem::replace(&mut self.alerting, true) {
            tracing::error!(role = "infra", overdue = overdue.len(), %message, "archive bundle unshipped");
        }
        (self.report)(Some(
            HostCondition::builder()
                .condition_type(ARCHIVE_CONDITION)
                .value(ConditionValue::False)
                .reason("BundleUnshipped")
                .message(message)
                .observed_at(self.clock.now())
                // The host keeps working; only its disk grows.
                .blocks_readiness(false)
                .blocks_forge_observation(false)
                .build(),
        ));
        Ok(overdue.len())
    }
}

/// The archive's object stores: every configured fleet blob store.
pub(crate) fn targets(stores: &[flotilla_config::BlobStoreConfig]) -> Result<Vec<Arc<dyn ArchiveTarget>>, String> {
    stores.iter().map(|config| S3BlobStore::from_config(config).map(|store| Arc::new(store) as Arc<dyn ArchiveTarget>)).collect()
}

/// One archived resource by name, as [`readable`] shows it: `kind` is the kind's name (any case)
/// or its code.
pub(crate) async fn read_by_name(
    targets: &[Arc<dyn ArchiveTarget>],
    kinds: &KindRegistry,
    kind: &str,
    name: &str,
    home: &str,
) -> Result<Option<Value>, String> {
    // A code names one kind exactly, so it wins over a name that happens to spell it.
    let set = kinds
        .iter()
        .find(|set| set.code.as_str() == kind)
        .or_else(|| kinds.iter().find(|set| set.kind.eq_ignore_ascii_case(kind)))
        .ok_or_else(|| format!("unknown kind {kind}"))?;
    let identity = Identity { kind: set.code, name: name.to_owned(), home: home.to_owned() };
    read_archived(targets, &identity).await?.map(|bundle| readable(&bundle, kinds)).transpose()
}

/// The bundle of an archived resource, from the first store that holds it.
pub(crate) async fn read_archived(targets: &[Arc<dyn ArchiveTarget>], identity: &Identity) -> Result<Option<Bundle>, String> {
    let key = bundle_key(identity);
    let mut errors = Vec::new();
    for store in targets {
        match store.get(&key).await {
            Ok(Some(bytes)) => {
                let bundle = Bundle::decode(&bytes).map_err(|error| format!("{}: {error}", store.location()))?;
                if bundle.identity() != *identity {
                    return Err(format!("{}: the bundle at {key} holds another resource", store.location()));
                }
                return Ok(Some(bundle));
            }
            Ok(None) => {}
            Err(error) => errors.push(format!("{}: {error}", store.location())),
        }
    }
    if errors.is_empty() {
        Ok(None)
    } else {
        Err(errors.join("; "))
    }
}

/// A bundle as a reader sees it: the kind by name, each table's rows by column name, and the
/// entries with their edges and labels named, times as RFC 3339.
pub(crate) fn readable(bundle: &Bundle, kinds: &KindRegistry) -> Result<Value, String> {
    use flotilla_schema::Value as Cell;
    let snapshot = &bundle.snapshot;
    let set = kinds.iter().find(|set| set.code == snapshot.kind).ok_or_else(|| format!("kind {} is not known here", snapshot.kind))?;
    let time = |at: Timestamp| at.to_datetime().map_or(json!(at.micros()), |time| json!(time.to_rfc3339()));
    let cell = |column: Option<&flotilla_schema::ColumnDef>, value: &Cell| -> Value {
        match value {
            Cell::Null => Value::Null,
            Cell::Int(value) => match column.map(|column| &column.ty) {
                Some(flotilla_schema::ColumnType::Bool) => json!(*value != 0),
                Some(flotilla_schema::ColumnType::Timestamp) => time(Timestamp(*value)),
                _ => json!(value),
            },
            Cell::Text(value) | Cell::Str(value) => json!(value),
            Cell::Blob(bytes) => match column.map(|column| &column.ty) {
                Some(flotilla_schema::ColumnType::Cbor) => {
                    flotilla_cbor::from_slice::<Value>(bytes, &Default::default()).unwrap_or_else(|_| json!(bytes))
                }
                _ => {
                    use base64::Engine;
                    json!(base64::engine::general_purpose::STANDARD.encode(bytes))
                }
            },
            Cell::Ref(identity) => json!(format!("{}/{}/{}", identity.home, identity.kind, identity.name)),
            Cell::Object(hash) => snapshot
                .rows
                .objects
                .get(hash)
                .and_then(|bytes| flotilla_cbor::from_slice::<Value>(bytes, &Default::default()).ok())
                .unwrap_or_else(|| json!(hash.0.iter().map(|byte| format!("{byte:02x}")).collect::<String>())),
        }
    };
    let mut tables = serde_json::Map::new();
    for (table, rows) in set.tables.iter().zip(&snapshot.rows.tables) {
        let rows: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut object = serde_json::Map::new();
                for (index, value) in row.key.iter().enumerate() {
                    let column = table.key.get(index);
                    object.insert(column.map_or_else(|| format!("key{index}"), |column| column.name.clone()), cell(column, value));
                }
                for (index, value) in row.values.iter().enumerate() {
                    let column = table.columns.get(index);
                    object.insert(column.map_or_else(|| format!("column{index}"), |column| column.name.clone()), cell(column, value));
                }
                Value::Object(object)
            })
            .collect();
        if !rows.is_empty() {
            tables.insert(table.name.clone(), Value::Array(rows));
        }
    }
    let edge_name = |code: &flotilla_schema::Code| {
        set.edges.iter().position(|edge| edge == code).map_or_else(|| code.as_str().to_owned(), |index| set.edge_names[index].clone())
    };
    let label_name = |code: &flotilla_schema::Code| {
        set.tables[0]
            .columns
            .iter()
            .find(|column| column.name == "state")
            .and_then(|column| column.categories.iter().find(|(value, _)| value == code.as_str()).map(|(_, name)| name.clone()))
            .unwrap_or_else(|| code.as_str().to_owned())
    };
    let entries: Vec<Value> = bundle
        .entries
        .iter()
        .map(|entry| {
            let mut value = json!({"seq": entry.home_seq, "cause": entry.cause.name(), "writer": entry.writer, "at": time(entry.at)});
            if let flotilla_schema::Cause::Edge { edge, from, to } = &entry.cause {
                value["edge"] = json!(edge_name(edge));
                value["from"] = json!(label_name(from));
                value["to"] = json!(label_name(to));
            }
            value
        })
        .collect();
    Ok(json!({
        "kind": set.kind,
        "name": snapshot.name,
        "home": snapshot.home,
        "version": snapshot.version,
        "key": bundle_key(&bundle.identity()),
        "tables": tables,
        "entries": entries,
    }))
}
