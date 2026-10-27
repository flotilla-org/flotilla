use crate::StoreExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    marker::PhantomData,
    sync::Arc,
};

use chrono::{DateTime, Utc};
use flotilla_protocol::NodeId;
use flotilla_resources::{
    engine_records::{EngineObject, EngineRecords},
    ObjectMeta, ReadResourceObject, ReadWatchEvent,
};
use futures::{stream::BoxStream, StreamExt};
use serde_json::{Map, Value};

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
    fn records(&self) -> Option<Arc<dyn EngineRecords>> {
        self.backend.engine_records(T::API_PATHS.kind)
    }

    /// An engine record as the old store's object: the stamps its port rebuilds, and no merge
    /// metadata, owner references or finalizers. The engine keeps no creation time.
    fn object(&self, record: EngineObject) -> Result<ResourceObject<T>, ResourceError> {
        engine_object(&self.namespace, record)
    }

    /// Every engine record of this kind, with the built-ins.
    async fn engine_list(&self, records: &dyn EngineRecords) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        let listed = records.list(T::API_PATHS.kind).await.map_err(ResourceError::other)?;
        let objects = listed.into_iter().map(|record| self.object(record)).collect::<Result<Vec<_>, _>>()?;
        overlay_builtins(T::API_PATHS.kind, &self.namespace, objects, None)
    }

    /// The engine's records of this kind as replica reads, every home's copy with its home as
    /// provenance, and the built-ins as local; `None` unless the kind is served by the engine.
    pub(crate) async fn engine_reads(&self) -> Option<Result<Vec<ReadResourceObject<T>>, ResourceError>> {
        let records = self.records()?;
        Some(self.read_all(&*records).await)
    }

    async fn read_all(&self, records: &dyn EngineRecords) -> Result<Vec<ReadResourceObject<T>>, ResourceError> {
        let home = records.home();
        // Every home's copy, as the old store's replica reads return each origin's.
        let listed = records.sources(T::API_PATHS.kind).await.map_err(ResourceError::other)?;
        let mut reads = Vec::with_capacity(listed.len());
        for record in listed {
            let provenance = if record.home == home {
                ResourceProvenance::Local
            } else {
                ResourceProvenance::Replica { origin_root: NodeId::new(record.home.clone()), last_synced_at: Utc::now() }
            };
            reads.push(ReadResourceObject { object: self.object(record)?, provenance });
        }
        let builtins = overlay_builtins(T::API_PATHS.kind, &self.namespace, Vec::new(), None)?;
        reads.extend(builtins.into_iter().map(|object| ReadResourceObject { object, provenance: ResourceProvenance::Local }));
        Ok(reads)
    }

    /// The engine's records of this kind as a watch: the current reads, then each change of any
    /// home's records as the difference from the reads before it. `None` unless the kind is served
    /// by the engine.
    #[allow(clippy::type_complexity)]
    pub(crate) async fn engine_watch(
        &self,
    ) -> Option<Result<(Vec<ReadResourceObject<T>>, BoxStream<'static, Result<ReadWatchEvent<T>, ResourceError>>), ResourceError>> {
        let records = self.records()?;
        // Subscribe before listing, so no change between the two is lost.
        let changes = match records.changes().await {
            Ok(changes) => changes,
            Err(error) => return Some(Err(ResourceError::other(error))),
        };
        let initial = match self.read_all(&*records).await {
            Ok(initial) => initial,
            Err(error) => return Some(Err(error)),
        };
        let previous: BTreeMap<String, ReadResourceObject<T>> =
            initial.iter().map(|read| (read.object.metadata.name.clone(), clone_read(read))).collect();
        let resolver = Self::new(self.backend.clone(), self.namespace.clone());
        let state = (resolver, records, changes, previous, std::collections::VecDeque::new());
        let stream = futures::stream::unfold(state, |(resolver, records, mut changes, mut previous, mut pending)| async move {
            loop {
                if let Some(event) = pending.pop_front() {
                    return Some((Ok(event), (resolver, records, changes, previous, pending)));
                }
                changes.next().await?;
                let current = match resolver.read_all(&*records).await {
                    Ok(current) => current,
                    Err(error) => return Some((Err(error), (resolver, records, changes, previous, pending))),
                };
                let mut next = BTreeMap::new();
                for read in current {
                    let name = read.object.metadata.name.clone();
                    match previous.remove(&name) {
                        None => pending.push_back(ReadWatchEvent::Added(clone_read(&read))),
                        Some(before)
                            if before.object.metadata.resource_version != read.object.metadata.resource_version
                                || before.object.metadata.labels != read.object.metadata.labels
                                || before.object.metadata.annotations != read.object.metadata.annotations =>
                        {
                            pending.push_back(ReadWatchEvent::Modified(clone_read(&read)))
                        }
                        Some(_) => {}
                    }
                    next.insert(name, read);
                }
                pending.extend(previous.into_values().map(ReadWatchEvent::Deleted));
                previous = next;
            }
        })
        .boxed();
        Some(Ok((initial, stream)))
    }

    /// Writes `spec` under `meta`'s name at this home, with the origin `meta`'s stamps name.
    async fn engine_apply(
        &self,
        records: &dyn EngineRecords,
        meta: &InputMeta,
        spec: &T::Spec,
    ) -> Result<ResourceObject<T>, ResourceError> {
        let value = serde_json::to_value(spec).map_err(|error| ResourceError::decode(error.to_string()))?;
        let record =
            records.apply(T::API_PATHS.kind, &meta.name, &meta.labels, &meta.annotations, value).await.map_err(ResourceError::other)?;
        self.object(record)
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
            let record = records.get(T::API_PATHS.kind, name).await.map_err(ResourceError::other)?;
            return record.map(|record| self.object(record)).unwrap_or_else(|| Err(ResourceError::not_found(name)));
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
        if let Some(records) = self.records() {
            return select(self.engine_list(&*records).await?, query);
        }
        let hydration =
            crate::StoreQuery { all_sources_for_matches: true, include_replicas: true, suppress_self_origin: false, ..query.clone() };
        let result = self.backend.store().query_decoded(crate::ResourceKind::of::<T>(), &self.namespace, &hydration).await?;
        let root = result.local_root.clone().ok_or_else(|| ResourceError::invalid("definition query has no authority root"))?;
        let merged =
            overlay_builtins(T::API_PATHS.kind, &self.namespace, merge_cached_definitions(result.into_typed::<T>()?, &root)?, None)?;
        select(merged, query)
    }

    pub async fn list(&self) -> Result<Vec<ResourceObject<T>>, ResourceError> {
        ensure_definitions::<T>()?;
        if let Some(records) = self.records() {
            return self.engine_list(&*records).await;
        }
        if self.backend.serves_read_views() {
            let listed = self.backend.using::<T>(&self.namespace).list().await?.items;
            return overlay_builtins(T::API_PATHS.kind, &self.namespace, listed, None);
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
        overlay_builtins(T::API_PATHS.kind, &self.namespace, merged, None)
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
            let stored = records.list(T::API_PATHS.kind).await.map_err(ResourceError::other)?;
            let objects = stored.into_iter().map(|record| self.object(record)).collect::<Result<Vec<_>, _>>()?;
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
        let _hierarchy_admission = self.backend.hierarchy_admission(T::API_PATHS.kind).await;
        if let Some(records) = self.records() {
            if records.get(T::API_PATHS.kind, name).await.map_err(ResourceError::other)?.is_none() {
                return Err(ResourceError::not_found(name));
            }
            return records.delete(T::API_PATHS.kind, name).await.map_err(ResourceError::other);
        }
        if T::API_PATHS.kind == "Project" {
            let hierarchy = crate::ProjectHierarchy::load_for_inspection(&self.backend, &self.namespace).await?;
            if hierarchy.fleet() == Some(name) || !hierarchy.descendants(name)?.is_empty() {
                return Err(ResourceError::invalid(format!("Project `{name}` is the fleet or has descendants; reparent before deleting")));
            }
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

/// `merged` with `kind`'s built-ins in place of any stored definition of their names (a copy an
/// earlier generation seeded), restricted to `names` when the read selected names.
pub(crate) fn overlay_builtins<T: Resource>(
    kind: &str,
    namespace: &str,
    mut merged: Vec<ResourceObject<T>>,
    names: Option<&std::collections::BTreeSet<String>>,
) -> Result<Vec<ResourceObject<T>>, ResourceError> {
    let builtins = builtin_objects::<T>(kind, namespace)?;
    if builtins.is_empty() {
        return Ok(merged);
    }
    merged.retain(|object| !builtins.iter().any(|builtin| builtin.metadata.name == object.metadata.name));
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
    for read in &reads {
        index.update(
            &read.object.metadata.name,
            Some(&serde_json::to_value(read.object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?),
        );
    }
    let selected = index.names(query);
    Ok(reads.into_iter().filter(|read| selected.contains(&read.object.metadata.name)).collect())
}

/// An engine record as the old store's object in `namespace`: the labels and annotations its
/// port rebuilds, and no merge metadata, owner references or finalizers. The engine keeps no
/// creation time, so it reads as the epoch.
pub(crate) fn engine_object<T: Resource>(namespace: &str, record: EngineObject) -> Result<ResourceObject<T>, ResourceError> {
    let decode =
        |what: &str, error: serde_json::Error| ResourceError::decode(format!("{} {} {what}: {error}", T::API_PATHS.kind, record.name));
    let spec = serde_json::from_value(record.spec.clone()).map_err(|error| decode("spec", error))?;
    let status = record.status.clone().map(serde_json::from_value).transpose().map_err(|error| decode("status", error))?;
    Ok(ResourceObject {
        metadata: ObjectMeta {
            name: record.name,
            namespace: namespace.to_owned(),
            resource_version: record.version.to_string(),
            labels: record.labels,
            annotations: record.annotations,
            owner_references: Vec::new(),
            finalizers: Vec::new(),
            deletion_timestamp: None,
            creation_timestamp: DateTime::<Utc>::UNIX_EPOCH,
            merge: None,
        },
        spec,
        status,
    })
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
        index.update(
            &object.metadata.name,
            Some(&serde_json::to_value(object.to_k8s_object()).map_err(|error| ResourceError::decode(error.to_string()))?),
        );
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
