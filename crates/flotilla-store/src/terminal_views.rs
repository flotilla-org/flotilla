//! Terminal sessions in the old TerminalSession shape, read from the engine's launches (#3108).
//!
//! The engine holds each launch ([`TerminalRecords`]); readers not yet converted to records see
//! each session's newest launch per home as a [`TerminalSession`] read view, joined with its holder
//! Vessel's convoy and labels and, at the launch's own home, with what that home observes of it.
//! Nothing here writes: requests and renewals go through [`TerminalRecords`].
//!
//! Installed as the backend's [`EngineRecords`] for `TerminalSession`, it serves the old store's
//! typed reads, lists and watches of the kind; its writes are refused.

use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    sync::Arc,
};

use futures::{stream::BoxStream, StreamExt};

use flotilla_protocol::NodeId;
use flotilla_resources::{
    engine_records::{EngineObject, EngineRecords},
    lifecycle_reads::LifecycleReads,
    terminal_records::{newest_launches, TerminalHolderView, TerminalRecord, TerminalRecords},
    ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance, TerminalSession, Vessel, ACTUATOR_HOST_REF_ANNOTATION,
    ACTUATOR_SOURCE_ROOT_ANNOTATION,
};

use crate::ResourceBackend;

const KIND: &str = "TerminalSession";

/// Read views of the terminal sessions in one namespace.
#[derive(Clone)]
pub struct TerminalViews {
    backend: ResourceBackend,
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
}

impl TerminalViews {
    pub fn new(backend: ResourceBackend, namespace: &str, reads: Arc<dyn LifecycleReads>) -> Self {
        Self { backend, namespace: namespace.to_owned(), reads }
    }

    /// Each session's newest launch at every home, as read sources: this home's are local, other
    /// homes' replicas of their home.
    pub async fn sources(&self) -> Result<Vec<ReadResourceObject<TerminalSession>>, ResourceError> {
        let records = self.blocking(|reads| reads.terminals()?.every()).await?;
        self.compose(newest_launches(records).into_values().collect()).await
    }

    /// This home's sessions' newest launches.
    pub async fn here(&self) -> Result<Vec<ResourceObject<TerminalSession>>, ResourceError> {
        let records = self
            .blocking(|reads| {
                let terminals = reads.terminals()?;
                terminals.at(terminals.home())
            })
            .await?;
        Ok(self.compose(newest_launches(records).into_values().collect()).await?.into_iter().map(|source| source.object).collect())
    }

    /// The newest launch of session `name` at this home, as a resolver's `get` reads it.
    pub async fn get(&self, name: &str) -> Result<ResourceObject<TerminalSession>, ResourceError> {
        self.find(name, None).await?.map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
    }

    /// The newest launch of session (or launch) `name` at `home`, or at this home when `None`.
    pub async fn find(&self, name: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<TerminalSession>>, ResourceError> {
        let (name, home) = (name.to_owned(), home.map(str::to_owned));
        let record = self.blocking(move |reads| reads.terminals()?.get(&name, home.as_deref())).await?;
        Ok(self.compose(record.into_iter().collect()).await?.pop())
    }

    /// The newest launches held for `vessel`, at each home.
    pub async fn of_vessel(&self, vessel: &str) -> Result<Vec<ReadResourceObject<TerminalSession>>, ResourceError> {
        let vessel = vessel.to_owned();
        let records = self.blocking(move |reads| reads.terminals()?.of_vessel(&vessel)).await?;
        self.compose(newest_launches(records).into_values().collect()).await
    }

    /// `records` as read views.
    pub async fn compose(&self, records: Vec<TerminalRecord>) -> Result<Vec<ReadResourceObject<TerminalSession>>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let holders = self.holders(&records).await?;
        let terminals = self.reads.terminals().map_err(ResourceError::other)?;
        let here = terminals.home().to_owned();
        Ok(records
            .into_iter()
            .map(|record| {
                let holder = record.vessel.as_ref().and_then(|vessel| holders.get(vessel));
                let local = record.home == here;
                let observation = local.then(|| terminals.observations().get(&record.name)).flatten();
                let object = record.read_view(&self.namespace, holder, observation.as_ref());
                let provenance = if local {
                    ResourceProvenance::Local
                } else {
                    ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: record.since }
                };
                ReadResourceObject { object, provenance }
            })
            .collect())
    }

    /// The holder Vessels `records` name, local or replicated.
    async fn holders(&self, records: &[TerminalRecord]) -> Result<BTreeMap<String, TerminalHolderView>, ResourceError> {
        let vessels = self.backend.including_replicas::<Vessel>(&self.namespace);
        let mut holders = BTreeMap::new();
        for vessel in records.iter().filter_map(|record| record.vessel.as_deref()) {
            if holders.contains_key(vessel) {
                continue;
            }
            match vessels.get(vessel).await {
                Ok(source) => {
                    let object = source.object;
                    let annotations = [ACTUATOR_HOST_REF_ANNOTATION, ACTUATOR_SOURCE_ROOT_ANNOTATION]
                        .into_iter()
                        .filter_map(|key| object.metadata.annotations.get(key).map(|value| (key.to_owned(), value.clone())))
                        .collect();
                    holders.insert(
                        vessel.to_owned(),
                        TerminalHolderView { convoy: object.spec.convoy_ref, vessel_name: object.spec.vessel_name, annotations },
                    );
                }
                // A launch outliving its vessel reads without its convoy.
                Err(ResourceError::NotFound { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(holders)
    }

    async fn blocking<T: Send + 'static>(
        &self,
        read: impl FnOnce(&dyn LifecycleReads) -> Result<T, String> + Send + 'static,
    ) -> Result<T, ResourceError> {
        let reads = Arc::clone(&self.reads);
        tokio::task::spawn_blocking(move || read(reads.as_ref()))
            .await
            .map_err(|error| ResourceError::other(error.to_string()))?
            .map_err(ResourceError::other)
    }
}

impl std::fmt::Debug for TerminalViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("TerminalViews").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

/// `source` as the engine port hands it to the old store: its version a digest of what it shows,
/// so a watcher sees each change of the launch or of what its home observes of it.
fn engine_object(source: ReadResourceObject<TerminalSession>, home: String) -> Result<EngineObject, String> {
    let object = source.object;
    let spec = serde_json::to_value(&object.spec).map_err(|error| error.to_string())?;
    let status = object.status.as_ref().map(serde_json::to_value).transpose().map_err(|error| error.to_string())?;
    let mut digest = std::hash::DefaultHasher::new();
    (&object.metadata.resource_version, spec.to_string(), status.as_ref().map(ToString::to_string), &object.metadata.labels)
        .hash(&mut digest);
    Ok(EngineObject {
        name: object.metadata.name,
        home,
        version: digest.finish(),
        labels: object.metadata.labels,
        annotations: object.metadata.annotations,
        spec,
        status,
    })
}

impl TerminalViews {
    fn terminals(&self) -> Result<&dyn TerminalRecords, String> {
        self.reads.terminals()
    }

    async fn engine_sources(&self) -> Result<Vec<EngineObject>, String> {
        let sources = self.sources().await.map_err(|error| error.to_string())?;
        sources
            .into_iter()
            .map(|source| {
                let home = match &source.provenance {
                    ResourceProvenance::Local => self.terminals()?.home().to_owned(),
                    ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
                };
                engine_object(source, home)
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl EngineRecords for TerminalViews {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
    }

    fn home(&self) -> String {
        self.terminals().map(|terminals| terminals.home().to_owned()).unwrap_or_default()
    }

    async fn get(&self, _kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        Ok(self.list(KIND).await?.into_iter().find(|object| object.name == name))
    }

    /// Each session's newest launch, this home's where it holds one, else the lexically first
    /// home's.
    async fn list(&self, _kind: &str) -> Result<Vec<EngineObject>, String> {
        let home = self.home();
        let mut chosen = BTreeMap::<String, EngineObject>::new();
        for object in self.engine_sources().await? {
            let keep = chosen.get(&object.name).is_some_and(|held| held.home == home || (object.home != home && held.home <= object.home));
            if !keep {
                chosen.insert(object.name.clone(), object);
            }
        }
        Ok(chosen.into_values().collect())
    }

    async fn sources(&self, _kind: &str) -> Result<Vec<EngineObject>, String> {
        self.engine_sources().await
    }

    async fn apply(
        &self,
        _kind: &str,
        name: &str,
        _labels: &BTreeMap<String, String>,
        _annotations: &BTreeMap<String, String>,
        _spec: serde_json::Value,
    ) -> Result<EngineObject, String> {
        Err(format!("terminal session {name} is requested through the engine's terminal sessions, not written"))
    }

    async fn set_status(&self, _kind: &str, name: &str, _status: serde_json::Value) -> Result<EngineObject, String> {
        Err(format!("terminal session {name}'s status is the engine's; it is not written"))
    }

    async fn delete(&self, _kind: &str, name: &str) -> Result<(), String> {
        Err(format!("terminal session {name} ends when its holder does; it is not deleted"))
    }

    /// A tick at each launch change at any home, and at each change of what this home observes.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let terminals = self.terminals()?;
        Ok(futures::stream::select(ticks(terminals.changes()), ticks(terminals.observations().subscribe())).boxed())
    }
}

/// A tick per message on `changes`, and one when it lagged: a reader reads everything again.
fn ticks<T: Clone + Send + 'static>(changes: tokio::sync::broadcast::Receiver<T>) -> BoxStream<'static, ()> {
    futures::stream::unfold(changes, |mut changes| async move {
        match changes.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some(((), changes)),
            Err(tokio::sync::broadcast::error::RecvError::Closed) => None,
        }
    })
    .boxed()
}
