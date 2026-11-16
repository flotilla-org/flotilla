use crate::StoreExt;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    marker::PhantomData,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use flotilla_resources::{
    engine_records::{KindRecords, LogPosition, Record, Since},
    ObjectMeta, ReadResourceObject, ReadWatchEvent,
};
use futures::{stream::BoxStream, StreamExt};
use serde_json::{Map, Value};

use crate::engine_feed::Copies;
use crate::port_reads;
use crate::ProjectHierarchyStoreExt;
use crate::{
    CausalDot, FieldMergeMetadata, InputMeta, MergeConflictSibling, MergeMetadata, ReplicationClass, Resource, ResourceBackend,
    ResourceError, ResourceObject, ResourceProvenance, WriterIdentity,
};

const DELETION_FIELD: &str = "$deleted";

#[derive(Debug)]
struct DefinitionSource<T: Resource> {
    object: ResourceObject<T>,
    origin: NodeId,
    synced_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct FieldCandidate {
    value: Value,
    metadata: FieldMergeMetadata,
}

/// Read/write view for a definitions-class resource kind.
///
/// Reads merge the locally-authored record with every durable origin replica.
/// Writes always append to this root's local authority after the store stamps
/// causal context derived from that merged view.
#[derive(Debug, Clone)]
pub struct DefinitionResolver<T: Resource> {
    backend: ResourceBackend,
    namespace: String,
    _marker: PhantomData<T>,
}

impl<T: Resource> DefinitionResolver<T> {
    pub(crate) fn new(backend: ResourceBackend, namespace: String) -> Self {
        Self { backend, namespace, _marker: PhantomData }
    }

    /// The engine's records of this kind, installed behind `[primitives]` (#3824 layer 2b-ii).
    fn records(&self) -> Option<Arc<dyn KindRecords<T>>> {
        self.backend.engine_records::<T>()
    }

    /// Whether this namespace holds the engine's records of this kind. The engine serves its kinds
    /// in one namespace; a read or watch through any other finds none of them, as the old store
    /// found none of one namespace's records through another.
    fn holds_engine_records(&self) -> bool {
        self.backend.engine_namespace(T::API_PATHS.kind).is_none_or(|namespace| namespace == self.namespace)
    }

    /// Refuses a write through a namespace other than the one the engine serves this kind in: its
    /// records can be in no other.
    pub(crate) fn refuse_write_elsewhere(&self) -> Result<(), ResourceError> {
        match self.backend.engine_namespace(T::API_PATHS.kind) {
            Some(namespace) if namespace != self.namespace => {
                Err(ResourceError::invalid(format!("{} is served in namespace {namespace}", T::API_PATHS.kind)))
            }
            _ => Ok(()),
        }
    }

    /// An engine record as a resource in this namespace.
    fn object(&self, record: Record<T>) -> ResourceObject<T> {
        record_object(&self.namespace, record)
    }

    /// Every engine record of this kind read through this namespace, with the built-ins.
    async fn engine_list(&self, records: &dyn KindRecords<T>) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        let listed = if self.holds_engine_records() { port_reads::list(records).await.map_err(ResourceError::other)? } else { Vec::new() };
        let objects = listed.into_iter().map(|record| self.object(record)).collect();
        with_builtins(T::API_PATHS.kind, &self.namespace, objects, None)
    }

    /// Where a reading of the engine's records of this kind now starts, as a resource version a
    /// watch resumes from; `None` unless the kind is served by the engine.
    pub(crate) async fn engine_position(&self) -> Option<Result<String, ResourceError>> {
        let records = self.records()?;
        Some(records.position().await.map(|position| position.encode()).map_err(ResourceError::other))
    }

    /// The engine's records of this kind as replica reads, every home's copy with its home as
    /// provenance, and the built-ins as local; `None` unless the kind is served by the engine.
    pub(crate) async fn engine_reads(&self) -> Option<Result<Vec<ReadResourceObject<T>>, ResourceError>> {
        let records = self.records()?;
        Some(self.read_all(&*records).await)
    }

    /// [`Self::engine_reads`] for `query`: a query for one receiver's open messages, or one whose
    /// rows its index keys cover, reads them through the port's index when it keeps one, and every
    /// other reads them all.
    pub(crate) async fn engine_reads_for(&self, query: &crate::StoreQuery) -> Option<Result<Vec<ReadResourceObject<T>>, ResourceError>> {
        let records = self.records()?;
        if !self.holds_engine_records() {
            return Some(self.reads(&records.home(), Vec::new(), None));
        }
        if let Some(receiver) = query.message.as_ref().and_then(crate::MessageQuery::open_receiver) {
            if let Some(open) = port_reads::open_to(&*records, receiver).await {
                return Some(open.map_err(ResourceError::other).and_then(|open| self.reads(&records.home(), open, None)));
            }
        }
        if let Some(cover) = query.index_cover() {
            // A query of this home's rows reads none of another home's.
            if let Some(held) = port_reads::covered(&*records, &cover, !query.include_replicas).await {
                return Some(held.map_err(ResourceError::other).and_then(|held| self.reads(&records.home(), held, None)));
            }
        }
        Some(self.read_all(&*records).await)
    }

    async fn read_all(&self, records: &dyn KindRecords<T>) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        // Every home's copy, as the old store's replica reads return each origin's.
        let listed =
            if self.holds_engine_records() { port_reads::sources(records).await.map_err(ResourceError::other)? } else { Vec::new() };
        self.reads(&records.home(), listed, None)
    }

    /// Every home's copy of `name`, as replica reads, with a built-in of that name; `None` unless
    /// the kind is served by the engine.
    pub(crate) async fn engine_copies(&self, name: &str) -> Option<Result<Vec<ReadResourceObject<T>>, ResourceError>> {
        let records = self.records()?;
        let copies = match self.holds_engine_records() {
            false => Vec::new(),
            true => match port_reads::copies(&*records, name).await {
                Ok(copies) => copies,
                Err(error) => return Some(Err(ResourceError::other(error))),
            },
        };
        Some(self.reads(&records.home(), copies, Some(name)))
    }

    /// `listed` as replica reads, each home's copy with its home as provenance and this home's as
    /// local, followed by the built-ins (those named `name`, when given).
    fn reads(&self, home: &str, listed: Vec<Record<T>>, name: Option<&str>) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        let mut reads = Vec::with_capacity(listed.len());
        for record in listed {
            let provenance = if record.home == home {
                ResourceProvenance::Local
            } else {
                ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: Utc::now() }
            };
            reads.push(ReadResourceObject { object: self.object(record), provenance });
        }
        let builtins = builtin_objects::<T>(T::API_PATHS.kind, &self.namespace)?;
        reads.extend(
            builtins
                .into_iter()
                .filter(|object| name.is_none_or(|name| object.metadata.name == name))
                .map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }),
        );
        Ok(reads)
    }

    /// The engine's records of this kind as a watch: the current reads, then each change of any
    /// home's records as the difference from the reads before it. Every watcher of the kind on this
    /// backend shares one listing of it; each converts only the records that changed. `None` unless
    /// the kind is served by the engine.
    #[allow(clippy::type_complexity)]
    pub(crate) async fn engine_watch(
        &self,
    ) -> Option<Result<(Vec<ReadResourceObject<T>>, BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>), ResourceError>> {
        Some(self.engine_watch_fed().await?.map(|(initial, _, stream)| (initial, events_of(stream))))
    }

    /// [`Self::engine_watch`] with the position its current reads stand at, and a bookmark after
    /// each batch of changes.
    #[allow(clippy::type_complexity)]
    pub(crate) async fn engine_watch_fed(&self) -> Option<Result<(Vec<ReadResourceObject<T>>, String, FedStream<T>), ResourceError>> {
        let records = self.records()?;
        if !self.holds_engine_records() {
            // This namespace holds only the built-ins, which never change.
            let initial = match self.reads(&records.home(), Vec::new(), None) {
                Ok(initial) => initial,
                Err(error) => return Some(Err(error)),
            };
            let position = match records.position().await {
                Ok(position) => position.encode(),
                Err(error) => return Some(Err(ResourceError::other(error))),
            };
            return Some(Ok((initial, position, futures::stream::pending().boxed())));
        }
        let watch = match self.engine_watcher(&records.home(), false).await? {
            Ok(watch) => watch,
            Err(error) => return Some(Err(error)),
        };
        let initial = watch.seen.reads.values().map(|(_, read)| clone_read(read)).collect();
        let position = watch.position.clone();
        Some(Ok((initial, position, watch.stream())))
    }

    /// The engine's records of this kind as a watch resumed from `requested_version`, a position
    /// an engine list or watch handed out: each name changed since as its current read, or deleted,
    /// then each later change. Expired where the position cannot be resumed from. `None` unless
    /// the kind is served by the engine.
    pub(crate) async fn engine_watch_from(
        &self,
        requested_version: &str,
    ) -> Option<Result<BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>, ResourceError>> {
        Some(self.engine_watch_from_fed(requested_version).await?.map(events_of))
    }

    /// [`Self::engine_watch_from`] with a bookmark after the changes since, and after each later
    /// batch.
    pub(crate) async fn engine_watch_from_fed(&self, requested_version: &str) -> Option<Result<FedStream<T>, ResourceError>> {
        let records = self.records()?;
        let expired = |reason: String| {
            tracing::info!(kind = T::API_PATHS.kind, %reason, "engine watch: its position expired; the watcher lists afresh");
            ResourceError::WatchExpired { requested_version: requested_version.to_owned(), compacted_through: None, reason }
        };
        let Some(position) = LogPosition::decode(requested_version) else {
            return Some(Err(expired("cursor not readable: it is not a position this daemon handed out".to_owned())));
        };
        if !self.holds_engine_records() {
            return Some(Ok(futures::stream::pending().boxed()));
        }
        let mut watch = match self.engine_watcher(&records.home(), true).await? {
            Ok(watch) => watch,
            Err(error) => return Some(Err(error)),
        };
        // Read after the watcher's records: every name changed before them is named here, and
        // each change after them is also in a later batch.
        let names = match records.changed_since(&position).await {
            Ok(Since::Changed { names, .. }) => names,
            Ok(Since::Expired(reason)) => return Some(Err(expired(reason))),
            Err(error) => return Some(Err(ResourceError::other(error))),
        };
        let position = watch.position.clone();
        for name in names.into_iter().filter(|name| !flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, name)) {
            // A name named but not held changed and is gone: whether the resumer held it is not known.
            let event = match watch.seen.reads.get(&name) {
                Some((_, read)) => ReadWatchEvent::Modified(clone_read(read)),
                None => ReadWatchEvent::DeletedByName {
                    tombstone: flotilla_resources::ResourceTombstone {
                        name,
                        namespace: self.namespace.clone(),
                        resource_version: position.clone(),
                        annotations: BTreeMap::new(),
                    },
                    provenance: ResourceProvenance::Local,
                },
            };
            watch.pending.push_back(Ok(Fed::Event(event)));
        }
        watch.pending.push_back(Ok(Fed::Bookmark(position)));
        Some(Ok(watch.stream()))
    }

    /// A watcher of the kind's shared feed, holding the reads of its records now; for a resume,
    /// with every change committed before it.
    async fn engine_watcher(&self, home: &str, resuming: bool) -> Option<Result<EngineWatch<T>, ResourceError>> {
        let feed = match self.backend.engine_feed::<T>().await? {
            Ok(feed) => feed,
            Err(error) => return Some(Err(error)),
        };
        // Subscribed before the records are read: a batch after them is in the watcher's next events.
        let serials = feed.serials();
        if resuming {
            // The records hold every change committed before the resume, so each name it reads
            // since its position shows as the feed holds it now, and later changes come in batches.
            feed.read_through_now().await;
        }
        let mut seen = SeenReads::<T>::default();
        let (serial, position) = {
            let state = feed.state();
            if let Err(error) = self.diff(home, &state.records, &mut seen) {
                return Some(Err(error));
            }
            (state.serial, state.position.encode())
        };
        Some(Ok(EngineWatch {
            feed,
            serials,
            serial,
            seen,
            position,
            pending: VecDeque::new(),
            resolver: Self::new(self.backend.clone(), self.namespace.clone()),
            home: home.to_owned(),
        }))
    }

    /// The watch events taking `seen` to every record, with the built-ins: a record whose home,
    /// version, labels and annotations are unchanged keeps its earlier read without decoding again.
    /// A name several homes hold reads as the last listed.
    fn diff(&self, home: &str, records: &Copies<T>, seen: &mut SeenReads<T>) -> Result<Vec<ReadWatchEvent<T>>, ResourceError> {
        let mut current = BTreeMap::new();
        for record in records.values().flatten() {
            let stamp = RecordStamp::of(record);
            let read = self.read_of(home, record, &stamp, seen);
            current.insert(record.name.clone(), (Some(stamp), read));
        }
        for object in builtin_objects::<T>(T::API_PATHS.kind, &self.namespace)? {
            current.insert(object.metadata.name.clone(), (None, ReadResourceObject { object, provenance: ResourceProvenance::Local }));
        }
        let mut previous = std::mem::take(&mut seen.reads);
        let mut pending = Vec::new();
        for (name, (_, read)) in &current {
            match previous.remove(name) {
                None => pending.push(ReadWatchEvent::Added(clone_read(read))),
                Some((_, before)) if changed(&before, read) => pending.push(ReadWatchEvent::Modified(clone_read(read))),
                Some(_) => {}
            }
        }
        pending.extend(previous.into_values().map(|(_, read)| ReadWatchEvent::Deleted(read)));
        seen.reads = current;
        Ok(pending)
    }

    /// The watch events taking `seen` to `records` for `names` alone, as [`Self::diff`] reads
    /// them; a built-in's name stays the built-in.
    fn diff_names(&self, home: &str, records: &Copies<T>, names: &BTreeSet<String>, seen: &mut SeenReads<T>) -> Vec<ReadWatchEvent<T>> {
        let mut pending = Vec::new();
        for name in names.iter().filter(|name| !flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, name)) {
            let current = records.get(name).and_then(|copies| copies.last()).map(|record| {
                let stamp = RecordStamp::of(record);
                let read = self.read_of(home, record, &stamp, seen);
                (stamp, read)
            });
            match (seen.reads.remove(name), current) {
                (None, Some((stamp, read))) => {
                    pending.push(ReadWatchEvent::Added(clone_read(&read)));
                    seen.reads.insert(name.clone(), (Some(stamp), read));
                }
                (Some((_, before)), Some((stamp, read))) => {
                    if changed(&before, &read) {
                        pending.push(ReadWatchEvent::Modified(clone_read(&read)));
                    }
                    seen.reads.insert(name.clone(), (Some(stamp), read));
                }
                (Some((_, before)), None) => pending.push(ReadWatchEvent::Deleted(before)),
                (None, None) => {}
            }
        }
        pending
    }

    /// `record`'s read: the one `seen` holds while its stamp is unchanged, else converted afresh.
    fn read_of(&self, home: &str, record: &Record<T>, stamp: &RecordStamp, seen: &SeenReads<T>) -> ReadResourceObject<T> {
        match seen.reads.get(&record.name) {
            Some((Some(before), read)) if before == stamp => clone_read(read),
            _ => {
                let provenance = if record.home == home {
                    ResourceProvenance::Local
                } else {
                    ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: Utc::now() }
                };
                ReadResourceObject { object: self.object(record.clone()), provenance }
            }
        }
    }

    /// Writes `spec` under `meta`'s name at this home, with the origin `meta`'s stamps name.
    async fn engine_apply(
        &self,
        records: &dyn KindRecords<T>,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let record = records.apply(&meta.name, &meta.labels, &meta.annotations, spec).await.map_err(ResourceError::other)?;
        Ok(self.object(record))
    }

    pub async fn get(&self, name: &str) -> Result<ResourceObject<T>, ResourceError> {
        ensure_definitions::<T>()?;
        // Decode the built-ins only for a built-in's name.
        if flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, name) {
            if let Some(builtin) =
                builtin_objects::<T>(T::API_PATHS.kind, &self.namespace)?.into_iter().find(|object| object.metadata.name == name)
            {
                return Ok(builtin);
            }
        }
        if let Some(records) = self.records() {
            if !self.holds_engine_records() {
                return Err(ResourceError::not_found(name));
            }
            let record = port_reads::get(&*records, name).await.map_err(ResourceError::other)?;
            return record.map(|record| self.object(record)).ok_or_else(|| ResourceError::not_found(name));
        }
        if self.backend.serves_read_views() {
            return self.backend.using::<T>(&self.namespace).get(name).await;
        }
        let sources = self.sources_for_name(name).await?;
        if sources.is_empty() {
            return Err(ResourceError::not_found(name));
        }
        let object = merge_sources(&sources)?;
        if !definition_is_visible(&object) {
            return Err(ResourceError::not_found(name));
        }
        Ok(object)
    }

    /// Select indexed candidate names, hydrate their origins in one store read,
    /// then apply causal merge and revalidate the selection against the merged value.
    pub async fn query_declared(&self, query: &crate::StoreQuery) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        ensure_definitions::<T>()?;
        let hydration =
            crate::StoreQuery { all_sources_for_matches: true, include_replicas: true, suppress_self_origin: false, ..query.clone() };
        if let Some(records) = self.records() {
            // Refused as the store refuses the hydration.
            hydration.validate(crate::ResourceKind::of::<T>())?;
            return select(self.engine_list(&*records).await?, query);
        }
        let result = self.backend.store().query_decoded(crate::ResourceKind::of::<T>(), &self.namespace, &hydration).await?;
        let root = result.local_root.clone().ok_or_else(|| ResourceError::invalid("definition query has no authority root"))?;
        let merged = with_builtins(T::API_PATHS.kind, &self.namespace, merge_cached_definitions(result.into_typed::<T>()?, &root)?, None)?;
        select(merged, query)
    }

    pub async fn list(&self) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        ensure_definitions::<T>()?;
        if let Some(records) = self.records() {
            return self.engine_list(&*records).await;
        }
        if self.backend.serves_read_views() {
            let listed = self.backend.using::<T>(&self.namespace).list().await?.items;
            return with_builtins(T::API_PATHS.kind, &self.namespace, listed, None);
        }
        let mut by_name = BTreeMap::<String, Vec<DefinitionSource<T>>>::new();
        for source in self.sources().await? {
            by_name.entry(source.object.metadata.name.clone()).or_default().push(source);
        }
        let mut merged = Vec::with_capacity(by_name.len());
        for sources in by_name.into_values() {
            let object = merge_sources(&sources)?;
            if definition_is_visible(&object) {
                merged.push(object);
            }
        }
        with_builtins(T::API_PATHS.kind, &self.namespace, merged, None)
    }

    /// Read a named inventory once while merging every author for each name.
    pub async fn list_names(&self, names: impl IntoIterator<Item = String>) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        ensure_definitions::<T>()?;
        if let Some(records) = self.records() {
            let names: BTreeSet<String> = names.into_iter().collect();
            return Ok(self.engine_list(&*records).await?.into_iter().filter(|object| names.contains(&object.metadata.name)).collect());
        }
        let mut query = crate::StoreQuery::names(names, true);
        query.suppress_self_origin = false;
        query.merged_definitions = true;
        Ok(self.backend.query_declared::<T>(&self.namespace, &query).await?.items.into_iter().map(|item| item.object).collect())
    }

    /// Select definition names through declared indexes, then merge every
    /// author of each selected name. Predicates may need checking after merge.
    ///
    /// The selection runs over stored rows, so it never selects a built-in definition
    /// (#3824 layer 2a); a caller that wants the built-ins too reads them by name or by `list`.
    pub async fn list_selected(&self, mut selection: crate::StoreQuery) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        ensure_definitions::<T>()?;
        if let Some(records) = self.records() {
            // The stored selection never selects a built-in; neither does this one.
            if !self.holds_engine_records() {
                return Ok(Vec::new());
            }
            let stored = port_reads::list(&*records).await.map_err(ResourceError::other)?;
            let objects = stored.into_iter().map(|record| self.object(record)).collect();
            return select(objects, &selection);
        }
        selection.include_replicas = true;
        selection.suppress_self_origin = false;
        let query = crate::StoreQuery {
            include_replicas: true,
            suppress_self_origin: false,
            merged_definitions: true,
            ..crate::StoreQuery::related("metadata.name", "metadata.name", selection)
        };
        Ok(self.backend.query_declared::<T>(&self.namespace, &query).await?.items.into_iter().map(|item| item.object).collect())
    }

    pub async fn create(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        self.create_as(&WriterIdentity::operator().with_source("definition-create"), meta, spec).await
    }

    pub async fn create_as(&self, writer: &WriterIdentity, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        refuse_builtin::<T>(&meta.name)?;
        match self.get(&meta.name).await {
            Ok(_) => Err(ResourceError::conflict(&meta.name, "resource already exists")),
            Err(ResourceError::NotFound { .. }) => self.apply_as(writer, meta, spec).await,
            Err(error) => Err(error),
        }
    }

    pub async fn apply(&self, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        self.apply_as(&WriterIdentity::operator().with_source("definition-apply"), meta, spec).await
    }

    pub async fn apply_as(&self, writer: &WriterIdentity, meta: &InputMeta, spec: &T::Spec) -> Result<ResourceObject<T>, ResourceError> {
        ensure_definitions::<T>()?;
        refuse_builtin::<T>(&meta.name)?;
        self.refuse_write_elsewhere()?;
        let _hierarchy_admission = self.backend.hierarchy_admission(T::API_PATHS.kind).await;
        T::validate_spec(meta, spec)?;
        if crate::project_hierarchy::is_hierarchy_kind(T::API_PATHS.kind) {
            crate::project_hierarchy::validate_hierarchy_write(
                &self.backend,
                &self.namespace,
                T::API_PATHS.kind,
                &meta.name,
                serde_json::to_value(spec).map_err(|error| ResourceError::decode(error.to_string()))?,
            )
            .await?;
        }
        // Project hierarchy validation above already includes merged siblings;
        // the embedded store still repeats local cycle checks atomically.
        if T::VALIDATE_NAMESPACE_SPEC && !crate::project_hierarchy::is_hierarchy_kind(T::API_PATHS.kind) {
            // This pass includes merged replicas. Embedded stores repeat the local
            // check under their lock/transaction to serialize local admissions.
            // No-op reapplies intentionally refuse legacy overlaps until repaired.
            // list() excludes resolved tombstones; deletion conflicts stay visible
            // and must continue reserving ownership for namespace consumers.
            let siblings = self.list().await?.into_iter().filter(|object| object.metadata.name != meta.name).collect::<Vec<_>>();
            T::validate_spec_with_named_siblings(meta, spec, &siblings)?;
        }
        if let Some(records) = self.records() {
            return self.engine_apply(&*records, meta, spec).await;
        }
        let local_root = self.backend.local_root()?;
        let sources = self.sources_for_name(&meta.name).await?;
        let current = (!sources.is_empty()).then(|| merge_sources(&sources)).transpose()?;
        let mut admitted_meta = meta.clone();
        admitted_meta.deletion_timestamp = None;
        let requested = spec_object(spec)?;
        let current_spec = current.as_ref().map(|object| spec_object(&object.spec)).transpose()?;
        let metadata_changed = current.as_ref().is_some_and(|object| InputMeta::from(&object.metadata) != admitted_meta);
        let mut changed = current_spec.is_none() || metadata_changed;
        let mut merge = current.as_ref().and_then(|object| object.metadata.merge.clone()).unwrap_or_else(|| MergeMetadata {
            fields: BTreeMap::new(),
            seen: BTreeMap::new(),
            conflicts: BTreeMap::new(),
        });
        let context = causal_context(&sources);
        let next_counter = context.get(&local_root).copied().unwrap_or_default().saturating_add(1);
        let dot = CausalDot { author_root: local_root.clone(), author_counter: next_counter };
        let now = Utc::now();
        let has_value_changes =
            requested.iter().any(|(field, value)| current_spec.as_ref().and_then(|current| current.get(field)) != Some(value));
        let resolves_existing_view = current.is_some() && !has_value_changes;
        let mut spec_changed = false;

        for (field, value) in &requested {
            let path = format!("spec.{field}");
            let value_changed = current_spec.as_ref().and_then(|current| current.get(field)) != Some(value);
            // A full-spec apply cannot distinguish an echoed merged value from
            // an intentional choice of that sibling. Treat a pure no-op apply
            // as conflict resolution, but do not let an edit to one field
            // silently collapse conflicts in untouched fields.
            let resolves_conflict = resolves_existing_view && merge.conflicts.contains_key(&path);
            if value_changed || resolves_conflict || current.is_none() {
                changed = true;
                spec_changed = true;
                merge.fields.insert(
                    path,
                    FieldMergeMetadata { dot: dot.clone(), seen: context.clone(), written_at: now, writer: Some(writer.clone()) },
                );
            }
        }
        if spec_changed {
            tracing::info!(
                kind = T::API_PATHS.kind,
                namespace = %self.namespace,
                name = %meta.name,
                writer_role = ?writer.role,
                writer_source = writer.source.as_deref().unwrap_or("unknown"),
                writer_root = %local_root,
                "applying definition spec"
            );
        }
        if merge.conflicts.contains_key(DELETION_FIELD)
            || current.as_ref().is_some_and(|object| object.metadata.deletion_timestamp.is_some())
        {
            changed = true;
        }
        if !changed {
            return current.ok_or_else(|| ResourceError::not_found(&meta.name));
        }

        merge.fields.insert(
            DELETION_FIELD.to_string(),
            FieldMergeMetadata { dot: dot.clone(), seen: context.clone(), written_at: now, writer: Some(writer.clone()) },
        );
        merge.seen = context;
        merge.seen.insert(local_root, next_counter);
        merge.conflicts.clear();

        match self.backend.using::<T>(&self.namespace).get(&meta.name).await {
            Ok(local) => {
                self.backend
                    .update_definition_typed::<T>(&self.namespace, &admitted_meta, &local.metadata.resource_version, spec, merge)
                    .await?;
            }
            Err(ResourceError::NotFound { .. }) => {
                self.backend.create_definition_typed::<T>(&self.namespace, &admitted_meta, spec, merge).await?;
            }
            Err(error) => return Err(error),
        }
        self.get(&meta.name).await
    }

    pub async fn delete(&self, name: &str) -> Result<(), ResourceError> {
        ensure_definitions::<T>()?;
        refuse_builtin::<T>(name)?;
        self.refuse_write_elsewhere()?;
        let _hierarchy_admission = self.backend.hierarchy_admission(T::API_PATHS.kind).await;
        if T::API_PATHS.kind == "Project" {
            let hierarchy = crate::ProjectHierarchy::load_for_inspection(&self.backend, &self.namespace).await?;
            if hierarchy.fleet() == Some(name) || !hierarchy.descendants(name)?.is_empty() {
                return Err(ResourceError::invalid(format!("Project `{name}` is the fleet or has descendants; reparent before deleting")));
            }
        }
        if let Some(records) = self.records() {
            if port_reads::get(&*records, name).await.map_err(ResourceError::other)?.is_none() {
                return Err(ResourceError::not_found(name));
            }
            return records.delete(name).await.map_err(ResourceError::other);
        }
        let local_root = self.backend.local_root()?;
        let sources = self.sources_for_name(name).await?;
        if sources.is_empty() {
            return Err(ResourceError::not_found(name));
        }
        let current = merge_sources(&sources)?;
        let context = causal_context(&sources);
        let next_counter = context.get(&local_root).copied().unwrap_or_default().saturating_add(1);
        let dot = CausalDot { author_root: local_root.clone(), author_counter: next_counter };
        let mut merge = current.metadata.merge.clone().unwrap_or_else(|| MergeMetadata {
            fields: BTreeMap::new(),
            seen: BTreeMap::new(),
            conflicts: BTreeMap::new(),
        });
        merge.fields.insert(
            DELETION_FIELD.to_string(),
            FieldMergeMetadata {
                dot,
                seen: context.clone(),
                written_at: Utc::now(),
                writer: Some(WriterIdentity::operator().with_source("definition-delete")),
            },
        );
        merge.seen = context;
        merge.seen.insert(local_root, next_counter);
        merge.conflicts.clear();
        let mut meta = InputMeta::from(&current.metadata);
        meta.deletion_timestamp = Some(Utc::now());

        match self.backend.using::<T>(&self.namespace).get(name).await {
            Ok(local) => {
                self.backend
                    .update_definition_typed::<T>(&self.namespace, &meta, &local.metadata.resource_version, &current.spec, merge)
                    .await?;
            }
            Err(ResourceError::NotFound { .. }) => {
                self.backend.create_definition_typed::<T>(&self.namespace, &meta, &current.spec, merge).await?;
            }
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// Update definition metadata without replaying or re-authoring its spec.
    pub async fn update_metadata(&self, meta: &InputMeta) -> Result<ResourceObject<T>, ResourceError> {
        ensure_definitions::<T>()?;
        refuse_builtin::<T>(&meta.name)?;
        self.refuse_write_elsewhere()?;
        let _hierarchy_admission = self.backend.hierarchy_admission(T::API_PATHS.kind).await;
        // The backend writes the read spec along with metadata. Serialize this
        // read/write pair with reparenting to avoid restoring a stale parent.
        let current = self.get(&meta.name).await?;
        if let Some(records) = self.records() {
            // The engine keeps an origin, not metadata: the stamps that name it are written as it;
            // any others (the manifest's hashes, now the root's records) are not kept.
            return self.engine_apply(&*records, meta, &current.spec).await;
        }
        let merge = current.metadata.merge.clone().unwrap_or_else(|| MergeMetadata {
            fields: BTreeMap::new(),
            seen: BTreeMap::new(),
            conflicts: BTreeMap::new(),
        });
        match self.backend.using::<T>(&self.namespace).get(&meta.name).await {
            Ok(local) => {
                self.backend
                    .update_definition_typed::<T>(&self.namespace, meta, &local.metadata.resource_version, &current.spec, merge)
                    .await?;
            }
            Err(ResourceError::NotFound { .. }) => {
                self.backend.create_definition_typed::<T>(&self.namespace, meta, &current.spec, merge).await?;
            }
            Err(error) => return Err(error),
        }
        self.get(&meta.name).await
    }

    async fn sources_for_name(&self, name: &str) -> Result<Vec<DefinitionSource<T>>, ResourceError> {
        let local_root = self.backend.local_root()?;
        let mut sources = match self.backend.using::<T>(&self.namespace).get(name).await {
            Ok(object) => vec![DefinitionSource { object, origin: local_root, synced_at: None }],
            Err(ResourceError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(error),
        };
        let replicas = { self.backend.get_replicas_typed::<T>(&self.namespace, name).await? };
        sources.extend(replicas.into_iter().filter_map(|replica| match replica.provenance {
            ResourceProvenance::Replica { origin_root, last_synced_at } => {
                Some(DefinitionSource { object: replica.object, origin: origin_root, synced_at: Some(last_synced_at) })
            }
            ResourceProvenance::Local => None,
        }));
        Ok(sources)
    }

    async fn sources(&self) -> Result<Vec<DefinitionSource<T>>, ResourceError> {
        let local_root = self.backend.local_root()?;
        let local = self.backend.using::<T>(&self.namespace).list().await?;
        let mut sources = local
            .items
            .into_iter()
            .map(|object| DefinitionSource { object, origin: local_root.clone(), synced_at: None })
            .collect::<Vec<_>>();
        let replicas = { self.backend.list_replicas_typed::<T>(&self.namespace).await? };
        sources.extend(replicas.into_iter().filter_map(|replica| match replica.provenance {
            ResourceProvenance::Replica { origin_root, last_synced_at } => {
                Some(DefinitionSource { object: replica.object, origin: origin_root, synced_at: Some(last_synced_at) })
            }
            ResourceProvenance::Local => None,
        }));
        Ok(sources)
    }
}

/// Merge a cached set of definition sources using the same causal semantics as
/// the store-backed resolver. No backing-store reads are needed.
pub(crate) fn merge_cached_definitions<T: Resource>(
    items: Vec<crate::ReadResourceObject<T>>,
    local_root: &NodeId,
) -> Result<Vec<ResourceObject<T>>, ResourceError> {
    let mut by_name = BTreeMap::<(String, String), Vec<DefinitionSource<T>>>::new();
    for item in items {
        let (origin, synced_at) = match item.provenance {
            ResourceProvenance::Local => (local_root.clone(), None),
            ResourceProvenance::Replica { origin_root, last_synced_at } => (origin_root, Some(last_synced_at)),
        };
        by_name.entry((item.object.metadata.namespace.clone(), item.object.metadata.name.clone())).or_default().push(DefinitionSource {
            object: item.object,
            origin,
            synced_at,
        });
    }
    by_name
        .into_values()
        .map(|sources| merge_sources(&sources))
        .collect::<Result<Vec<_>, _>>()
        .map(|objects| objects.into_iter().filter(definition_is_visible).collect())
}

/// One definition's copies, each with the home that wrote it, merged as every reader of the old
/// store merges them: what the cut-over import (#3824 layer 2b) carries onto the engine. The merge
/// metadata on the result names any field still in conflict.
pub fn merge_definition_copies<T: Resource>(copies: Vec<(ResourceObject<T>, NodeId)>) -> Result<ResourceObject<T>, ResourceError> {
    let sources: Vec<_> = copies.into_iter().map(|(object, origin)| DefinitionSource { object, origin, synced_at: None }).collect();
    merge_sources(&sources)
}

/// Whether a merged definition reads as live: not deleted, or deleted while the deletion itself is
/// in conflict.
pub fn definition_is_live<T: Resource>(object: &ResourceObject<T>) -> bool {
    definition_is_visible(object)
}

/// The built-in definitions of `kind` (#3824 layer 2a) as objects of `T` in `namespace`: never
/// stored, so their version is fixed and they carry no merge metadata.
pub(crate) fn builtin_objects<T: Resource>(kind: &str, namespace: &str) -> Result<Vec<ResourceObject<T>>, ResourceError> {
    flotilla_resources::builtins::builtin_definitions(kind)
        .map_err(ResourceError::invalid)?
        .iter()
        .map(|builtin| {
            let spec = serde_json::from_value::<T::Spec>(builtin.spec.clone())
                .map_err(|error| ResourceError::decode(format!("built-in {kind} `{}`: {error}", builtin.name)))?;
            Ok(ResourceObject {
                metadata: crate::ObjectMeta {
                    name: builtin.name.clone(),
                    namespace: namespace.to_owned(),
                    resource_version: BUILTIN_VERSION.to_owned(),
                    labels: builtin.labels.clone(),
                    annotations: BTreeMap::new(),
                    owner_references: Vec::new(),
                    finalizers: Vec::new(),
                    deletion_timestamp: None,
                    creation_timestamp: DateTime::<Utc>::UNIX_EPOCH,
                    merge: None,
                },
                spec,
                status: None,
            })
        })
        .collect()
}

/// The version every built-in reads at.
pub const BUILTIN_VERSION: &str = "builtin";

/// `merged` followed by `kind`'s built-ins, those in `names` when the read selected names. No
/// store holds a built-in's name: every write of one is refused, and the import drops them.
pub(crate) fn with_builtins<T: Resource>(
    kind: &str,
    namespace: &str,
    mut merged: Vec<ResourceObject<T>>,
    names: Option<&std::collections::BTreeSet<String>>,
) -> Result<Vec<ResourceObject<T>>, ResourceError> {
    let builtins = builtin_objects::<T>(kind, namespace)?;
    merged.extend(builtins.into_iter().filter(|builtin| names.is_none_or(|names| names.contains(&builtin.metadata.name))));
    Ok(merged)
}

/// Refuses a write of a built-in's name: built-ins come from the code (#3824 layer 2a).
pub(crate) fn refuse_builtin<T: Resource>(name: &str) -> Result<(), ResourceError> {
    if flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, name) {
        return Err(ResourceError::invalid(format!(
            "{} `{name}` is built in and read-only; declare a variant under a new name",
            T::API_PATHS.kind
        )));
    }
    Ok(())
}

fn definition_is_visible<T: Resource>(object: &ResourceObject<T>) -> bool {
    let deleted = object
        .metadata
        .merge
        .as_ref()
        .and_then(|merge| merge.fields.get(DELETION_FIELD))
        .is_some_and(|_| object.metadata.deletion_timestamp.is_some());
    !deleted || object.metadata.merge.as_ref().is_some_and(|merge| merge.conflicts.contains_key(DELETION_FIELD))
}

/// The engine reads `query` selects (#3824 layer 2b-ii): this home's alone unless the query takes
/// replicas, and the built-ins only for a merged read.
pub(crate) fn select_reads<T: Resource>(
    reads: Vec<ReadResourceObject<T>>,
    query: &crate::StoreQuery,
) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
    let reads: Vec<_> = reads
        .into_iter()
        .filter(|read| query.include_replicas || matches!(read.provenance, ResourceProvenance::Local))
        .filter(|read| query.merged_definitions || !flotilla_resources::builtins::is_builtin(T::API_PATHS.kind, &read.object.metadata.name))
        .collect();
    let mut index = crate::QueryIndex::default();
    let mut messages = BTreeSet::new();
    for read in &reads {
        if query.message.as_ref().map_or(Ok(true), |message| message.selects(&read.object))? {
            messages.insert(read.object.metadata.name.clone());
        }
        index.update_object(&read.object.metadata.name, Some(&read.object))?;
    }
    let selected = index.names(query);
    Ok(reads
        .into_iter()
        .filter(|read| selected.contains(&read.object.metadata.name) && messages.contains(&read.object.metadata.name))
        .collect())
}

/// An engine record as a resource in `namespace`: the labels and annotations its port rebuilds,
/// its creation time, and no merge metadata, owner references or finalizers.
pub(crate) fn record_object<T: Resource>(namespace: &str, record: Record<T>) -> ResourceObject<T> {
    ResourceObject {
        metadata: ObjectMeta {
            name: record.name,
            namespace: namespace.to_owned(),
            resource_version: record.version.to_string(),
            labels: record.labels,
            annotations: record.annotations,
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: record.created_at,
            merge: None,
        },
        spec: record.spec,
        status: record.status,
    }
}

/// What a watcher last read of each name, and the record it was read from; the built-ins have none.
/// One watcher's place in its kind's engine listings: the reads it has converted and the events
/// of the last listing it has not yet handed on.
struct EngineWatch<T: Resource> {
    feed: std::sync::Arc<crate::engine_feed::EngineFeed<T>>,
    serials: tokio::sync::watch::Receiver<u64>,
    /// The serial of the feed's last batch taken.
    serial: u64,
    seen: SeenReads<T>,
    /// The feed's position that `seen` holds the records at.
    position: String,
    pending: VecDeque<Result<Fed<T>, ResourceError>>,
    resolver: DefinitionResolver<T>,
    home: String,
}

/// What an engine watch sends: an event, or a bookmark naming the position its events so far
/// bring a reader to, which a watch resumes from as from a listing's.
// The event is moved through as every watch stream carries it; boxing would allocate per event.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Fed<T: Resource> {
    Event(ReadWatchEvent<T>),
    Bookmark(String),
}

/// An engine watch's events and bookmarks.
pub(crate) type FedStream<T> = BoxStream<'static, Result<Fed<T>, ResourceError>>;

/// `stream`'s events alone, for a reader that never resumes.
pub(crate) fn events_of<T: Resource>(stream: FedStream<T>) -> BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>> {
    stream
        .filter_map(|fed| {
            std::future::ready(match fed {
                Ok(Fed::Event(event)) => Some(Ok(event)),
                Ok(Fed::Bookmark(_)) => None,
                Err(error) => Some(Err(error)),
            })
        })
        .boxed()
}

impl<T: Resource> EngineWatch<T> {
    /// Each event, as the watcher takes the feed's batches: the names a batch read again are
    /// converted, and every record is compared again after a fresh listing or batches missed.
    /// Each batch taken ends with a bookmark at the feed's position after it.
    fn stream(self) -> FedStream<T> {
        futures::stream::unfold(self, |mut watch| async move {
            loop {
                if let Some(event) = watch.pending.pop_front() {
                    return Some((event, watch));
                }
                watch.serials.changed().await.ok()?;
                watch.take();
            }
        })
        .boxed()
    }

    /// The events of the batches after the last taken.
    fn take(&mut self) {
        let EngineWatch { feed, serial, seen, position, pending, resolver, home, .. } = self;
        let state = feed.state();
        if state.serial == *serial {
            return;
        }
        let mut names = BTreeSet::new();
        let mut whole = false;
        match state.after(*serial) {
            None => whole = true,
            Some(batches) => {
                for batch in batches {
                    match batch {
                        crate::engine_feed::Batch::Changed(changed) => names.extend(changed.iter().cloned()),
                        crate::engine_feed::Batch::Relisted => whole = true,
                        crate::engine_feed::Batch::Failed(error) => pending.push_back(Err(ResourceError::other(error.clone()))),
                    }
                }
            }
        }
        let events = match whole {
            true => resolver.diff(home, &state.records, seen),
            false => Ok(resolver.diff_names(home, &state.records, &names, seen)),
        };
        *serial = state.serial;
        match events {
            Ok(events) => {
                pending.extend(events.into_iter().map(|event| Ok(Fed::Event(event))));
                *position = state.position.encode();
                pending.push_back(Ok(Fed::Bookmark(position.clone())));
            }
            Err(error) => pending.push_back(Err(error)),
        }
    }
}

struct SeenReads<T: Resource> {
    reads: BTreeMap<String, (Option<RecordStamp>, ReadResourceObject<T>)>,
}

impl<T: Resource> Default for SeenReads<T> {
    fn default() -> Self {
        Self { reads: BTreeMap::new() }
    }
}

/// The parts of a record that say whether a read of it is still current.
#[derive(Debug, PartialEq)]
struct RecordStamp {
    home: String,
    version: u64,
    labels: BTreeMap<String, String>,
    annotations: BTreeMap<String, String>,
}

impl RecordStamp {
    fn of<T: Resource>(record: &Record<T>) -> Self {
        Self { home: record.home.clone(), version: record.version, labels: record.labels.clone(), annotations: record.annotations.clone() }
    }
}

/// Whether `read` shows a change from `before`: its version, labels or annotations moved.
fn changed<T: Resource>(before: &ReadResourceObject<T>, read: &ReadResourceObject<T>) -> bool {
    before.object.metadata.resource_version != read.object.metadata.resource_version
        || before.object.metadata.labels != read.object.metadata.labels
        || before.object.metadata.annotations != read.object.metadata.annotations
}

/// A copy of `read`; `ReadResourceObject` is `Clone` only for `Clone` kinds.
fn clone_read<T: Resource>(read: &ReadResourceObject<T>) -> ReadResourceObject<T> {
    ReadResourceObject {
        object: ResourceObject {
            metadata: read.object.metadata.clone(),
            spec: read.object.spec.clone(),
            status: read.object.status.clone(),
        },
        provenance: read.provenance.clone(),
    }
}

/// The objects among `objects` that `query` selects, through the store's in-memory index.
fn select<T: Resource>(objects: Vec<ResourceObject<T>>, query: &crate::StoreQuery) -> Result<Vec<ResourceObject<T>>, ResourceError> {
    let mut index = crate::QueryIndex::default();
    for object in &objects {
        index.update_object(&object.metadata.name, Some(object))?;
    }
    let selected = index.names(query);
    Ok(objects.into_iter().filter(|object| selected.contains(&object.metadata.name)).collect())
}

fn ensure_definitions<T: Resource>() -> Result<(), ResourceError> {
    if T::REPLICATION_CLASS == ReplicationClass::Definitions {
        Ok(())
    } else {
        Err(ResourceError::invalid(format!("{} is not a definitions-class resource", T::API_PATHS.kind)))
    }
}

fn spec_object<T: serde::Serialize>(spec: &T) -> Result<Map<String, Value>, ResourceError> {
    serde_json::to_value(spec)
        .map_err(|error| ResourceError::decode(format!("encode definition spec: {error}")))?
        .as_object()
        .cloned()
        .ok_or_else(|| ResourceError::decode("definition spec is not a JSON object"))
}

fn causal_context<T: Resource>(sources: &[DefinitionSource<T>]) -> BTreeMap<NodeId, u64> {
    let mut context = BTreeMap::new();
    for source in sources {
        if let Some(merge) = &source.object.metadata.merge {
            merge_vector(&mut context, &merge.seen);
            for field in merge.fields.values() {
                merge_counter(&mut context, &field.dot.author_root, field.dot.author_counter);
            }
        } else {
            merge_counter(&mut context, &source.origin, legacy_counter(&source.object));
        }
    }
    context
}

fn merge_sources<T: Resource>(sources: &[DefinitionSource<T>]) -> Result<ResourceObject<T>, ResourceError> {
    let context = causal_context(sources);
    let mut sources = sources.iter().collect::<Vec<_>>();
    sources.sort_by(|left, right| left.origin.cmp(&right.origin));
    let mut field_candidates = BTreeMap::<String, Vec<FieldCandidate>>::new();
    for source in &sources {
        let spec = spec_object(&source.object.spec)?;
        for (field, value) in spec {
            let path = format!("spec.{field}");
            let metadata = source
                .object
                .metadata
                .merge
                .as_ref()
                .and_then(|merge| merge.fields.get(&path))
                .cloned()
                .unwrap_or_else(|| legacy_field(source));
            field_candidates.entry(path).or_default().push(FieldCandidate { value, metadata });
        }
        let metadata = source
            .object
            .metadata
            .merge
            .as_ref()
            .and_then(|merge| merge.fields.get(DELETION_FIELD))
            .cloned()
            .unwrap_or_else(|| legacy_field(source));
        field_candidates
            .entry(DELETION_FIELD.to_string())
            .or_default()
            .push(FieldCandidate { value: Value::Bool(source.object.metadata.deletion_timestamp.is_some()), metadata });
    }

    let mut merged_fields = BTreeMap::new();
    let mut conflicts = BTreeMap::new();
    let mut spec = Map::new();
    let mut deleted_at = None;
    for (path, candidates) in field_candidates {
        let maximal = causally_maximal(candidates);
        let mut distinct = BTreeMap::<String, FieldCandidate>::new();
        for candidate in maximal {
            let key = serde_json::to_string(&candidate.value)
                .map_err(|error| ResourceError::decode(format!("encode merged definition field: {error}")))?;
            distinct.entry(key).or_insert(candidate);
        }
        let mut values = distinct.into_values().collect::<Vec<_>>();
        values.sort_by(|left, right| left.metadata.dot.cmp(&right.metadata.dot));
        let chosen = values.first().cloned().ok_or_else(|| ResourceError::decode("definition field has no maximal value"))?;
        if values.len() > 1 {
            conflicts.insert(
                path.clone(),
                values
                    .iter()
                    .map(|candidate| MergeConflictSibling {
                        value: candidate.value.clone(),
                        dot: candidate.metadata.dot.clone(),
                        written_at: candidate.metadata.written_at,
                    })
                    .collect(),
            );
        }
        let chosen_written_at = chosen.metadata.written_at;
        merged_fields.insert(path.clone(), chosen.metadata);
        if path == DELETION_FIELD {
            deleted_at = chosen.value.as_bool().unwrap_or(false).then_some(chosen_written_at);
        } else if let Some(field) = path.strip_prefix("spec.") {
            spec.insert(field.to_string(), chosen.value);
        }
    }

    let source = sources.first().ok_or_else(|| ResourceError::decode("cannot merge an empty definition source set"))?;
    let mut object = ResourceObject::<T> {
        metadata: source.object.metadata.clone(),
        spec: source.object.spec.clone(),
        status: source.object.status.clone(),
    };
    object.spec = serde_json::from_value(Value::Object(spec))
        .map_err(|error| ResourceError::decode(format!("decode merged definition spec: {error}")))?;
    object.metadata.resource_version =
        sources.iter().map(|source| format!("{}:{}", source.origin, source.object.metadata.resource_version)).collect::<Vec<_>>().join(",");
    object.metadata.deletion_timestamp = deleted_at;
    object.metadata.merge = Some(MergeMetadata { fields: merged_fields, seen: context, conflicts });
    Ok(object)
}

fn causally_maximal(candidates: Vec<FieldCandidate>) -> Vec<FieldCandidate> {
    candidates
        .iter()
        .enumerate()
        .filter(|(index, candidate)| {
            !candidates.iter().enumerate().any(|(other_index, other)| {
                *index != other_index
                    && other.metadata.dot != candidate.metadata.dot
                    && covers(&other.metadata.seen, &candidate.metadata.dot)
            })
        })
        .map(|(_, candidate)| candidate.clone())
        .collect()
}

fn covers(seen: &BTreeMap<NodeId, u64>, dot: &CausalDot) -> bool {
    seen.get(&dot.author_root).is_some_and(|counter| *counter >= dot.author_counter)
}

/// The dot of a field a copy carries no merge entry for: its copy's version at its origin. The
/// 73aa22b9b store holds this shape (Project, WorkflowTemplate and ImageLayer spec fields added
/// after their copies were written); goes with the import.
fn legacy_field<T: Resource>(source: &DefinitionSource<T>) -> FieldMergeMetadata {
    FieldMergeMetadata {
        dot: CausalDot { author_root: source.origin.clone(), author_counter: legacy_counter(&source.object) },
        seen: BTreeMap::new(),
        written_at: source.synced_at.unwrap_or(source.object.metadata.creation_timestamp),
        writer: None,
    }
}

fn legacy_counter<T: Resource>(object: &ResourceObject<T>) -> u64 {
    object.metadata.resource_version.parse().unwrap_or(1)
}

fn merge_vector(target: &mut BTreeMap<NodeId, u64>, source: &BTreeMap<NodeId, u64>) {
    for (root, counter) in source {
        merge_counter(target, root, *counter);
    }
}

fn merge_counter(target: &mut BTreeMap<NodeId, u64>, root: &NodeId, counter: u64) {
    target.entry(root.clone()).and_modify(|current| *current = (*current).max(counter)).or_insert(counter);
}
