use crate::store::transcode_request;
use crate::StoreExt;
use flotilla_resources::lifecycle_reads::LifecycleReads;
use std::{
    collections::{BTreeMap, HashSet},
    marker::PhantomData,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use flotilla_resources::{ReadResourceList, ReadResourceObject, ReadWatchEvent, ReplicaCursor, ResourceProvenance};
use flotilla_resources::{ResourceList, WatchStart, WatchStream};
use futures::{stream, StreamExt};

use crate::{
    definition::DefinitionResolver,
    error::ResourceError,
    replica::StoredReplicaEventKind,
    resource::{InputMeta, Resource, ResourceObject},
    retention::ResourceStoreDiagnostics,
};

macro_rules! dispatch_backend {
    ($self:expr, $method:ident $(, $args:expr)*) => {
        $self.backend.$method::<T>(&$self.namespace $(, $args)*).await
    };
}

/// The engine ports installed behind `[primitives]`, by the resource kinds they serve, and the
/// namespace their records are served in: shared by every handle cloned or derived from one
/// backend (#3824 layers 2b-ii and 3).
type EngineSlot = std::sync::Arc<std::sync::RwLock<EngineSlots>>;

#[derive(Debug, Default)]
struct EngineSlots {
    ports: std::collections::BTreeMap<&'static str, crate::engine_port::Installed>,
    /// How many ports have been installed: each installation's mark, so a feed started for a
    /// replaced port is not kept.
    installs: u64,
    /// The engine keeps no namespaces: its records read in whichever namespace is asked, and carry
    /// this daemon's one, as a scan for the namespaces holding a kind it serves answers.
    namespace: Option<String>,
    /// Whether this backend is the engine's home: a kind the engine holds is then refused until
    /// its port is installed, never served from the old store.
    home: bool,
    /// Each kind's shared listing, an `EngineFeed` of its resource kind, while any watcher holds it.
    feeds: std::collections::BTreeMap<&'static str, std::sync::Weak<dyn std::any::Any + Send + Sync>>,
}

#[derive(Default)]
struct AttentionSlot(std::sync::RwLock<Option<std::sync::Arc<dyn flotilla_resources::attentions::Attentions>>>);
impl std::fmt::Debug for AttentionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AttentionSlot")
    }
}

/// Cloneable handle chosen by the composition root.
#[derive(Debug, Clone)]
pub struct ResourceBackend {
    store: std::sync::Arc<dyn crate::Store>,
    engines: EngineSlot,
    attentions: std::sync::Arc<AttentionSlot>,
}
/// A [`ResourceBackend`] that does not keep its installed ports alive: a port the backend holds
/// keeps this, not the backend, so dropping the backend drops the port.
#[derive(Debug, Clone)]
pub struct WeakResourceBackend {
    store: std::sync::Arc<dyn crate::Store>,
    engines: std::sync::Weak<std::sync::RwLock<EngineSlots>>,
    attentions: std::sync::Weak<AttentionSlot>,
}
impl WeakResourceBackend {
    /// The backend, while any handle to it is alive.
    pub fn upgrade(&self) -> Option<ResourceBackend> {
        Some(ResourceBackend {
            store: std::sync::Arc::clone(&self.store),
            engines: self.engines.upgrade()?,
            attentions: self.attentions.upgrade()?,
        })
    }
}
impl ResourceBackend {
    pub fn new(store: impl Into<Self>) -> Self {
        store.into()
    }
    pub fn downgrade(&self) -> WeakResourceBackend {
        WeakResourceBackend {
            store: std::sync::Arc::clone(&self.store),
            engines: std::sync::Arc::downgrade(&self.engines),
            attentions: std::sync::Arc::downgrade(&self.attentions),
        }
    }
    pub fn from_arc(store: std::sync::Arc<dyn crate::Store>) -> Self {
        Self { store, engines: EngineSlot::default(), attentions: Default::default() }
    }
    /// Serves the kinds `port` serves from it, the engine's: every handle cloned or derived from
    /// this backend reads and writes them there. A daemon runtime restarted over the same backend
    /// installs its reopened ports in place of the last.
    pub fn install_engine_records<P: crate::EnginePort + ?Sized>(&self, port: std::sync::Arc<P>) {
        port.install(self);
    }

    /// Serves resource kind `R` from `records`, in place of any port installed for it before.
    pub fn install_engine_kind<R: Resource>(&self, records: std::sync::Arc<dyn flotilla_resources::engine_records::KindRecords<R>>) {
        let mut engines = self.engines.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        engines.installs += 1;
        let mark = engines.installs;
        engines.ports.insert(R::API_PATHS.kind, crate::engine_port::Installed::new(records, mark));
        // Watchers of the port replaced keep its feed; new ones list the new port.
        engines.feeds.remove(R::API_PATHS.kind);
    }
    /// The namespace the engine's records are served in, the daemon's provisioning namespace: a
    /// namespace scan of a kind the engine serves answers with it.
    ///
    /// The engine has no namespaces, and the fleet's data holds only `flotilla`, so this is that
    /// one name. Do not serve or read a second namespace: the namespace parameter of the read and
    /// write APIs is to be deleted (after the R2 field removal), not given a meaning.
    pub fn serve_engine_in(&self, namespace: &str) {
        self.engines.write().unwrap_or_else(std::sync::PoisonError::into_inner).namespace = Some(namespace.to_owned());
    }
    /// The one namespace the engine serves `kind` in, when the engine serves it: its records are
    /// in no other, whichever namespace they are read through.
    pub fn engine_namespace(&self, kind: &str) -> Option<String> {
        let engines = self.engines.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        engines.ports.contains_key(kind).then(|| engines.namespace.clone()).flatten()
    }
    /// Whether an engine port serves `kind`: its records reach other homes by log following,
    /// never through the old store's replication.
    pub fn engine_serves(&self, kind: &str) -> bool {
        self.engine_facts(kind).is_some()
    }

    /// Marks this backend as the engine's home: until its port is installed, every read and write
    /// of a kind the engine holds is refused. Other backends (observed, HTTP, tests) serve every
    /// kind with no port installed from their store.
    pub fn compose_as_engine_home(&self) {
        self.engines.write().unwrap_or_else(std::sync::PoisonError::into_inner).home = true;
    }

    /// Whether this backend refuses `kind` for want of its port: the engine's home before the
    /// port of a kind the engine holds is installed.
    fn uncomposed(engines: &EngineSlots, kind: &str) -> bool {
        engines.home && crate::REGISTERED_RESOURCE_KINDS.iter().any(|registered| registered.kind == kind)
    }

    /// The engine port serving `R`, when one is installed, with its installation's mark; on the
    /// engine's home, a refusing port for a kind the engine holds before its port is installed.
    fn engine_port<R: Resource>(&self) -> Option<(std::sync::Arc<dyn flotilla_resources::engine_records::KindRecords<R>>, u64)> {
        let engines = self.engines.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        match engines.ports.get(R::API_PATHS.kind) {
            Some(installed) => installed.typed::<R>(),
            None => Self::uncomposed(&engines, R::API_PATHS.kind)
                .then(|| (std::sync::Arc::new(crate::uncomposed_records::Uncomposed::<R>::default()) as _, 0)),
        }
    }

    /// The engine port serving `R`; see [`Self::engine_port`].
    pub(crate) fn engine_records<R: Resource>(&self) -> Option<std::sync::Arc<dyn flotilla_resources::engine_records::KindRecords<R>>> {
        self.engine_port::<R>().map(|(records, _)| records)
    }

    /// What the engine port serving `kind` says of it whatever its type; on the engine's home, a
    /// refusing port's for a kind the engine holds before its port is installed.
    pub(crate) fn engine_facts(&self, kind: &str) -> Option<std::sync::Arc<dyn crate::engine_port::KindFacts>> {
        let engines = self.engines.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        match engines.ports.get(kind) {
            Some(installed) => Some(installed.facts()),
            None => Self::uncomposed(&engines, kind).then(|| crate::engine_port::uncomposed_facts(kind)),
        }
    }

    /// The stored kinds whose log entries can change `kind`'s records as its port reads them;
    /// `None` unless the engine serves `kind`.
    pub fn engine_codes(&self, kind: &str) -> Option<Vec<flotilla_resources::engine_records::KindCode>> {
        self.engine_facts(kind).map(|port| port.codes())
    }

    /// How many times `kind`'s engine records changed outside the log since this home started;
    /// `None` unless the engine serves `kind` with some of its state held only in memory.
    pub fn engine_unlogged_changes(&self, kind: &str) -> Option<u64> {
        self.engine_facts(kind).and_then(|port| port.unlogged_changes())
    }

    /// The shared listing of `R`'s engine records, started for the first watcher; `None` unless
    /// the engine serves `R`.
    pub(crate) async fn engine_feed<R: Resource>(
        &self,
    ) -> Option<Result<std::sync::Arc<crate::engine_feed::EngineFeed<R>>, ResourceError>> {
        let (records, mark) = self.engine_port::<R>()?;
        if let Some(feed) = self.held_engine_feed::<R>() {
            return Some(Ok(feed));
        }
        let started = match crate::engine_feed::EngineFeed::start(records).await {
            Ok(started) => std::sync::Arc::new(started),
            Err(error) => return Some(Err(ResourceError::other(error))),
        };
        let mut engines = self.engines.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Another watcher may have started one meanwhile: the first kept wins, for its port.
        if engines.ports.get(R::API_PATHS.kind).is_none_or(|installed| installed.mark() != mark) {
            return Some(Ok(started));
        }
        if let Some(feed) = Self::feed_of::<R>(&engines) {
            return Some(Ok(feed));
        }
        let held: std::sync::Arc<dyn std::any::Any + Send + Sync> = started.clone();
        engines.feeds.insert(R::API_PATHS.kind, std::sync::Arc::downgrade(&held));
        Some(Ok(started))
    }

    /// The shared listing of `R`'s engine records while any watcher holds it.
    pub(crate) fn held_engine_feed<R: Resource>(&self) -> Option<std::sync::Arc<crate::engine_feed::EngineFeed<R>>> {
        Self::feed_of::<R>(&self.engines.read().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn feed_of<R: Resource>(engines: &EngineSlots) -> Option<std::sync::Arc<crate::engine_feed::EngineFeed<R>>> {
        engines.feeds.get(R::API_PATHS.kind)?.upgrade()?.downcast::<crate::engine_feed::EngineFeed<R>>().ok()
    }

    /// Installs the engine's attentions behind `[primitives]`. A daemon runtime restarted over the
    /// same backend installs its reopened engine in place of the last.
    pub fn install_attentions(&self, attentions: std::sync::Arc<dyn flotilla_resources::attentions::Attentions>) {
        *self.attentions.0.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(attentions);
    }

    /// The engine's attentions; before they are installed, every attention call is refused.
    pub fn attentions(&self) -> std::sync::Arc<dyn flotilla_resources::attentions::Attentions> {
        match self.attentions.0.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
            Some(engine) => std::sync::Arc::clone(engine),
            None => std::sync::Arc::new(crate::attentions::Uncomposed),
        }
    }

    pub fn store(&self) -> &dyn crate::Store {
        &*self.store
    }
    pub fn with_read_observer(self, observer: std::sync::Arc<dyn crate::ReadObserver>) -> Self {
        Self { store: self.store.with_read_observer(observer), engines: self.engines.clone(), attentions: self.attentions.clone() }
    }
    pub fn read_observer(&self) -> Option<&dyn crate::ReadObserver> {
        self.store.read_observer()
    }
    pub(crate) async fn hierarchy_admission(&self, kind: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if !crate::project_hierarchy::is_hierarchy_kind(kind) {
            return None;
        }
        Some(self.store.hierarchy_admission()?.lock_owned().await)
    }
    pub fn with_local_root(self, root: NodeId) -> Self {
        Self { store: self.store.with_local_root(root), engines: self.engines.clone(), attentions: self.attentions.clone() }
    }
    pub fn local_root(&self) -> Result<NodeId, ResourceError> {
        self.store.local_root()
    }
    pub fn serves_read_views(&self) -> bool {
        self.store.serves_read_views()
    }
    pub fn using<T: Resource>(&self, namespace: &str) -> TypedResolver<T> {
        TypedResolver { backend: self.clone(), namespace: namespace.to_string(), lifecycle_status_reads: None, _marker: PhantomData }
    }

    /// Namespaces containing locally-authored objects of this kind.
    pub async fn local_namespaces<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        if let Some(facts) = self.engine_facts(T::API_PATHS.kind) {
            return self.engine_kind_namespaces(T::API_PATHS.kind, &*facts, true).await;
        }
        self.local_namespaces_typed::<T>().await
    }

    /// Namespaces containing a local or replicated source of this kind.
    pub async fn stored_namespaces<T: Resource>(&self) -> Result<Vec<String>, ResourceError> {
        if let Some(facts) = self.engine_facts(T::API_PATHS.kind) {
            return self.engine_kind_namespaces(T::API_PATHS.kind, &*facts, false).await;
        }
        self.stored_namespaces_typed::<T>().await
    }

    /// For a kind the engine serves: its namespace when any home holds a record of the kind (this
    /// home, when `local`), else none.
    async fn engine_kind_namespaces(
        &self,
        kind: &str,
        facts: &dyn crate::engine_port::KindFacts,
        local: bool,
    ) -> Result<Vec<String>, ResourceError> {
        let namespace = self.engines.read().unwrap_or_else(std::sync::PoisonError::into_inner).namespace.clone();
        let namespace = namespace.ok_or_else(|| ResourceError::other(format!("{kind} is served by the engine in no namespace")))?;
        let homes = facts.homes().await.map_err(ResourceError::other)?;
        let held = if local { homes.contains(&facts.home()) } else { !homes.is_empty() };
        Ok(if held { vec![namespace] } else { Vec::new() })
    }

    /// Every namespace with an authored or replicated record, by kind plural: a kind the engine
    /// serves answers from the engine, every other from the store. A kind the engine does not
    /// read as resources is in no namespace; a kind whose read fails names itself in the error.
    pub async fn stored_namespace_inventory(&self) -> Result<std::collections::BTreeMap<String, Vec<String>>, ResourceError> {
        // The engine's home answers every kind from the engine; its store holds none of them.
        let home = self.engines.read().unwrap_or_else(std::sync::PoisonError::into_inner).home;
        let mut inventory = if home { std::collections::BTreeMap::new() } else { self.store.namespace_inventory().await? };
        for kind in crate::declared_resource_kinds() {
            if let Some(facts) = self.engine_facts(kind.paths.kind) {
                let namespaces = if facts.reads() {
                    self.engine_kind_namespaces(kind.paths.kind, &*facts, false)
                        .await
                        .map_err(|error| ResourceError::other(format!("namespaces of {}: {error}", kind.paths.kind)))?
                } else {
                    Vec::new()
                };
                inventory.insert(kind.paths.plural.to_string(), namespaces);
            }
        }
        Ok(inventory)
    }

    pub fn definitions<T: Resource>(&self, namespace: &str) -> DefinitionResolver<T> {
        DefinitionResolver::new(self.clone(), namespace.to_string())
    }

    /// A read-only union of locally-authored objects and durable replicas.
    ///
    /// This deliberately returns a different resolver type with no mutation
    /// methods, so controllers cannot accidentally reconcile replica rows.
    pub fn including_replicas<T: Resource>(&self, namespace: &str) -> ReplicaReadResolver<T> {
        ReplicaReadResolver { backend: self.clone(), namespace: namespace.to_string(), suppress_self_origin: true, _marker: PhantomData }
    }

    pub fn replica_writer<T: Resource>(&self, origin_root: NodeId, namespace: &str) -> ReplicaWriter<T> {
        ReplicaWriter { backend: self.clone(), origin_root, namespace: namespace.to_string(), _marker: PhantomData }
    }

    pub async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        self.store.diagnostics().await
    }
    pub(crate) async fn delete_decode_quarantine<T: Resource>(&self, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        self.store.delete_decode_quarantine_typed::<T>(namespace, name).await
    }
}

#[derive(Debug)]
pub struct ReplicaReadResolver<T: Resource> {
    backend: ResourceBackend,
    namespace: String,
    suppress_self_origin: bool,
    _marker: PhantomData<T>,
}

impl<T: Resource> std::clone::Clone for ReplicaReadResolver<T> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            namespace: self.namespace.clone(),
            suppress_self_origin: self.suppress_self_origin,
            _marker: PhantomData,
        }
    }
}

impl<T: Resource> ReplicaReadResolver<T> {
    /// Fault-injection seam for convergence tests. Production callers must
    /// leave self-origin suppression enabled.
    #[doc(hidden)]
    pub fn with_self_origin_suppression(mut self, suppress: bool) -> Self {
        self.suppress_self_origin = suppress;
        self
    }

    pub async fn get(&self, name: &str) -> Result<ReadResourceObject<T>, ResourceError> {
        if !T::RANKED_REPLICA_READS {
            return self.get_local_first(name).await;
        }
        // Demanded placeholders must not hide observed replicas. Read each copy
        // once and share selection with leaf evaluation and observer ownership.
        flotilla_resources::pick_replica_read(self.get_all(name).await?.items, |item| {
            (&item.object, matches!(item.provenance, ResourceProvenance::Local))
        })
        .ok_or_else(|| ResourceError::not_found(name))
    }

    async fn get_local_first(&self, name: &str) -> Result<ReadResourceObject<T>, ResourceError> {
        if let Some(reads) = self.engine_copies(name).await {
            // This home's copy first, then the lexically first home's, as the port resolves.
            let mut reads = reads?;
            let local = reads.iter().position(|read| matches!(read.provenance, ResourceProvenance::Local));
            return match local {
                Some(index) => Ok(reads.swap_remove(index)),
                None => reads.into_iter().next().ok_or_else(|| ResourceError::not_found(name)),
            };
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::None {
            return self
                .backend
                .using::<T>(&self.namespace)
                .get(name)
                .await
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local });
        }
        ensure_replication_enabled::<T>()?;
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.get_including_replicas_typed::<T>(&self.namespace, name).await;
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            let definitions = self.backend.definitions::<T>(&self.namespace);
            if let Some(read) = definitions.engine_resolved(name).await {
                return read;
            }
            return definitions.get(name).await.map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local });
        }
        match self.backend.using::<T>(&self.namespace).get(name).await {
            Ok(object) => Ok(ReadResourceObject { object, provenance: ResourceProvenance::Local }),
            Err(ResourceError::NotFound { .. }) => {
                let replicas = { self.backend.get_replicas_typed::<T>(&self.namespace, name).await? };
                replicas.into_iter().next().ok_or_else(|| ResourceError::not_found(name))
            }
            Err(error) => Err(error),
        }
    }

    /// Read every visible provenance for one name without listing the namespace.
    pub async fn get_all(&self, name: &str) -> Result<ReadResourceList<T>, ResourceError> {
        if let Some(reads) = self.engine_copies(name).await {
            return reads.map(|items| ReadResourceList { items });
        }
        // These classes have at most one visible copy for a name.
        if T::REPLICATION_CLASS == crate::ReplicationClass::None || T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return match self.get_local_first(name).await {
                Ok(item) => Ok(ReadResourceList { items: vec![item] }),
                Err(ResourceError::NotFound { .. }) => Ok(ReadResourceList { items: Vec::new() }),
                Err(error) => Err(error),
            };
        }
        ensure_replication_enabled::<T>()?;
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.get_all_including_replicas_typed::<T>(&self.namespace, name).await;
        }
        let mut items = match self.backend.using::<T>(&self.namespace).get(name).await {
            Ok(object) => vec![ReadResourceObject { object, provenance: ResourceProvenance::Local }],
            Err(ResourceError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(error),
        };
        let replicas = { self.backend.get_replicas_typed::<T>(&self.namespace, name).await? };
        items.extend(replicas);
        let listed = ReadResourceList { items };
        if self.suppress_self_origin {
            self.suppress_shadowed_self_origin_sources(listed)
        } else {
            Ok(listed)
        }
    }

    pub async fn list(&self) -> Result<ReadResourceList<T>, ResourceError> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::None {
            let items = self
                .backend
                .using::<T>(&self.namespace)
                .list()
                .await?
                .items
                .into_iter()
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local })
                .collect();
            return Ok(ReadResourceList { items });
        }
        ensure_replication_enabled::<T>()?;
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.list_including_replicas_typed::<T>(&self.namespace).await;
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            let definitions = self.backend.definitions::<T>(&self.namespace);
            if let Some(reads) = definitions.engine_resolved_list().await {
                return reads.map(|items| ReadResourceList { items });
            }
            let items = definitions
                .list()
                .await?
                .into_iter()
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local })
                .collect();
            return Ok(ReadResourceList { items });
        }
        if !self.suppress_self_origin {
            return self.list_sources().await;
        }
        let listed = self.list_sources().await?;
        self.suppress_shadowed_self_origin_sources(listed)
    }

    fn suppress_shadowed_self_origin_sources(&self, mut listed: ReadResourceList<T>) -> Result<ReadResourceList<T>, ResourceError> {
        let local_root = self.backend.local_root()?;
        let local_names = listed
            .items
            .iter()
            .filter(|item| matches!(item.provenance, ResourceProvenance::Local))
            .map(|item| item.object.metadata.name.clone())
            .collect::<HashSet<_>>();
        listed.items.retain(|item| {
            !matches!(
                &item.provenance,
                ResourceProvenance::Replica { origin_root, .. }
                    if origin_root == &local_root && local_names.contains(&item.object.metadata.name)
            )
        });
        Ok(listed)
    }

    pub async fn list_matching_labels(&self, required: &BTreeMap<String, String>) -> Result<ReadResourceList<T>, ResourceError> {
        if required.is_empty() {
            return self.list().await;
        }
        if self.backend.serves_read_views() && self.suppress_self_origin {
            return self.backend.list_including_replicas_typed_matching_labels::<T>(&self.namespace, required).await;
        }
        self.backend.query_declared::<T>(&self.namespace, &crate::StoreQuery::labels(required, true, self.suppress_self_origin)).await
    }

    /// Every home's engine copy of `name`, for a kind the engine serves other than a definition
    /// kind (definitions resolve through their resolver).
    async fn engine_copies(&self, name: &str) -> Option<Result<Vec<ReadResourceObject<T>>, ResourceError>> {
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return None;
        }
        self.backend.definitions::<T>(&self.namespace).engine_copies(name).await
    }

    pub(crate) async fn list_sources(&self) -> Result<ReadResourceList<T>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if let Some(reads) = self.backend.definitions::<T>(&self.namespace).engine_reads().await {
            return reads.map(|items| ReadResourceList { items });
        }
        if self.backend.serves_read_views() {
            return Err(ResourceError::invalid("HTTP replica-source lists use the resource API"));
        }
        let local = self.backend.using::<T>(&self.namespace).list().await?;
        let mut items =
            local.items.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }).collect::<Vec<_>>();
        let replicas = { self.backend.list_replicas_typed::<T>(&self.namespace).await? };
        items.extend(replicas);
        items.sort_by(|left, right| {
            left.object.metadata.name.cmp(&right.object.metadata.name).then_with(|| match (&left.provenance, &right.provenance) {
                (ResourceProvenance::Local, ResourceProvenance::Local) => std::cmp::Ordering::Equal,
                (ResourceProvenance::Local, ResourceProvenance::Replica { .. }) => std::cmp::Ordering::Less,
                (ResourceProvenance::Replica { .. }, ResourceProvenance::Local) => std::cmp::Ordering::Greater,
                (
                    ResourceProvenance::Replica { origin_root: left_origin, .. },
                    ResourceProvenance::Replica { origin_root: right_origin, .. },
                ) => left_origin.cmp(right_origin),
            })
        });
        Ok(ReadResourceList { items })
    }

    /// Lists every local and replicated source object without collapsing
    /// same-name replicas behind the local object.
    ///
    /// Consumers that combine host-local status need the individual sources;
    /// ordinary resource reads should continue to use [`Self::list`].
    pub async fn list_replica_sources(&self) -> Result<ReadResourceList<T>, ResourceError> {
        let listed = self.list_sources().await?;
        if self.suppress_self_origin {
            self.suppress_shadowed_self_origin_sources(listed)
        } else {
            Ok(listed)
        }
    }

    pub async fn watch(&self) -> Result<futures::stream::BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        if let Some(watch) = self.backend.definitions::<T>(&self.namespace).engine_watch().await {
            // The engine's records stream their changes; the current ones are read by `list`.
            return watch.map(|(_, stream)| stream);
        }
        if self.backend.serves_read_views() {
            let backend = &self.backend;
            return backend.watch_including_replicas_typed::<T>(&self.namespace).await;
        }
        let replicas = { self.backend.watch_replicas_typed::<T>(&self.namespace).await? };
        let local = self.backend.using::<T>(&self.namespace).watch(WatchStart::Now).await?.map(|event| event.map(ReadWatchEvent::local));
        let raw = stream::select(local, replicas);
        if T::REPLICATION_CLASS != crate::ReplicationClass::Definitions && self.suppress_self_origin {
            let backend = self.backend.clone();
            let namespace = self.namespace.clone();
            let local_root = backend.local_root()?;
            return Ok(raw
                .filter_map(move |event| {
                    let backend = backend.clone();
                    let namespace = namespace.clone();
                    let local_root = local_root.clone();
                    async move {
                        crate::watch::complete_watch_lookup(async move {
                            let event = match event {
                                Ok(event) => event,
                                Err(error) => return Some(Err(error)),
                            };
                            let self_origin_name = match &event {
                                ReadWatchEvent::Added(item) | ReadWatchEvent::Modified(item) | ReadWatchEvent::Deleted(item) => matches!(
                                    &item.provenance,
                                    ResourceProvenance::Replica { origin_root, .. } if origin_root == &local_root
                                )
                                .then_some(item.object.metadata.name.as_str()),
                                ReadWatchEvent::DeletedByName { tombstone, provenance } => matches!(
                                    provenance,
                                    ResourceProvenance::Replica { origin_root, .. } if origin_root == &local_root
                                )
                                .then_some(tombstone.name.as_str()),
                            };
                            let Some(name) = self_origin_name else {
                                return Some(Ok(event));
                            };
                            match backend.using::<T>(&namespace).get(name).await {
                                Ok(_) => None,
                                Err(ResourceError::NotFound { .. }) => Some(Ok(event)),
                                Err(error) => Some(Err(error)),
                            }
                        })
                        .await
                        .unwrap_or_else(|error| Some(Err(error)))
                    }
                })
                .boxed());
        }
        let backend = self.backend.clone();
        let namespace = self.namespace.clone();
        Ok(raw
            .then(move |event| {
                let backend = backend.clone();
                let namespace = namespace.clone();
                async move {
                    crate::watch::complete_watch_lookup(async move {
                        let event = event?;
                        if let ReadWatchEvent::DeletedByName { tombstone, .. } = event {
                            return match backend.definitions::<T>(&namespace).get(&tombstone.name).await {
                                Ok(object) => {
                                    Ok(ReadWatchEvent::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local }))
                                }
                                Err(ResourceError::NotFound { .. }) => {
                                    Ok(ReadWatchEvent::DeletedByName { tombstone, provenance: ResourceProvenance::Local })
                                }
                                Err(error) => Err(error),
                            };
                        }
                        let fallback = match event {
                            ReadWatchEvent::Added(object) | ReadWatchEvent::Modified(object) | ReadWatchEvent::Deleted(object) => object,
                            ReadWatchEvent::DeletedByName { .. } => unreachable!("handled above"),
                        };
                        match backend.definitions::<T>(&namespace).get(&fallback.object.metadata.name).await {
                            Ok(object) => {
                                Ok(ReadWatchEvent::Modified(ReadResourceObject { object, provenance: ResourceProvenance::Local }))
                            }
                            // Definitions are one merged logical member, whose initial
                            // list uses Local provenance even when entirely replicated.
                            Err(ResourceError::NotFound { .. }) => Ok(ReadWatchEvent::Deleted(ReadResourceObject {
                                object: fallback.object,
                                provenance: ResourceProvenance::Local,
                            })),
                            Err(error) => Err(error),
                        }
                    })
                    .await?
                }
            })
            .boxed())
    }
}

#[derive(Debug, Clone, bon::Builder)]
#[builder(builder_type(vis = "pub(in crate::backend)"))]
pub struct ReplicaWriter<T: Resource> {
    backend: ResourceBackend,
    origin_root: NodeId,
    namespace: String,
    #[builder(skip)]
    _marker: PhantomData<T>,
}

impl<T: Resource> ReplicaWriter<T> {
    pub async fn replace(&self, listed: &ResourceList<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        self.backend.replace_replicas_typed(&self.origin_root, &self.namespace, listed, synced_at, None).await
    }

    /// Replace only one complete authoritative hash bucket; never move the log cursor.
    pub async fn replace_bucket(&self, bucket: u8, listed: &ResourceList<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        if listed
            .items
            .iter()
            .any(|object| crate::digest_bucket(&object.metadata.name) != bucket || object.metadata.namespace != self.namespace)
        {
            return Err(ResourceError::invalid("snapshot contains keys outside its partition"));
        }
        self.backend.replace_replicas_typed(&self.origin_root, &self.namespace, listed, synced_at, Some(bucket)).await
    }

    pub async fn apply(&self, event: crate::WatchEvent<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        let (kind, object) = match event {
            crate::WatchEvent::Added(object) => (StoredReplicaEventKind::Added, object),
            crate::WatchEvent::Modified(object) => (StoredReplicaEventKind::Modified, object),
            crate::WatchEvent::Deleted(object) => (StoredReplicaEventKind::Deleted, object),
            crate::WatchEvent::DeletedByName(tombstone) => {
                return self.backend.apply_replica_tombstone_typed::<T>(&self.origin_root, &self.namespace, &tombstone, synced_at).await;
            }
        };
        self.backend.apply_replica_typed(&self.origin_root, &self.namespace, kind, &object, synced_at).await
    }

    pub async fn apply_batch(&self, events: &[crate::WatchEvent<T>], synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        for batch in events.chunks(crate::replica::REPLICA_BATCH_SIZE) {
            let serialized = crate::transcode::<_, Vec<crate::WatchEvent<crate::SerializedResource>>>(batch)?;
            let stamped = serialized.into_iter().map(|event| (event, synced_at)).collect::<Vec<_>>();
            self.backend.apply_replica_batch_raw(crate::ResourceKind::of::<T>(), &self.origin_root, &self.namespace, &stamped).await?;
        }
        Ok(())
    }

    /// Relay batches retain each event's originating sync time for conflict resolution.
    pub async fn apply_stamped_batch(&self, events: &[(crate::WatchEvent<T>, DateTime<Utc>)]) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        for batch in events.chunks(crate::replica::REPLICA_BATCH_SIZE) {
            let serialized = crate::transcode::<_, Vec<(crate::WatchEvent<crate::SerializedResource>, DateTime<Utc>)>>(batch)?;
            self.backend.apply_replica_batch_raw(crate::ResourceKind::of::<T>(), &self.origin_root, &self.namespace, &serialized).await?;
        }
        Ok(())
    }

    /// Prove the entire contiguous batch before applying any of it. A crash
    /// before cursor advancement replays idempotent events rather than skipping them.
    pub async fn apply_direct_batch(&self, events: &[crate::WatchEvent<T>], synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        if events.is_empty() {
            return Ok(());
        }
        let previous = self.cursor().await?.ok_or_else(|| ResourceError::invalid("replica prefix missing; snapshot required"))?;
        let mut version = previous.resource_version.parse::<u64>().map_err(|err| ResourceError::invalid(err.to_string()))?;
        let mut next = previous.resource_version.clone();
        for event in events {
            next = match event {
                crate::WatchEvent::Added(object) | crate::WatchEvent::Modified(object) | crate::WatchEvent::Deleted(object) => {
                    object.metadata.resource_version.clone()
                }
                crate::WatchEvent::DeletedByName(tombstone) => tombstone.resource_version.clone(),
            };
            let next_version = next.parse::<u64>().map_err(|err| ResourceError::invalid(err.to_string()))?;
            if version.checked_add(1) != Some(next_version) {
                return Err(ResourceError::invalid("replica sequence gap; snapshot required"));
            }
            version = next_version;
        }
        self.apply_batch(events, synced_at).await?;
        self.backend.advance_replica_cursor_typed::<T>(&self.origin_root, &self.namespace, &previous, &next).await
    }

    /// Apply a direct origin event, then commit its contiguous prefix. Relay
    /// `apply` never advances this cursor. A crash between these operations only
    /// replays an already-applied event; it cannot skip a missing event.
    pub async fn apply_direct(&self, event: crate::WatchEvent<T>, synced_at: DateTime<Utc>) -> Result<(), ResourceError> {
        self.apply_direct_batch(&[event], synced_at).await
    }

    /// Require a new authoritative snapshot without removing cached objects.
    pub async fn invalidate_cursor(&self) -> Result<(), ResourceError> {
        ensure_replication_enabled::<T>()?;
        self.backend.invalidate_replica_cursor_typed::<T>(&self.origin_root, &self.namespace).await
    }

    pub async fn cursor(&self) -> Result<Option<ReplicaCursor>, ResourceError> {
        ensure_replication_enabled::<T>()?;
        self.backend.replica_cursor_typed::<T>(&self.origin_root, &self.namespace).await
    }
}

fn ensure_replication_enabled<T: Resource>() -> Result<(), ResourceError> {
    if T::REPLICATION_CLASS != crate::ReplicationClass::None {
        Ok(())
    } else {
        Err(ResourceError::invalid(format!("{} is not enabled for overlay replication", T::API_PATHS.kind)))
    }
}

pub struct TypedResolver<T: Resource> {
    lifecycle_status_reads: Option<std::sync::Arc<dyn LifecycleReads>>,
    pub(crate) backend: ResourceBackend,
    pub(crate) namespace: String,
    pub(crate) _marker: PhantomData<T>,
}

impl<T: Resource> std::fmt::Debug for TypedResolver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TypedResolver").field("backend", &self.backend).field("namespace", &self.namespace).finish()
    }
}

impl<T: Resource> std::clone::Clone for TypedResolver<T> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            namespace: self.namespace.clone(),
            lifecycle_status_reads: self.lifecycle_status_reads.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: Resource> TypedResolver<T> {
    /// Local controller writes select engine validation only after a checked ownership read.
    pub fn with_lifecycle_status_reads(mut self, reads: std::sync::Arc<dyn LifecycleReads>) -> Self {
        self.lifecycle_status_reads = Some(reads);
        self
    }

    pub async fn get(&self, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        if let Some(copies) = self.backend.definitions::<T>(&self.namespace).engine_copies(name).await {
            // This home's own record: no built-in and no other home's.
            return copies?
                .into_iter()
                .find(|read| {
                    matches!(read.provenance, ResourceProvenance::Local)
                        && !flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, name)
                })
                .map(|read| read.object)
                .ok_or_else(|| ResourceError::not_found(name));
        }
        dispatch_backend!(self, get_typed, name)
    }

    /// This home's own records of an engine-served definition kind (#3824 layer 2b-ii), as the
    /// old store's local rows: no built-ins and no other home's records.
    async fn engine_local(&self) -> Option<Result<Vec<ResourceObject<T>>, ResourceError>> {
        let reads = self.backend.definitions::<T>(&self.namespace).engine_reads().await?;
        Some(reads.map(|reads| {
            reads
                .into_iter()
                .filter(|read| {
                    matches!(read.provenance, ResourceProvenance::Local)
                        && !flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, &read.object.metadata.name)
                })
                .map(|read| read.object)
                .collect()
        }))
    }

    /// Read the local collection's list/watch position without decoding objects.
    /// HTTP uses collection metadata from the standard list endpoint.
    pub async fn current_position(&self) -> Result<crate::ResourcePosition, ResourceError> {
        // An engine-served kind's position is where its port's reading stands in the logs.
        if let Some(position) = self.backend.definitions::<T>(&self.namespace).engine_position().await {
            return Ok(crate::ResourcePosition { resource_version: position?, generation: None });
        }
        dispatch_backend!(self, current_position_typed)
    }

    pub async fn list(&self) -> Result<ResourceList<T>, ResourceError> {
        // Positioned before the list: a resume from it names again any change the list holds.
        if let Some(position) = self.backend.definitions::<T>(&self.namespace).engine_position().await {
            let local = self.engine_local().await.ok_or_else(|| ResourceError::other("the engine stopped serving this kind"))?;
            return Ok(ResourceList { items: local?, resource_version: position?, generation: None });
        }
        dispatch_backend!(self, list_typed)
    }

    pub async fn list_matching_labels(&self, required: &BTreeMap<String, String>) -> Result<ResourceList<T>, ResourceError> {
        if let Some(position) = self.backend.definitions::<T>(&self.namespace).engine_position().await {
            let local = self.engine_local().await.ok_or_else(|| ResourceError::other("the engine stopped serving this kind"))?;
            let items = local?
                .into_iter()
                .filter(|object| required.iter().all(|(key, value)| object.metadata.labels.get(key) == Some(value)))
                .collect();
            return Ok(ResourceList { items, resource_version: position?, generation: None });
        }
        dispatch_backend!(self, list_typed_matching_labels, required)
    }

    pub async fn create(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        self.refuse_engine_write_elsewhere()?;
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return self.backend.definitions::<T>(&self.namespace).create(meta, spec).await;
        }
        if let Some(records) = self.engine_owned() {
            T::validate_spec(meta, spec)?;
            if self.engine_home_record(&*records, &meta.name).await?.is_some() {
                return Err(ResourceError::conflict(&meta.name, "resource already exists"));
            }
            return self.engine_apply(&*records, meta, spec).await;
        }
        dispatch_backend!(self, create_typed, meta, spec)
    }

    /// The engine port serving this kind, for a kind the old store keeps a record of per home
    /// (#3824 layer 3): definitions are served through their resolver instead.
    fn engine_owned(&self) -> Option<std::sync::Arc<dyn flotilla_resources::engine_records::KindRecords<T>>> {
        (T::REPLICATION_CLASS != crate::ReplicationClass::Definitions).then(|| self.backend.engine_records::<T>()).flatten()
    }

    /// Refuses a write through a namespace other than the one the engine serves this kind in.
    fn refuse_engine_write_elsewhere(&self) -> Result<(), ResourceError> {
        self.backend.definitions::<T>(&self.namespace).refuse_write_elsewhere()
    }

    /// `patch` applied to this home's engine record `name`'s status by its port, in the port's own
    /// order of writes; whether it wrote. `None` when no port serving the kind keeps one.
    pub(crate) async fn patch_engine_status(
        &self,
        name: &str,
        patch: &flotilla_resources::engine_records::PatchStatus<'_, T::Status>,
    ) -> Option<Result<bool, ResourceError>> {
        let records = self.backend.engine_records::<T>()?;
        if let Err(error) = self.refuse_engine_write_elsewhere() {
            return Some(Err(error));
        }
        Some(records.patch_status(name, patch).await?.map_err(ResourceError::other))
    }

    /// This home's own engine record `name`, if any.
    async fn engine_home_record(
        &self,
        records: &dyn flotilla_resources::engine_records::KindRecords<T>,
        name: &str,
    ) -> Result<Option<flotilla_resources::engine_records::Record<T>>, ResourceError> {
        let home = records.home();
        let copies = crate::port_reads::copies(records, name).await.map_err(ResourceError::other)?;
        Ok(copies.into_iter().find(|record| record.home == home))
    }

    /// Why `verb` on `name` fails when this home holds no copy: other homes' copies, named,
    /// since only a record's home writes it, a definition's too; otherwise not found.
    pub(crate) async fn not_held_here(&self, name: &str, verb: &str) -> Result<ResourceError, ResourceError> {
        self.refuse_engine_write_elsewhere()?;
        let Some(records) = self.backend.engine_records::<T>() else {
            return Ok(ResourceError::not_found(name));
        };
        let copies = crate::port_reads::copies(&*records, name).await.map_err(ResourceError::other)?;
        if copies.is_empty() {
            return Ok(ResourceError::not_found(name));
        }
        let homes = copies.iter().map(|copy| format!("`{}`", copy.home)).collect::<Vec<_>>().join(", ");
        Ok(ResourceError::conflict(name, format!("{} {name} is homed at {homes}; {verb} it there", T::API_PATHS.kind)))
    }

    /// The engine record `name` at this home, refused unless it is at `resource_version`.
    async fn engine_current(
        &self,
        records: &dyn flotilla_resources::engine_records::KindRecords<T>,
        name: &str,
        resource_version: &str,
    ) -> Result<(), ResourceError> {
        match self.engine_home_record(records, name).await? {
            None => Err(self.not_held_here(name, "write").await?),
            Some(current) if current.version.to_string() != resource_version => Err(ResourceError::conflict(name, "stale resourceVersion")),
            Some(_) => Ok(()),
        }
    }

    async fn engine_apply(
        &self,
        records: &dyn flotilla_resources::engine_records::KindRecords<T>,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let record = records.apply(&meta.name, &meta.labels, &meta.annotations, spec).await.map_err(ResourceError::other)?;
        Ok(crate::definition::record_object(&self.namespace, record))
    }

    pub async fn update(&self, meta: &InputMeta, resource_version: &str, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        self.refuse_engine_write_elsewhere()?;
        if let Some(records) = self.engine_owned() {
            if !records.writes() {
                return self.engine_apply(&*records, meta, spec).await;
            }
            self.engine_current(&*records, &meta.name, resource_version).await?;
            let current = self.get(&meta.name).await?;
            T::validate_spec_update(&current.spec, spec)?;
            T::validate_spec(meta, spec)?;
            return self.engine_apply(&*records, meta, spec).await;
        }
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            let definitions = self.backend.definitions::<T>(&self.namespace);
            let current = definitions.get(&meta.name).await?;
            let local_version_matches = self.get(&meta.name).await.is_ok_and(|local| local.metadata.resource_version == resource_version);
            if current.metadata.resource_version != resource_version && !local_version_matches {
                return Err(ResourceError::conflict(&meta.name, "stale resourceVersion"));
            }
            return definitions.apply(meta, spec).await;
        }
        dispatch_backend!(self, update_typed, meta, resource_version, spec)
    }

    /// Ownership can be acquired after the read below. A legacy validation refusal is
    /// retried by reconciliation with the newly held lifecycle; it cannot change engine state.
    /// HTTP validates on its server and does not transmit this local validator selection.
    pub async fn update_status(&self, name: &str, resource_version: &str, status: &T::Status) -> Result<ResourceObject<T>, ResourceError> {
        self.refuse_engine_write_elsewhere()?;
        let engine_owned = match (T::STATUS_LIFECYCLE_KIND, self.lifecycle_status_reads.as_ref()) {
            (Some(lifecycle), Some(reads)) => {
                let reads = std::sync::Arc::clone(reads);
                let name = name.to_owned();
                tokio::task::spawn_blocking(flotilla_schema::reads::carry(move || reads.try_reading(lifecycle, &name, None)))
                    .await
                    .map_err(|error| ResourceError::other(error.to_string()))?
                    .map_err(ResourceError::other)?
                    .is_some()
            }
            _ => false,
        };
        // A definition kind with a status (Project, #3824 layer 3b) writes it through its port too.
        if let Some(records) = self.backend.engine_records::<T>() {
            if records.writes() {
                self.engine_current(&*records, name, resource_version).await?;
            }
            let record = records.set_status(name, status).await.map_err(ResourceError::other)?;
            return Ok(crate::definition::record_object(&self.namespace, record));
        }
        if !engine_owned {
            return dispatch_backend!(self, update_status_typed, name, resource_version, status);
        }
        // Ownership is checked before the resource transaction. Endings do not revoke it:
        // the engine retains ended resources, so lifecycle progress cannot select the old validator again.
        let kind = crate::ResourceKind::of::<T>().with_engine_status_validator::<T>();
        let status = transcode_request(self.backend.blocking_request_encoding(), status.clone()).await?;
        crate::typed_object(self.backend.update_status_raw(kind, &self.namespace, name, resource_version, &status).await?)
    }

    pub async fn delete(&self, name: &str) -> Result<(), ResourceError> {
        self.refuse_engine_write_elsewhere()?;
        if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
            return self.backend.definitions::<T>(&self.namespace).delete(name).await;
        }
        if let Some(records) = self.engine_owned() {
            if self.engine_home_record(&*records, name).await?.is_none() {
                return Err(self.not_held_here(name, "delete").await?);
            }
            return records.delete(name).await.map_err(ResourceError::other);
        }
        dispatch_backend!(self, delete_typed, name)
    }

    pub(crate) async fn tombstone(&self, name: &str) -> Result<crate::watch::TombstoneWrite, ResourceError> {
        ensure_replication_enabled::<T>()?;
        let origin_root = self.backend.local_root()?;
        let replica_cursor = self.backend.replica_writer::<T>(origin_root, &self.namespace).cursor().await?;
        let minimum_resource_version = replica_cursor.as_ref().map(|cursor| cursor.resource_version.as_str());
        dispatch_backend!(self, tombstone_typed, name, minimum_resource_version)
    }

    pub async fn watch(&self, start: WatchStart) -> Result<WatchStream<T>, ResourceError> {
        let definitions = self.backend.definitions::<T>(&self.namespace);
        let watched = match &start {
            // An engine-served kind resumes from the position its list or watch handed out.
            WatchStart::FromVersion(requested_version)
            | WatchStart::FromVersionInGeneration { resource_version: requested_version, .. } => {
                definitions.engine_watch_from(requested_version).await
            }
            // An engine-served kind streams every home's changes from now.
            WatchStart::Now => definitions.engine_watch().await.map(|watch| watch.map(|(_, stream)| stream)),
        };
        if let Some(stream) = watched {
            let events = stream?.map(|event| {
                event.map(|event| match event {
                    ReadWatchEvent::Added(read) => crate::WatchEvent::Added(read.object),
                    ReadWatchEvent::Modified(read) => crate::WatchEvent::Modified(read.object),
                    ReadWatchEvent::Deleted(read) => crate::WatchEvent::Deleted(read.object),
                    ReadWatchEvent::DeletedByName { tombstone, .. } => crate::WatchEvent::DeletedByName(tombstone),
                })
            });
            return Ok(WatchStream::new(None, events.boxed()));
        }
        dispatch_backend!(self, watch_typed, start)
    }
}

impl<T: Resource> TypedResolver<T> {
    /// The backing store and namespace used by this typed resolver.
    pub fn context(&self) -> (&ResourceBackend, &str) {
        (&self.backend, &self.namespace)
    }
}

impl std::ops::Deref for ResourceBackend {
    type Target = dyn crate::Store;
    fn deref(&self) -> &Self::Target {
        &*self.store
    }
}

impl<S: crate::Store + 'static> From<S> for ResourceBackend {
    fn from(store: S) -> Self {
        Self::from_arc(std::sync::Arc::new(store))
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use flotilla_resources::{
        engine_records::{KindRecords, LogPosition, Record, Since},
        Artifact, ImageBuild, Resource, Vessel, VesselSpec,
    };
    use futures::stream::BoxStream;

    /// A port serving `records`; refusing the kind when it does not read it, and failing every
    /// read when `failing`.
    struct Port<R: Resource> {
        records: Vec<Record<R>>,
        reads: bool,
        failing: bool,
    }

    impl<R: Resource> std::fmt::Debug for Port<R> {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.debug_struct("Port").field("records", &self.records).finish_non_exhaustive()
        }
    }

    impl<R: Resource> Port<R> {
        fn serving(records: Vec<Record<R>>) -> Arc<Self> {
            Arc::new(Self { records, reads: true, failing: false })
        }
    }

    #[async_trait::async_trait]
    impl<R: Resource> KindRecords<R> for Port<R> {
        fn codes(&self) -> Vec<flotilla_resources::engine_records::KindCode> {
            Vec::new()
        }
        fn ticks(&self) -> flotilla_resources::engine_records::Ticks {
            flotilla_resources::engine_records::Ticks::AtCommit
        }
        fn reads(&self) -> bool {
            self.reads
        }
        fn writes(&self) -> bool {
            false
        }
        fn home(&self) -> String {
            "kiwi".into()
        }
        async fn get(&self, name: &str) -> Result<Option<Record<R>>, String> {
            Ok(self.sources().await?.into_iter().find(|record| record.name == name))
        }
        async fn list(&self) -> Result<Vec<Record<R>>, String> {
            self.sources().await
        }
        async fn sources(&self) -> Result<Vec<Record<R>>, String> {
            if !self.reads {
                return Err(format!("{} is not read through the resource commands", R::API_PATHS.kind));
            }
            if self.failing {
                return Err("the records could not be read".into());
            }
            Ok(self.records.clone())
        }
        async fn positioned(&self) -> Result<(Vec<Record<R>>, LogPosition), String> {
            Ok((self.sources().await?, LogPosition::default()))
        }
        async fn position(&self) -> Result<LogPosition, String> {
            Ok(LogPosition::default())
        }
        async fn changed_since(&self, _: &LogPosition) -> Result<Since, String> {
            Ok(Since::Expired("no log is held".into()))
        }
        async fn copies(&self, name: &str) -> Result<Vec<Record<R>>, String> {
            Ok(self.sources().await?.into_iter().filter(|record| record.name == name).collect())
        }
        async fn apply(
            &self,
            _name: &str,
            _labels: &BTreeMap<String, String>,
            _annotations: &BTreeMap<String, String>,
            _spec: &R::Spec,
        ) -> Result<Record<R>, String> {
            Err("read only".into())
        }
        async fn set_status(&self, _name: &str, _status: &R::Status) -> Result<Record<R>, String> {
            Err("read only".into())
        }
        async fn delete(&self, _name: &str) -> Result<(), String> {
            Err("read only".into())
        }
        async fn changes(&self) -> Result<BoxStream<'static, ()>, String> {
            Err("no changes".into())
        }
    }

    fn vessel() -> Record<Vessel> {
        Record {
            name: "v#1".into(),
            home: "kiwi".into(),
            version: 1,
            created_at: chrono::DateTime::UNIX_EPOCH,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec: VesselSpec {
                convoy_ref: "c".into(),
                vessel_name: "w".into(),
                fulfilment_kind_ref: "k".into(),
                adopted_checkout_refs: BTreeMap::new(),
            },
            status: None,
        }
    }

    // Behaviour: kind discovery lists a kind the engine serves only to refuse in no namespace,
    // and a kind whose read fails fails discovery naming that kind.
    #[tokio::test]
    async fn namespace_inventory_lists_refused_kinds_empty_and_names_a_failing_kind() {
        let backend = crate::test_store::memory_backend();
        backend.serve_engine_in("flotilla");
        backend.install_engine_kind::<Vessel>(Port::serving(vec![vessel()]));
        backend.install_engine_kind::<ImageBuild>(Arc::new(Port { records: Vec::new(), reads: false, failing: false }));
        let inventory = backend.stored_namespace_inventory().await.expect("inventory");
        assert_eq!(inventory.get("imagebuilds"), Some(&Vec::new()), "image builds are in no namespace: {inventory:?}");
        assert_eq!(inventory.get("vessels"), Some(&vec!["flotilla".to_owned()]), "{inventory:?}");

        backend.install_engine_kind::<Artifact>(Arc::new(Port { records: Vec::new(), reads: true, failing: true }));
        let error = backend.stored_namespace_inventory().await.expect_err("a failing kind fails discovery").to_string();
        assert!(error.contains("Artifact") && error.contains("could not be read"), "the failing kind is named: {error}");
    }

    // Behaviour: a write of a kind its port never writes is refused for that reason, though the
    // record moved since the writer read it: a moved record is no conflict where nothing is
    // written, and a conflict would send its writer to read and write again.
    #[tokio::test]
    async fn a_write_the_port_refuses_is_refused_whatever_version_it_names() {
        let backend = crate::test_store::memory_backend();
        backend.serve_engine_in("flotilla");
        backend.install_engine_kind::<Vessel>(Port::serving(vec![vessel()]));
        let vessels = backend.using::<Vessel>("flotilla");
        let meta = crate::InputMeta::builder().name("v#1".into()).build();
        let stale = "0";
        let refused = vessels.update(&meta, stale, &vessel().spec).await.expect_err("a spec write is refused");
        assert!(refused.to_string().contains("read only"), "refused by its port, not as a conflict: {refused}");
        let refused = vessels.update_status("v#1", stale, &Default::default()).await.expect_err("a status write is refused");
        assert!(refused.to_string().contains("read only"), "refused by its port, not as a conflict: {refused}");
    }

    thread_local! {
        static ENCODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// A kind whose spec counts each time it is encoded.
    struct Counted;

    #[derive(Debug, Clone, serde::Deserialize)]
    struct CountedSpec(String);

    impl serde::Serialize for CountedSpec {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            ENCODED.with(|encoded| encoded.set(encoded.get() + 1));
            serializer.serialize_str(&self.0)
        }
    }

    impl Resource for Counted {
        type Spec = CountedSpec;
        type Status = ();
        type StatusPatch = flotilla_resources::NoStatusPatch;
        const API_PATHS: flotilla_resources::ApiPaths =
            flotilla_resources::ApiPaths { group: "flotilla.work", version: "v1", plural: "counteds", kind: "Counted" };
    }

    // Behaviour (#3108 R3): a typed read of an engine kind through its resolver hands over the
    // port's records as they are: a get and a list encode nothing, and a declared query encodes
    // only the paths it selects by.
    #[tokio::test]
    async fn a_typed_read_of_an_engine_kind_encodes_nothing() {
        let backend = crate::test_store::memory_backend();
        backend.serve_engine_in("flotilla");
        let record = Record::<Counted> {
            name: "one".into(),
            home: "kiwi".into(),
            version: 1,
            created_at: chrono::DateTime::UNIX_EPOCH,
            labels: BTreeMap::new(),
            annotations: BTreeMap::new(),
            spec: CountedSpec("value".into()),
            status: None,
        };
        backend.install_engine_kind::<Counted>(Port::serving(vec![record]));
        let resolver = backend.using::<Counted>("flotilla");
        assert_eq!(resolver.get("one").await.expect("get").spec.0, "value");
        assert_eq!(resolver.list().await.expect("list").items.len(), 1);
        assert_eq!(ENCODED.with(std::cell::Cell::get), 0, "a typed read encodes no spec");

        // A declared query reads only the paths it selects by: none of them in the spec.
        let query = crate::StoreQuery { fields: BTreeMap::from([("metadata.name".to_owned(), "one".to_owned())]), ..Default::default() };
        let selected = resolver.query_declared(&query).await.expect("query").items;
        assert_eq!(selected.iter().map(|read| read.object.metadata.name.as_str()).collect::<Vec<_>>(), ["one"]);
        assert_eq!(ENCODED.with(std::cell::Cell::get), 0, "a declared query encodes no spec");
    }
}
