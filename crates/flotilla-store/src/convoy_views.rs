//! Convoys in the old Convoy shape, read from the engine's launches (#3108).
//!
//! The engine holds each launch of a convoy ([`ConvoyRecords`]); readers not yet converted to
//! records see each launch as a [`Convoy`] read view named by the launch, its phases derived from
//! the launch, its works' Vessels and its seats' sessions, and, at the launch's own home, joined
//! with what that home observes of it. Nothing here writes: the convoy verbs write edges, and the
//! home its notes.
//!
//! Installed as the backend's [`EngineRecords`] for `Convoy`, it serves the old store's typed
//! reads, lists and watches of the kind; its writes are refused.

use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

use futures::{stream::BoxStream, StreamExt};

use flotilla_protocol::{LifecycleReading, NodeId};
use flotilla_resources::{
    convoy::view::VesselReading,
    convoy_records::{ConvoyJoins, ConvoyRecord, ConvoyRecords, WorkFacts},
    engine_records::{EngineObject, EngineRecords},
    lifecycle_reads::LifecycleReads,
    terminal_records::newest_launches,
    vessel_resource_name, Convoy, ReadResourceObject, ResourceError, ResourceObject, ResourceProvenance, Vessel, VesselPhase,
};

use crate::ResourceBackend;

const KIND: &str = "Convoy";

/// Read views of the convoys in one namespace.
#[derive(Clone)]
pub struct ConvoyViews {
    backend: ResourceBackend,
    namespace: String,
    reads: Arc<dyn LifecycleReads>,
}

impl ConvoyViews {
    pub fn new(backend: ResourceBackend, namespace: &str, reads: Arc<dyn LifecycleReads>) -> Self {
        Self { backend, namespace: namespace.to_owned(), reads }
    }

    /// Every launch at every home, as read sources: this home's are local, other homes' replicas
    /// of their home.
    pub async fn sources(&self) -> Result<Vec<ReadResourceObject<Convoy>>, ResourceError> {
        let records = self.blocking(|reads| reads.convoys()?.every()).await?;
        self.compose(records).await
    }

    /// This home's launches.
    pub async fn here(&self) -> Result<Vec<ResourceObject<Convoy>>, ResourceError> {
        let records = self
            .blocking(|reads| {
                let convoys = reads.convoys()?;
                convoys.at(convoys.home())
            })
            .await?;
        Ok(self.compose(records).await?.into_iter().map(|source| source.object).collect())
    }

    /// Launch `name` at this home, as a resolver's `get` reads it.
    pub async fn get(&self, name: &str) -> Result<ResourceObject<Convoy>, ResourceError> {
        self.find(name, None).await?.map(|source| source.object).ok_or_else(|| ResourceError::not_found(name))
    }

    /// Launch `name` held at `home`, or at this home when `None`.
    pub async fn find(&self, name: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<Convoy>>, ResourceError> {
        let (name, home) = (name.to_owned(), home.map(str::to_owned));
        let record = self.blocking(move |reads| reads.convoys()?.get(&name, home.as_deref())).await?;
        Ok(self.compose(record.into_iter().collect()).await?.pop())
    }

    /// The newest launch of `of` in `project` held at `home`, or at this home when `None`.
    pub async fn newest(&self, of: &str, project: &str, home: Option<&str>) -> Result<Option<ReadResourceObject<Convoy>>, ResourceError> {
        let (of, project, home) = (of.to_owned(), project.to_owned(), home.map(str::to_owned));
        let record = self.blocking(move |reads| reads.convoys()?.newest(&of, &project, home.as_deref())).await?;
        Ok(self.compose(record.into_iter().collect()).await?.pop())
    }

    /// `records` as read views.
    pub async fn compose(&self, records: Vec<ConvoyRecord>) -> Result<Vec<ReadResourceObject<Convoy>>, ResourceError> {
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let vessels = self.vessels(&records).await?;
        let live = self.live_sessions(&records).await?;
        let convoys = self.reads.convoys().map_err(ResourceError::other)?;
        let here = convoys.home().to_owned();
        let none = BTreeSet::new();
        Ok(records
            .into_iter()
            .map(|record| {
                let local = record.home == here;
                let notes = local.then(|| convoys.memory().get(&record.name));
                let works: BTreeMap<String, WorkFacts> = record
                    .admission
                    .workflow_snapshot
                    .vessels
                    .iter()
                    .filter_map(|work| {
                        let vessel = vessels.get(&vessel_resource_name(&record.name, &work.name))?;
                        Some((work.name.clone(), vessel.clone()))
                    })
                    .collect();
                let working = record.working(live.get(&record.name).unwrap_or(&none), notes.as_ref());
                let object = record.read_view(&self.namespace, ConvoyJoins { works: &works, working: &working, notes: notes.as_ref() });
                let provenance = if local {
                    ResourceProvenance::Local
                } else {
                    let synced = record.ended.as_ref().map_or(record.admitted_at, |(_, at)| *at);
                    ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: synced }
                };
                ReadResourceObject { object, provenance }
            })
            .collect())
    }

    /// The Vessels of `records`' works, by Vessel name, local or replicated.
    async fn vessels(&self, records: &[ConvoyRecord]) -> Result<BTreeMap<String, WorkFacts>, ResourceError> {
        let vessels = self.backend.including_replicas::<Vessel>(&self.namespace);
        let mut held = BTreeMap::new();
        for record in records {
            for work in &record.admission.workflow_snapshot.vessels {
                let name = vessel_resource_name(&record.name, &work.name);
                match vessels.get(&name).await {
                    Ok(source) => {
                        let object = source.object;
                        let status = object.status.as_ref();
                        let phase = status.map_or(VesselPhase::Pending, |status| status.phase);
                        let facts = WorkFacts {
                            reading: VesselReading::At(phase),
                            ready_at: status.and_then(|status| status.ready_at),
                            started_at: status.and_then(|status| status.started_at),
                            finished_at: None,
                            message: status.and_then(|status| status.message.clone()),
                        };
                        held.insert(name, facts);
                    }
                    // A work with no Vessel has never had one.
                    Err(ResourceError::NotFound { .. }) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(held)
    }

    /// The seats `(work, role)` of each of `records`, by launch, whose newest session launch is
    /// realised.
    async fn live_sessions(&self, records: &[ConvoyRecord]) -> Result<BTreeMap<String, BTreeSet<(String, String)>>, ResourceError> {
        let wanted: Vec<(String, String, String)> = records
            .iter()
            .flat_map(|record| {
                record
                    .admission
                    .workflow_snapshot
                    .vessels
                    .iter()
                    .map(|work| (record.name.clone(), work.name.clone(), vessel_resource_name(&record.name, &work.name)))
            })
            .collect();
        self.blocking(move |reads| {
            let terminals = reads.terminals()?;
            let mut live: BTreeMap<String, BTreeSet<(String, String)>> = BTreeMap::new();
            for (convoy, work, vessel) in wanted {
                for record in newest_launches(terminals.of_vessel(&vessel)?).into_values() {
                    if matches!(record.reading, LifecycleReading::Ready { .. }) {
                        live.entry(convoy.clone()).or_default().insert((work.clone(), record.role));
                    }
                }
            }
            Ok(live)
        })
        .await
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

    fn convoys(&self) -> Result<&dyn ConvoyRecords, String> {
        self.reads.convoys()
    }

    async fn engine_sources(&self) -> Result<Vec<EngineObject>, String> {
        let sources = self.sources().await.map_err(|error| error.to_string())?;
        sources
            .into_iter()
            .map(|source| {
                let home = match &source.provenance {
                    ResourceProvenance::Local => self.convoys()?.home().to_owned(),
                    ResourceProvenance::Replica { origin_root, .. } => origin_root.as_str().to_owned(),
                };
                engine_object(source, home)
            })
            .collect()
    }
}

impl std::fmt::Debug for ConvoyViews {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ConvoyViews").field("namespace", &self.namespace).finish_non_exhaustive()
    }
}

/// `source` as the engine port hands it to the old store: its version a digest of what it shows,
/// so a watcher sees each change of the launch, of what it joins, or of its home's notes.
fn engine_object(source: ReadResourceObject<Convoy>, home: String) -> Result<EngineObject, String> {
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

#[async_trait::async_trait]
impl EngineRecords for ConvoyViews {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
    }

    fn home(&self) -> String {
        self.convoys().map(|convoys| convoys.home().to_owned()).unwrap_or_default()
    }

    async fn get(&self, _kind: &str, name: &str) -> Result<Option<EngineObject>, String> {
        let home = self.home();
        match self.find(name, None).await.map_err(|error| error.to_string())? {
            Some(source) => engine_object(source, home).map(Some),
            // Another home's launch of this name reads where this home holds none.
            None => Ok(self.list(KIND).await?.into_iter().find(|object| object.name == name)),
        }
    }

    /// Each launch, this home's where it holds one of the name, else the lexically first home's.
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
        Err(format!("convoy {name} is admitted on the engine, not written"))
    }

    async fn set_status(&self, _kind: &str, name: &str, _status: serde_json::Value) -> Result<EngineObject, String> {
        Err(format!("convoy {name}'s status is derived from the engine's records; it is not written"))
    }

    async fn delete(&self, _kind: &str, name: &str) -> Result<(), String> {
        Err(format!("convoy {name} ends on the engine; it is not deleted"))
    }

    /// A tick at each launch change at any home, at each change of what this home observes of
    /// one, and at each change of a Vessel or terminal session its works join.
    async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
        let convoys = self.convoys()?;
        let terminals = self.reads.terminals()?;
        let vessels = self.backend.including_replicas::<Vessel>(&self.namespace);
        // A watch error is a tick too: the reader reads everything again.
        let vessel_ticks = vessels.watch().await.map_err(|error| error.to_string())?.map(|_| ()).boxed();
        Ok(futures::stream::select_all([
            ticks(convoys.changes()),
            ticks(convoys.memory().subscribe()),
            ticks(terminals.changes()),
            vessel_ticks,
        ])
        .boxed())
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
