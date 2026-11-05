//! A daemon composed with no old resource store (#3108): the fleet on the engine store alone.
//!
//! [`OldStore`] stands where `resources.sqlite` stands, and records every access to it in
//! [`OldStoreAccesses`] with the scenario's current stage and where the access came from.
//! Unavailable, it refuses each access with [`OLD_STORE_UNAVAILABLE`], so a scenario fails at
//! its next step naming the access rather than hanging on a retry loop that swallowed the error.
//! Recording, it serves each access from an in-memory store as well, so one run lists every
//! access a scenario still makes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    sync::{Arc, Mutex},
};

use flotilla_protocol::NodeId;
use flotilla_resources::{
    DigestQuery, FieldOwnershipViolation, InputMeta, MergeMetadata, PartitionDigest, ReadResourceList, ReadResourceObject, ReadWatchEvent,
    ReplicaCursor, ResourceError, ResourceList, ResourceObject, ResourcePosition, ResourceStoreDiagnostics, WatchStart,
};
use flotilla_store::{
    replica::ReplicaDeletion, watch::TombstoneWrite, DecodedList, DecodedObject, DecodedQueryResult, DecodedReadObject, MessageQuery,
    QueryResult, ReadObserver, ResourceKind, SerializedReadWatchEvent, SerializedResource, SerializedWatchStream, Store, StoreQuery,
};
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::Notify;

/// What stands where the old resource store stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OldStoreMode {
    /// An in-memory old store, as before the cut-over.
    #[default]
    Available,
    /// No old store: every access is refused and fails the scenario.
    Unavailable,
    /// An in-memory old store whose every access is recorded.
    Recording,
}

/// The text every refused access carries.
pub const OLD_STORE_UNAVAILABLE: &str = "old resource store unavailable";

/// One access of the old store.
#[derive(Debug, Clone)]
pub struct Access {
    pub stage: String,
    pub operation: &'static str,
    pub kind: &'static str,
    pub target: String,
    /// Where the first access of this operation and kind came from.
    pub backtrace: Option<String>,
}

/// The accesses an [`OldStore`] saw, shared by every handle derived from it.
#[derive(Debug)]
pub struct OldStoreAccesses {
    accesses: Mutex<Vec<Access>>,
    seen: Mutex<BTreeSet<(&'static str, &'static str)>>,
    stage: Mutex<String>,
    first: Notify,
}

impl Default for OldStoreAccesses {
    fn default() -> Self {
        Self { accesses: Mutex::default(), seen: Mutex::default(), stage: Mutex::new("startup".into()), first: Notify::new() }
    }
}

impl OldStoreAccesses {
    fn record(&self, operation: &'static str, kind: &'static str, target: String) -> ResourceError {
        // Held until the access is recorded, so an operation's first recorded access is the one
        // whose caller was captured, even when another arrives while it is captured.
        let mut seen = self.seen.lock().expect("old store accesses");
        let backtrace = seen.insert((operation, kind)).then(|| std::backtrace::Backtrace::force_capture().to_string());
        let error = ResourceError::other(format!("{OLD_STORE_UNAVAILABLE}: {operation} {kind} {target}"));
        let stage = self.stage.lock().expect("old store stage").clone();
        let mut accesses = self.accesses.lock().expect("old store accesses");
        accesses.push(Access { stage, operation, kind, target, backtrace });
        if accesses.len() == 1 {
            self.first.notify_waiters();
        }
        error
    }

    /// Names the scenario's stage, which each later access records.
    pub fn stage(&self, stage: &str) {
        *self.stage.lock().expect("old store stage") = stage.to_owned();
    }

    /// Every access so far.
    pub fn accesses(&self) -> Vec<Access> {
        self.accesses.lock().expect("old store accesses").clone()
    }

    /// Resolves once the store has been accessed at all.
    pub async fn first_access(&self) {
        loop {
            let notified = self.first.notified();
            if !self.accesses.lock().expect("old store accesses").is_empty() {
                return;
            }
            notified.await;
        }
    }

    /// The accesses as a report: by stage, each operation and kind with its count, then each
    /// operation and kind's first caller.
    pub fn report(&self) -> String {
        let accesses = self.accesses();
        let mut counts: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
        let mut order: Vec<&str> = Vec::new();
        for access in &accesses {
            if !order.contains(&access.stage.as_str()) {
                order.push(&access.stage);
            }
            *counts.entry((access.stage.as_str(), access.kind, access.operation)).or_default() += 1;
        }
        let mut report = format!("the old resource store was accessed {} times:\n", accesses.len());
        for stage in order {
            let _ = writeln!(report, "stage {stage}:");
            for ((_, kind, operation), count) in counts.iter().filter(|((at, _, _), _)| *at == stage) {
                let _ = writeln!(report, "  {kind} {operation}: {count}");
            }
        }
        for access in accesses.iter().filter(|access| access.backtrace.is_some()) {
            let _ = writeln!(report, "\nfirst {} {} ({}, stage {}):", access.kind, access.operation, access.target, access.stage);
            let frames = access.backtrace.as_deref().unwrap_or_default().lines().map(str::trim).filter(|line| {
                line.starts_with("at ") && line.contains("/crates/flotilla-") && !line.contains("flotilla-daemon-testkit/src/old_store.rs")
            });
            for line in frames.take(12) {
                let _ = writeln!(report, "  {}", line.trim_start_matches("at ").split("/crates/").last().unwrap_or(line));
            }
        }
        report
    }
}

/// The old resource store, unavailable or recording: see the module documentation.
#[derive(Debug, Clone)]
pub struct OldStore {
    accesses: Arc<OldStoreAccesses>,
    /// The store that serves recorded accesses; none when the old store is unavailable.
    inner: Option<Arc<dyn Store>>,
    root: Option<NodeId>,
}

impl OldStore {
    /// Refuses every access.
    pub fn unavailable(accesses: Arc<OldStoreAccesses>) -> Self {
        Self { accesses, inner: None, root: None }
    }

    /// Serves every access from `inner`, recording it.
    pub fn recording(accesses: Arc<OldStoreAccesses>, inner: Arc<dyn Store>) -> Self {
        Self { accesses, inner: Some(inner), root: None }
    }
}

/// Records the access, then serves it from the inner store or refuses it.
macro_rules! access {
    ($self:ident, $operation:literal, $kind:expr, $target:expr, $method:ident($($argument:expr),*)) => {{
        let refused = $self.accesses.record($operation, $kind, $target);
        match &$self.inner {
            Some(inner) => inner.$method($($argument),*).await,
            None => Err(refused),
        }
    }};
}

#[async_trait::async_trait]
impl Store for OldStore {
    fn unsupported_operation(&self, operation: &str) -> ResourceError {
        self.accesses.record("unsupported", "-", operation.to_owned())
    }
    fn with_local_root(&self, root: NodeId) -> Arc<dyn Store> {
        let inner = self.inner.as_ref().map(|inner| inner.with_local_root(root.clone()));
        Arc::new(Self { accesses: Arc::clone(&self.accesses), inner, root: Some(root) })
    }
    fn local_root(&self) -> Result<NodeId, ResourceError> {
        match &self.inner {
            Some(inner) => inner.local_root(),
            None => self.root.clone().ok_or_else(|| ResourceError::other("local root not set")),
        }
    }
    fn hierarchy_admission(&self) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.inner.as_ref().and_then(|inner| inner.hierarchy_admission())
    }
    fn with_read_observer(&self, observer: Arc<dyn ReadObserver>) -> Arc<dyn Store> {
        let inner = self.inner.as_ref().map(|inner| inner.with_read_observer(observer));
        Arc::new(Self { accesses: Arc::clone(&self.accesses), inner, root: self.root.clone() })
    }
    fn read_observer(&self) -> Option<&dyn ReadObserver> {
        self.inner.as_ref().and_then(|inner| inner.read_observer())
    }
    async fn diagnostics(&self) -> Result<Option<ResourceStoreDiagnostics>, ResourceError> {
        access!(self, "diagnostics", "-", String::new(), diagnostics())
    }
    async fn record_field_ownership_violation(&self, violation: FieldOwnershipViolation) -> Result<(), ResourceError> {
        access!(self, "record_field_ownership_violation", "-", format!("{violation:?}"), record_field_ownership_violation(violation))
    }
    async fn query_raw(&self, kind: ResourceKind, namespace: &str, query: &StoreQuery) -> Result<QueryResult, ResourceError> {
        access!(self, "query", kind.paths.kind, namespace.to_owned(), query_raw(kind, namespace, query))
    }
    async fn query_values_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query: &StoreQuery,
        field: &str,
    ) -> Result<BTreeSet<String>, ResourceError> {
        access!(self, "query_values", kind.paths.kind, namespace.to_owned(), query_values_raw(kind, namespace, query, field))
    }
    async fn query_messages_raw(
        &self,
        namespace: &str,
        query: &MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        access!(self, "query_messages", "Message", namespace.to_owned(), query_messages_raw(namespace, query, include_replicas))
    }
    async fn query_message_replicas_raw(
        &self,
        namespace: &str,
        query: &MessageQuery,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        access!(self, "query_message_replicas", "Message", namespace.to_owned(), query_message_replicas_raw(namespace, query))
    }
    async fn delete_decode_quarantine_raw(&self, kind: ResourceKind, namespace: &str, name: &str) -> Result<bool, ResourceError> {
        access!(
            self,
            "delete_decode_quarantine",
            kind.paths.kind,
            format!("{namespace}/{name}"),
            delete_decode_quarantine_raw(kind, namespace, name)
        )
    }
    async fn namespace_inventory(&self) -> Result<BTreeMap<String, Vec<String>>, ResourceError> {
        access!(self, "namespace_inventory", "-", String::new(), namespace_inventory())
    }
    async fn local_namespaces_raw(&self, kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        access!(self, "local_namespaces", kind.paths.kind, String::new(), local_namespaces_raw(kind))
    }
    async fn stored_namespaces_raw(&self, kind: ResourceKind) -> Result<Vec<String>, ResourceError> {
        access!(self, "stored_namespaces", kind.paths.kind, String::new(), stored_namespaces_raw(kind))
    }
    async fn list_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        access!(self, "list_replicas", kind.paths.kind, namespace.to_owned(), list_replicas_raw(kind, namespace))
    }
    async fn get_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<Vec<ReadResourceObject<SerializedResource>>, ResourceError> {
        access!(self, "get_replicas", kind.paths.kind, format!("{namespace}/{name}"), get_replicas_raw(kind, namespace, name))
    }
    async fn watch_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
    ) -> Result<BoxStream<'static, Result<SerializedReadWatchEvent, ResourceError>>, ResourceError> {
        access!(self, "watch_replicas", kind.paths.kind, namespace.to_owned(), watch_replicas_raw(kind, namespace))
    }
    async fn invalidate_replica_cursor_raw(&self, kind: ResourceKind, origin: &NodeId, namespace: &str) -> Result<(), ResourceError> {
        access!(
            self,
            "invalidate_replica_cursor",
            kind.paths.kind,
            namespace.to_owned(),
            invalidate_replica_cursor_raw(kind, origin, namespace)
        )
    }
    async fn advance_replica_cursor_raw(
        &self,
        kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        previous: &ReplicaCursor,
        next: &str,
    ) -> Result<(), ResourceError> {
        access!(
            self,
            "advance_replica_cursor",
            kind.paths.kind,
            namespace.to_owned(),
            advance_replica_cursor_raw(kind, origin, namespace, previous, next)
        )
    }
    async fn replica_cursor_raw(
        &self,
        kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
    ) -> Result<Option<ReplicaCursor>, ResourceError> {
        access!(self, "replica_cursor", kind.paths.kind, namespace.to_owned(), replica_cursor_raw(kind, origin, namespace))
    }
    async fn get_raw(&self, kind: ResourceKind, namespace: &str, name: &str) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        access!(self, "get", kind.paths.kind, format!("{namespace}/{name}"), get_raw(kind, namespace, name))
    }
    async fn current_position_raw(&self, kind: ResourceKind, namespace: &str) -> Result<ResourcePosition, ResourceError> {
        access!(self, "current_position", kind.paths.kind, namespace.to_owned(), current_position_raw(kind, namespace))
    }
    async fn digest_raw(&self, kind: ResourceKind, namespace: &str, query: &DigestQuery) -> Result<PartitionDigest, ResourceError> {
        access!(self, "digest", kind.paths.kind, namespace.to_owned(), digest_raw(kind, namespace, query))
    }
    async fn confirm_replica_digest_raw(
        &self,
        kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        proof: &PartitionDigest,
        previous: &ReplicaCursor,
    ) -> Result<(), ResourceError> {
        access!(
            self,
            "confirm_replica_digest",
            kind.paths.kind,
            namespace.to_owned(),
            confirm_replica_digest_raw(kind, origin, namespace, proof, previous)
        )
    }
    async fn replica_digest_raw(
        &self,
        kind: ResourceKind,
        origin: &NodeId,
        namespace: &str,
        generation: Option<String>,
    ) -> Result<PartitionDigest, ResourceError> {
        access!(self, "replica_digest", kind.paths.kind, namespace.to_owned(), replica_digest_raw(kind, origin, namespace, generation))
    }
    async fn list_raw(&self, kind: ResourceKind, namespace: &str) -> Result<ResourceList<SerializedResource>, ResourceError> {
        access!(self, "list", kind.paths.kind, namespace.to_owned(), list_raw(kind, namespace))
    }
    async fn list_raw_matching_labels(
        &self,
        kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ResourceList<SerializedResource>, ResourceError> {
        access!(
            self,
            "list_matching_labels",
            kind.paths.kind,
            format!("{namespace} {required:?}"),
            list_raw_matching_labels(kind, namespace, required)
        )
    }
    async fn create_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        access!(self, "create", kind.paths.kind, format!("{namespace}/{}", meta.name), create_raw(kind, namespace, meta, spec))
    }
    async fn create_definition_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        access!(
            self,
            "create_definition",
            kind.paths.kind,
            format!("{namespace}/{}", meta.name),
            create_definition_raw(kind, namespace, meta, spec, merge)
        )
    }
    async fn update_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        access!(
            self,
            "update",
            kind.paths.kind,
            format!("{namespace}/{}", meta.name),
            update_raw(kind, namespace, meta, resource_version, spec)
        )
    }
    async fn update_definition_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        meta: &InputMeta,
        resource_version: &str,
        spec: &Value,
        merge: MergeMetadata,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        access!(
            self,
            "update_definition",
            kind.paths.kind,
            format!("{namespace}/{}", meta.name),
            update_definition_raw(kind, namespace, meta, resource_version, spec, merge)
        )
    }
    async fn update_status_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        name: &str,
        resource_version: &str,
        status: &Value,
    ) -> Result<ResourceObject<SerializedResource>, ResourceError> {
        access!(
            self,
            "update_status",
            kind.paths.kind,
            format!("{namespace}/{name}"),
            update_status_raw(kind, namespace, name, resource_version, status)
        )
    }
    async fn delete_raw(&self, kind: ResourceKind, namespace: &str, name: &str) -> Result<(), ResourceError> {
        access!(self, "delete", kind.paths.kind, format!("{namespace}/{name}"), delete_raw(kind, namespace, name))
    }
    async fn tombstone_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        name: &str,
        minimum_resource_version: Option<&str>,
    ) -> Result<TombstoneWrite, ResourceError> {
        access!(
            self,
            "tombstone",
            kind.paths.kind,
            format!("{namespace}/{name}"),
            tombstone_raw(kind, namespace, name, minimum_resource_version)
        )
    }
    async fn watch_raw(&self, kind: ResourceKind, namespace: &str, start: WatchStart) -> Result<SerializedWatchStream, ResourceError> {
        access!(self, "watch", kind.paths.kind, namespace.to_owned(), watch_raw(kind, namespace, start))
    }
    async fn list_including_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        access!(self, "list_including_replicas", kind.paths.kind, namespace.to_owned(), list_including_replicas_raw(kind, namespace))
    }
    async fn list_including_replicas_raw_matching_labels(
        &self,
        kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        access!(
            self,
            "list_including_replicas_matching_labels",
            kind.paths.kind,
            format!("{namespace} {required:?}"),
            list_including_replicas_raw_matching_labels(kind, namespace, required)
        )
    }
    async fn get_including_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceObject<SerializedResource>, ResourceError> {
        access!(
            self,
            "get_including_replicas",
            kind.paths.kind,
            format!("{namespace}/{name}"),
            get_including_replicas_raw(kind, namespace, name)
        )
    }
    async fn get_all_including_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        name: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        access!(
            self,
            "get_all_including_replicas",
            kind.paths.kind,
            format!("{namespace}/{name}"),
            get_all_including_replicas_raw(kind, namespace, name)
        )
    }
    async fn list_replica_sources_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        access!(self, "list_replica_sources", kind.paths.kind, namespace.to_owned(), list_replica_sources_raw(kind, namespace))
    }
    async fn list_read_view_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query_key: &str,
    ) -> Result<ReadResourceList<SerializedResource>, ResourceError> {
        access!(self, "list_read_view", kind.paths.kind, format!("{namespace} {query_key}"), list_read_view_raw(kind, namespace, query_key))
    }
    async fn watch_including_replicas_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        access!(self, "watch_including_replicas", kind.paths.kind, namespace.to_owned(), watch_including_replicas_raw(kind, namespace))
    }
    async fn list_replica_deletions_raw(&self, kind: ResourceKind, namespace: &str) -> Result<Vec<ReplicaDeletion>, ResourceError> {
        access!(self, "list_replica_deletions", kind.paths.kind, namespace.to_owned(), list_replica_deletions_raw(kind, namespace))
    }
    async fn watch_replica_sources_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        access!(self, "watch_replica_sources", kind.paths.kind, namespace.to_owned(), watch_replica_sources_raw(kind, namespace))
    }
    async fn watch_read_view_raw(
        &self,
        kind: ResourceKind,
        namespace: &str,
        query_key: &str,
    ) -> Result<BoxStream<'static, Result<ReadWatchEvent<SerializedResource>, ResourceError>>, ResourceError> {
        access!(
            self,
            "watch_read_view",
            kind.paths.kind,
            format!("{namespace} {query_key}"),
            watch_read_view_raw(kind, namespace, query_key)
        )
    }
    async fn get_decoded(&self, kind: ResourceKind, namespace: &str, name: &str) -> Result<DecodedObject, ResourceError> {
        access!(self, "get", kind.paths.kind, format!("{namespace}/{name}"), get_decoded(kind, namespace, name))
    }
    async fn list_decoded(&self, kind: ResourceKind, namespace: &str) -> Result<DecodedList, ResourceError> {
        access!(self, "list", kind.paths.kind, namespace.to_owned(), list_decoded(kind, namespace))
    }
    async fn list_decoded_matching_labels(
        &self,
        kind: ResourceKind,
        namespace: &str,
        required: &BTreeMap<String, String>,
    ) -> Result<DecodedList, ResourceError> {
        access!(
            self,
            "list_matching_labels",
            kind.paths.kind,
            format!("{namespace} {required:?}"),
            list_decoded_matching_labels(kind, namespace, required)
        )
    }
    async fn list_replicas_decoded(&self, kind: ResourceKind, namespace: &str) -> Result<Vec<DecodedReadObject>, ResourceError> {
        access!(self, "list_replicas", kind.paths.kind, namespace.to_owned(), list_replicas_decoded(kind, namespace))
    }
    async fn get_replicas_decoded(&self, kind: ResourceKind, namespace: &str, name: &str) -> Result<Vec<DecodedReadObject>, ResourceError> {
        access!(self, "get_replicas", kind.paths.kind, format!("{namespace}/{name}"), get_replicas_decoded(kind, namespace, name))
    }
    async fn query_decoded(&self, kind: ResourceKind, namespace: &str, query: &StoreQuery) -> Result<DecodedQueryResult, ResourceError> {
        access!(self, "query", kind.paths.kind, namespace.to_owned(), query_decoded(kind, namespace, query))
    }
    async fn query_messages_decoded(
        &self,
        namespace: &str,
        query: &MessageQuery,
        include_replicas: bool,
    ) -> Result<Vec<ReadResourceObject<flotilla_resources::Message>>, ResourceError> {
        access!(self, "query_messages", "Message", namespace.to_owned(), query_messages_decoded(namespace, query, include_replicas))
    }
    async fn watch_decoded(&self, kind: ResourceKind, namespace: &str, start: WatchStart) -> Result<SerializedWatchStream, ResourceError> {
        access!(self, "watch", kind.paths.kind, namespace.to_owned(), watch_decoded(kind, namespace, start))
    }
}
