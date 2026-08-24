use std::collections::{BTreeMap, HashSet};

use chrono::Utc;
use flotilla_protocol::NodeId;
use futures::{stream::BoxStream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    api_version,
    field_ownership::merge_owned_spec,
    host::HostStatus,
    replica::{LAST_SYNCED_AT_ANNOTATION, ORIGIN_ROOT_ANNOTATION},
    ChangeRequest, Checkout, Clone as CloneResource, Convoy, ConvoyEnsure, CredentialGrant, CredentialSpec, Demand, DispatchObservation,
    Environment, FieldOwnedResource, Host, InputMeta, MaterialPool, ObjectMeta, OwnerReference, PlacementPolicy, Presentation, Project,
    ReadResourceList, ReadWatchEvent, Regard, ReplicaCursor, ReplicationClass, Repository, Resource, ResourceBackend, ResourceError,
    ResourceList, ResourceObject, ResourceProvenance, TerminalSession, Usage, Vessel, WatchEvent, WatchStart, WorkflowTemplate,
    WriterIdentity,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisteredResourceKind {
    pub kind: &'static str,
    pub plural: &'static str,
    pub replication_class: ReplicationClass,
    aliases: &'static [&'static str],
    resource: RegisteredResource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegisteredResource {
    ChangeRequest,
    Checkout,
    Clone,
    Convoy,
    ConvoyEnsure,
    CredentialGrant,
    CredentialSpec,
    Demand,
    DispatchObservation,
    Environment,
    Host,
    MaterialPool,
    PlacementPolicy,
    Presentation,
    Project,
    Regard,
    Repository,
    TerminalSession,
    Usage,
    Vessel,
    WorkflowTemplate,
}

#[derive(Debug, Clone)]
pub struct DynamicResourceList {
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct DynamicResourceObject {
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub value: Value,
}

#[derive(Debug, Clone)]
pub struct DynamicResourceDelete {
    pub object: DynamicResourceObject,
    pub already_deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
pub struct HomeBoundAuthorshipCollision {
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub local_root: NodeId,
    pub replica_root: NodeId,
}

#[derive(bon::Builder)]
pub struct DynamicResourceWatch {
    pub kind: String,
    pub plural: String,
    pub namespace: String,
    pub resource_version: String,
    pub generation: Option<String>,
    pub initial: Vec<Value>,
    pub stream: BoxStream<'static, Result<Value, ResourceError>>,
}

pub const REGISTERED_RESOURCE_KINDS: &[RegisteredResourceKind] = &[
    kind::<ChangeRequest>(RegisteredResource::ChangeRequest, &["cr", "change_request", "change-request"]),
    kind::<Checkout>(RegisteredResource::Checkout, &[]),
    kind::<CloneResource>(RegisteredResource::Clone, &[]),
    kind::<Convoy>(RegisteredResource::Convoy, &[]),
    kind::<ConvoyEnsure>(RegisteredResource::ConvoyEnsure, &["convoyensure", "convoy_ensure", "convoy-ensure"]),
    kind::<CredentialGrant>(RegisteredResource::CredentialGrant, &[
        "credentialgrant",
        "credential_grant",
        "credential_grants",
        "credential-grant",
        "credential-grants",
    ]),
    kind::<CredentialSpec>(RegisteredResource::CredentialSpec, &[
        "credentialspec",
        "credential_spec",
        "credential_specs",
        "credential-spec",
        "credential-specs",
    ]),
    kind::<Demand>(RegisteredResource::Demand, &[]),
    kind::<DispatchObservation>(RegisteredResource::DispatchObservation, &[
        "dispatchobservation",
        "dispatch_observation",
        "dispatch_observations",
        "dispatch-observation",
        "dispatch-observations",
    ]),
    kind::<Environment>(RegisteredResource::Environment, &[]),
    kind::<Host>(RegisteredResource::Host, &[]),
    kind::<MaterialPool>(RegisteredResource::MaterialPool, &[
        "materialpool",
        "material_pool",
        "material_pools",
        "material-pool",
        "material-pools",
    ]),
    kind::<PlacementPolicy>(RegisteredResource::PlacementPolicy, &[
        "placementpolicy",
        "placement_policy",
        "placement_policies",
        "placement-policy",
        "placement-policies",
    ]),
    kind::<Presentation>(RegisteredResource::Presentation, &[]),
    kind::<Project>(RegisteredResource::Project, &[]),
    kind::<Regard>(RegisteredResource::Regard, &[]),
    kind::<Repository>(RegisteredResource::Repository, &[]),
    kind::<TerminalSession>(RegisteredResource::TerminalSession, &[
        "terminalsession",
        "terminal_session",
        "terminal_sessions",
        "terminal-session",
        "terminal-sessions",
    ]),
    kind::<Usage>(RegisteredResource::Usage, &[]),
    kind::<Vessel>(RegisteredResource::Vessel, &[]),
    kind::<WorkflowTemplate>(RegisteredResource::WorkflowTemplate, &[
        "workflowtemplate",
        "workflow_template",
        "workflow_templates",
        "workflow-template",
        "workflow-templates",
    ]),
];

const fn kind<T: Resource>(resource: RegisteredResource, aliases: &'static [&'static str]) -> RegisteredResourceKind {
    RegisteredResourceKind {
        kind: T::API_PATHS.kind,
        plural: T::API_PATHS.plural,
        replication_class: T::REPLICATION_CLASS,
        aliases,
        resource,
    }
}

macro_rules! dispatch_resource_kind {
    ($resource:expr, $body:ident($($arg:expr),*).await) => {
        match $resource {
            RegisteredResource::ChangeRequest => $body::<ChangeRequest>($($arg),*).await,
            RegisteredResource::Checkout => $body::<Checkout>($($arg),*).await,
            RegisteredResource::Clone => $body::<CloneResource>($($arg),*).await,
            RegisteredResource::Convoy => $body::<Convoy>($($arg),*).await,
            RegisteredResource::ConvoyEnsure => $body::<ConvoyEnsure>($($arg),*).await,
            RegisteredResource::CredentialGrant => $body::<CredentialGrant>($($arg),*).await,
            RegisteredResource::CredentialSpec => $body::<CredentialSpec>($($arg),*).await,
            RegisteredResource::Demand => $body::<Demand>($($arg),*).await,
            RegisteredResource::DispatchObservation => $body::<DispatchObservation>($($arg),*).await,
            RegisteredResource::Environment => $body::<Environment>($($arg),*).await,
            RegisteredResource::Host => $body::<Host>($($arg),*).await,
            RegisteredResource::MaterialPool => $body::<MaterialPool>($($arg),*).await,
            RegisteredResource::PlacementPolicy => $body::<PlacementPolicy>($($arg),*).await,
            RegisteredResource::Presentation => $body::<Presentation>($($arg),*).await,
            RegisteredResource::Project => $body::<Project>($($arg),*).await,
            RegisteredResource::Regard => $body::<Regard>($($arg),*).await,
            RegisteredResource::Repository => $body::<Repository>($($arg),*).await,
            RegisteredResource::TerminalSession => $body::<TerminalSession>($($arg),*).await,
            RegisteredResource::Usage => $body::<Usage>($($arg),*).await,
            RegisteredResource::Vessel => $body::<Vessel>($($arg),*).await,
            RegisteredResource::WorkflowTemplate => $body::<WorkflowTemplate>($($arg),*).await,
        }
    };
    ($resource:expr, $body:ident()) => {
        match $resource {
            RegisteredResource::ChangeRequest => $body::<ChangeRequest>(),
            RegisteredResource::Checkout => $body::<Checkout>(),
            RegisteredResource::Clone => $body::<CloneResource>(),
            RegisteredResource::Convoy => $body::<Convoy>(),
            RegisteredResource::ConvoyEnsure => $body::<ConvoyEnsure>(),
            RegisteredResource::CredentialGrant => $body::<CredentialGrant>(),
            RegisteredResource::CredentialSpec => $body::<CredentialSpec>(),
            RegisteredResource::Demand => $body::<Demand>(),
            RegisteredResource::DispatchObservation => $body::<DispatchObservation>(),
            RegisteredResource::Environment => $body::<Environment>(),
            RegisteredResource::Host => $body::<Host>(),
            RegisteredResource::MaterialPool => $body::<MaterialPool>(),
            RegisteredResource::PlacementPolicy => $body::<PlacementPolicy>(),
            RegisteredResource::Presentation => $body::<Presentation>(),
            RegisteredResource::Project => $body::<Project>(),
            RegisteredResource::Regard => $body::<Regard>(),
            RegisteredResource::Repository => $body::<Repository>(),
            RegisteredResource::TerminalSession => $body::<TerminalSession>(),
            RegisteredResource::Usage => $body::<Usage>(),
            RegisteredResource::Vessel => $body::<Vessel>(),
            RegisteredResource::WorkflowTemplate => $body::<WorkflowTemplate>(),
        }
    };
    ($resource:expr, $body:ident($($arg:expr),*)) => {
        match $resource {
            RegisteredResource::ChangeRequest => $body::<ChangeRequest>($($arg),*),
            RegisteredResource::Checkout => $body::<Checkout>($($arg),*),
            RegisteredResource::Clone => $body::<CloneResource>($($arg),*),
            RegisteredResource::Convoy => $body::<Convoy>($($arg),*),
            RegisteredResource::ConvoyEnsure => $body::<ConvoyEnsure>($($arg),*),
            RegisteredResource::CredentialGrant => $body::<CredentialGrant>($($arg),*),
            RegisteredResource::CredentialSpec => $body::<CredentialSpec>($($arg),*),
            RegisteredResource::Demand => $body::<Demand>($($arg),*),
            RegisteredResource::DispatchObservation => $body::<DispatchObservation>($($arg),*),
            RegisteredResource::Environment => $body::<Environment>($($arg),*),
            RegisteredResource::Host => $body::<Host>($($arg),*),
            RegisteredResource::MaterialPool => $body::<MaterialPool>($($arg),*),
            RegisteredResource::PlacementPolicy => $body::<PlacementPolicy>($($arg),*),
            RegisteredResource::Presentation => $body::<Presentation>($($arg),*),
            RegisteredResource::Project => $body::<Project>($($arg),*),
            RegisteredResource::Regard => $body::<Regard>($($arg),*),
            RegisteredResource::Repository => $body::<Repository>($($arg),*),
            RegisteredResource::TerminalSession => $body::<TerminalSession>($($arg),*),
            RegisteredResource::Usage => $body::<Usage>($($arg),*),
            RegisteredResource::Vessel => $body::<Vessel>($($arg),*),
            RegisteredResource::WorkflowTemplate => $body::<WorkflowTemplate>($($arg),*),
        }
    };
}

/// Dynamic apply uses the ownership-aware path for enrolled kinds. Enrolling
/// another resource changes one dispatch arm rather than adding a one-off
/// apply function or branch.
macro_rules! dispatch_apply_resource_kind {
    ($resource:expr, $backend:expr, $namespace:expr, $metadata:expr, $spec:expr) => {
        match $resource {
            RegisteredResource::PlacementPolicy => apply_owned_typed::<PlacementPolicy>($backend, $namespace, $metadata, $spec).await,
            resource => dispatch_resource_kind!(resource, apply_typed($backend, $namespace, $metadata, $spec).await),
        }
    };
}

/// Manifest reconciliation is authoritative for both operator-authored input
/// and loop-derived topology. Enrolled resources project the same desired spec
/// through each role instead of bypassing the single ownership-aware write path.
macro_rules! dispatch_manifest_apply_resource_kind {
    ($resource:expr, $backend:expr, $namespace:expr, $metadata:expr, $spec:expr) => {
        match $resource {
            RegisteredResource::PlacementPolicy => {
                apply_manifest_owned_typed::<PlacementPolicy>($backend, $namespace, $metadata, $spec).await
            }
            resource => dispatch_resource_kind!(resource, apply_typed($backend, $namespace, $metadata, $spec).await),
        }
    };
}

/// Drives a typed read of every kind in the embedded durable store.
///
/// Embedded stores use this startup pass to isolate rows whose persisted
/// representation no longer decodes under the running schema. Other backends
/// cannot contain an untyped local row and need no scan.
pub async fn quarantine_undecodable_stored_objects(backend: &ResourceBackend, namespace: &str) -> Result<(), ResourceError> {
    if !matches!(backend, ResourceBackend::Sqlite(_)) {
        return Ok(());
    }
    for registered in REGISTERED_RESOURCE_KINDS {
        list_resource_kind(backend, namespace, registered.kind).await?;
    }
    Ok(())
}

pub async fn list_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceList, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, list_typed(backend, namespace).await)
}

pub async fn list_resource_kind_including_replicas(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceList, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, list_typed_including_replicas(backend, namespace).await)
}

pub async fn replica_cursor_for_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    origin_root: &NodeId,
) -> Result<Option<ReplicaCursor>, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, replica_cursor_typed(backend, namespace, origin_root).await)
}

/// Find same-name records authored both locally and by another root for every
/// single-home resource kind.
///
/// The collision is diagnostic only: callers must surface it rather than
/// choosing either record as authoritative.
pub async fn home_bound_authorship_collisions(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<Vec<HomeBoundAuthorshipCollision>, ResourceError> {
    let local_root = backend.local_root()?;
    let mut collisions = Vec::new();
    for registered in REGISTERED_RESOURCE_KINDS {
        if registered.replication_class != ReplicationClass::HomeBoundRuntime {
            continue;
        }
        collisions.extend(dispatch_resource_kind!(
            registered.resource,
            home_bound_authorship_collisions_typed(backend, namespace, &local_root).await
        )?);
    }
    collisions.sort_by(|left, right| {
        (&left.kind, &left.namespace, &left.name, &left.replica_root).cmp(&(
            &right.kind,
            &right.namespace,
            &right.name,
            &right.replica_root,
        ))
    });
    Ok(collisions)
}

async fn home_bound_authorship_collisions_typed<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    local_root: &NodeId,
) -> Result<Vec<HomeBoundAuthorshipCollision>, ResourceError> {
    let sources = backend.including_replicas::<T>(namespace).list_sources().await?;
    let local_names = sources
        .items
        .iter()
        .filter(|item| matches!(item.provenance, ResourceProvenance::Local))
        .map(|item| item.object.metadata.name.as_str())
        .collect::<HashSet<_>>();
    Ok(sources
        .items
        .iter()
        .filter_map(|item| {
            let ResourceProvenance::Replica { origin_root, .. } = &item.provenance else {
                return None;
            };
            (origin_root != local_root && local_names.contains(item.object.metadata.name.as_str())).then(|| {
                HomeBoundAuthorshipCollision::builder()
                    .kind(T::API_PATHS.kind.to_string())
                    .namespace(namespace.to_string())
                    .name(item.object.metadata.name.clone())
                    .local_root(local_root.clone())
                    .replica_root(origin_root.clone())
                    .build()
            })
        })
        .collect())
}

async fn replica_cursor_typed<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    origin_root: &NodeId,
) -> Result<Option<ReplicaCursor>, ResourceError> {
    backend.replica_writer::<T>(origin_root.clone(), namespace).cursor().await
}

pub async fn get_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
) -> Result<DynamicResourceObject, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, get_typed(backend, namespace, name).await)
}

pub async fn get_resource_kind_including_replicas(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
) -> Result<DynamicResourceObject, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, get_typed_including_replicas(backend, namespace, name).await)
}

pub async fn delete_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
) -> Result<DynamicResourceDelete, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, delete_typed(backend, namespace, name).await)
}

/// Deliberately collect one cached replica while preserving its read-only
/// authority boundary.
///
/// The stored delete is timestamped and relayed like an ordinary replica
/// tombstone, so stale copies from other peers cannot immediately resurrect
/// the object. A later update from the original authority remains newer and
/// can recreate it.
pub async fn collect_resource_replica_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
    origin_root: &NodeId,
) -> Result<DynamicResourceObject, ResourceError> {
    dispatch_resource_kind!(
        lookup_resource_kind(requested_kind)?.resource,
        collect_replica_typed(backend, namespace, name, origin_root).await
    )
}

pub async fn patch_resource_status(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
    status: Value,
) -> Result<DynamicResourceObject, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, patch_status_typed(backend, namespace, name, status).await)
}

pub async fn watch_resource_kind(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceWatch, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, watch_typed(backend, namespace, None).await)
}

pub async fn watch_resource_kind_from(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    start: WatchStart,
) -> Result<DynamicResourceWatch, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, watch_typed(backend, namespace, Some(start)).await)
}

pub async fn watch_resource_kind_including_replicas(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceWatch, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, watch_typed_including_replicas(backend, namespace).await)
}

pub async fn list_resource_kind_replica_sources(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceList, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, list_typed_replica_sources(backend, namespace).await)
}

pub async fn watch_resource_kind_replica_sources(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
) -> Result<DynamicResourceWatch, ResourceError> {
    dispatch_resource_kind!(lookup_resource_kind(requested_kind)?.resource, watch_typed_replica_sources(backend, namespace).await)
}

pub async fn apply_resource_document(
    backend: &ResourceBackend,
    default_namespace: &str,
    document: Value,
) -> Result<DynamicResourceObject, ResourceError> {
    let document: DynamicApplyDocument =
        serde_json::from_value(document).map_err(|error| ResourceError::decode(format!("decode resource document: {error}")))?;
    let namespace = document.metadata.namespace.clone().unwrap_or_else(|| default_namespace.to_string());
    dispatch_apply_resource_kind!(lookup_resource_kind(&document.kind)?.resource, backend, &namespace, document.metadata, document.spec)
}

pub async fn patch_resource_annotation(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
    key: &str,
    value: &str,
) -> Result<DynamicResourceObject, ResourceError> {
    let annotations = BTreeMap::from([(key.to_string(), value.to_string())]);
    patch_resource_annotations(backend, namespace, requested_kind, name, &annotations).await
}

pub async fn patch_resource_annotations(
    backend: &ResourceBackend,
    namespace: &str,
    requested_kind: &str,
    name: &str,
    annotations: &BTreeMap<String, String>,
) -> Result<DynamicResourceObject, ResourceError> {
    dispatch_resource_kind!(
        lookup_resource_kind(requested_kind)?.resource,
        patch_annotations_typed(backend, namespace, name, annotations).await
    )
}

async fn patch_annotations_typed<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    annotations: &BTreeMap<String, String>,
) -> Result<DynamicResourceObject, ResourceError> {
    let updated = if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
        let resolver = backend.definitions::<T>(namespace);
        let existing = resolver.get(name).await?;
        let mut meta = InputMeta::from(&existing.metadata);
        meta.annotations.extend(annotations.clone());
        resolver.apply(&meta, &existing.spec).await?
    } else {
        let resolver = backend.using::<T>(namespace);
        let existing = resolver.get(name).await?;
        let mut meta = InputMeta::from(&existing.metadata);
        meta.annotations.extend(annotations.clone());
        resolver.update(&meta, &existing.metadata.resource_version, &existing.spec).await?
    };
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: object_value(&updated)?,
    })
}

pub async fn apply_manifest_resource_document(
    backend: &ResourceBackend,
    default_namespace: &str,
    document: Value,
) -> Result<DynamicResourceObject, ResourceError> {
    let document: DynamicApplyDocument =
        serde_json::from_value(document).map_err(|error| ResourceError::decode(format!("decode resource document: {error}")))?;
    let namespace = document.metadata.namespace.clone().unwrap_or_else(|| default_namespace.to_string());
    dispatch_manifest_apply_resource_kind!(
        lookup_resource_kind(&document.kind)?.resource,
        backend,
        &namespace,
        document.metadata,
        document.spec
    )
}

/// Hash a resource document's spec after its registered typed representation
/// has applied Serde defaults.
pub fn resource_document_spec_hash(document: &Value) -> Result<String, ResourceError> {
    let kind = document
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| ResourceError::decode("decode resource document: missing or non-string kind"))?;
    let spec = document.get("spec").ok_or_else(|| ResourceError::decode("decode resource document: missing spec"))?;
    dispatch_resource_kind!(lookup_resource_kind(kind)?.resource, typed_spec_hash(spec))
}

fn typed_spec_hash<T: Resource>(spec: &Value) -> Result<String, ResourceError> {
    let typed = serde_json::from_value::<T::Spec>(spec.clone())
        .map_err(|error| ResourceError::decode(format!("decode {} spec: {error}", T::API_PATHS.kind)))?;
    let normalized = serde_json::to_value(typed)
        .map_err(|error| ResourceError::decode(format!("encode normalized {} spec: {error}", T::API_PATHS.kind)))?;
    crate::content_hash(&normalized)
}

fn lookup_resource_kind(kind: &str) -> Result<&'static RegisteredResourceKind, ResourceError> {
    let normalized = kind.trim();
    REGISTERED_RESOURCE_KINDS
        .iter()
        .find(|entry| {
            normalized.eq_ignore_ascii_case(entry.kind)
                || normalized.eq_ignore_ascii_case(entry.plural)
                || entry.aliases.iter().any(|alias| normalized.eq_ignore_ascii_case(alias))
        })
        .ok_or_else(|| unknown_kind(kind))
}

#[derive(Debug, Deserialize)]
struct DynamicApplyDocument {
    kind: String,
    metadata: DynamicApplyMetadata,
    spec: Value,
}

#[derive(Debug, Deserialize)]
struct DynamicApplyMetadata {
    name: String,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    annotations: Option<BTreeMap<String, String>>,
    #[serde(default, rename = "ownerReferences")]
    owner_references: Option<Vec<OwnerReference>>,
    #[serde(default)]
    finalizers: Option<Vec<String>>,
}

impl DynamicApplyMetadata {
    fn input_meta_for_create(&self) -> InputMeta {
        InputMeta {
            name: self.name.clone(),
            labels: self.labels.clone().unwrap_or_default(),
            annotations: self.annotations.clone().unwrap_or_default(),
            owner_references: self.owner_references.clone().unwrap_or_default(),
            finalizers: self.finalizers.clone().unwrap_or_default(),
            deletion_timestamp: None,
        }
    }

    fn input_meta_for_update(&self, existing: &ObjectMeta) -> InputMeta {
        InputMeta {
            name: self.name.clone(),
            labels: self.labels.clone().unwrap_or_else(|| existing.labels.clone()),
            annotations: self.annotations.clone().unwrap_or_else(|| existing.annotations.clone()),
            owner_references: self.owner_references.clone().unwrap_or_else(|| existing.owner_references.clone()),
            finalizers: self.finalizers.clone().unwrap_or_else(|| existing.finalizers.clone()),
            deletion_timestamp: existing.deletion_timestamp,
        }
    }
}

fn unknown_kind(kind: &str) -> ResourceError {
    let supported = REGISTERED_RESOURCE_KINDS.iter().map(|entry| entry.plural).collect::<Vec<_>>().join(", ");
    ResourceError::invalid(format!("unknown resource kind '{kind}' (supported: {supported})"))
}

async fn list_typed<T: Resource>(backend: &ResourceBackend, namespace: &str) -> Result<DynamicResourceList, ResourceError> {
    let listed = backend.using::<T>(namespace).list().await?;
    Ok(DynamicResourceList {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: list_value::<T>(&listed)?,
    })
}

async fn list_typed_including_replicas<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<DynamicResourceList, ResourceError> {
    let listed = backend.including_replicas::<T>(namespace).list().await?;
    let items = listed.items.iter().map(read_object_value).collect::<Result<Vec<_>, _>>()?;
    Ok(DynamicResourceList {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: json!({
            "apiVersion": api_version(T::API_PATHS),
            "kind": format!("{}List", T::API_PATHS.kind),
            "metadata": {"resourceVersion": "overlay"},
            "items": items,
        }),
    })
}

async fn list_typed_replica_sources<T: Resource>(backend: &ResourceBackend, namespace: &str) -> Result<DynamicResourceList, ResourceError> {
    let listed = backend.including_replicas::<T>(namespace).list_sources().await?;
    dynamic_read_list::<T>(namespace, listed)
}

fn dynamic_read_list<T: Resource>(namespace: &str, listed: ReadResourceList<T>) -> Result<DynamicResourceList, ResourceError> {
    let items = listed.items.iter().map(read_object_value).collect::<Result<Vec<_>, _>>()?;
    Ok(DynamicResourceList {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: json!({
            "apiVersion": api_version(T::API_PATHS),
            "kind": format!("{}List", T::API_PATHS.kind),
            "metadata": {"resourceVersion": "overlay"},
            "items": items,
        }),
    })
}

async fn get_typed<T: Resource>(backend: &ResourceBackend, namespace: &str, name: &str) -> Result<DynamicResourceObject, ResourceError> {
    let object = if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
        backend.definitions::<T>(namespace).get(name).await?
    } else {
        backend.using::<T>(namespace).get(name).await?
    };
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: object_value(&object)?,
    })
}

async fn get_typed_including_replicas<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
) -> Result<DynamicResourceObject, ResourceError> {
    let object = backend.including_replicas::<T>(namespace).get(name).await?;
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: read_object_value(&object)?,
    })
}

async fn delete_typed<T: Resource>(backend: &ResourceBackend, namespace: &str, name: &str) -> Result<DynamicResourceDelete, ResourceError> {
    let resolver = backend.using::<T>(namespace);
    let (value, already_deleted) = match resolver.get(name).await {
        Ok(object) => {
            resolver.delete(name).await?;
            if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
                return Ok(DynamicResourceDelete {
                    object: DynamicResourceObject {
                        kind: T::API_PATHS.kind.to_string(),
                        plural: T::API_PATHS.plural.to_string(),
                        namespace: namespace.to_string(),
                        value: object_value(&object)?,
                    },
                    already_deleted: false,
                });
            }
            force_pending_finalization(&resolver, name).await?;
            match resolver.get(name).await {
                Err(ResourceError::NotFound { .. }) => {}
                Ok(_) => return Err(ResourceError::other(format!("delete reported success but {name} remains in the resource store"))),
                Err(error) => return Err(error),
            }
            if T::REPLICATION_CLASS != crate::ReplicationClass::None {
                retain_authoritative_name_tombstone::<T>(backend, namespace, name).await?;
            }
            (object_value(&object)?, false)
        }
        Err(not_found @ ResourceError::NotFound { .. }) => {
            if backend.delete_decode_quarantine::<T>(namespace, name).await? {
                if T::REPLICATION_CLASS != crate::ReplicationClass::None {
                    let write = retain_authoritative_name_tombstone::<T>(backend, namespace, name).await?;
                    (tombstone_value::<T>(&write.tombstone), !write.created)
                } else {
                    let tombstone = crate::ResourceTombstone {
                        name: name.to_string(),
                        namespace: namespace.to_string(),
                        resource_version: String::new(),
                        annotations: BTreeMap::new(),
                    };
                    (tombstone_value::<T>(&tombstone), false)
                }
            } else if T::REPLICATION_CLASS != crate::ReplicationClass::None {
                let write = retain_authoritative_name_tombstone::<T>(backend, namespace, name).await?;
                (tombstone_value::<T>(&write.tombstone), !write.created)
            } else {
                return Err(not_found);
            }
        }
        Err(error) => return Err(error),
    };
    Ok(DynamicResourceDelete {
        object: DynamicResourceObject {
            kind: T::API_PATHS.kind.to_string(),
            plural: T::API_PATHS.plural.to_string(),
            namespace: namespace.to_string(),
            value,
        },
        already_deleted,
    })
}

/// Raw resource deletion is the recovery path documented in the governor
/// charter, so it must not leave an object stuck behind an abandoned
/// finalizer. Ordinary controller deletion still goes through
/// `TypedResolver::delete` and retains normal finalizer semantics.
async fn force_pending_finalization<T: Resource>(resolver: &crate::TypedResolver<T>, name: &str) -> Result<(), ResourceError> {
    for _ in 0..3 {
        let object = match resolver.get(name).await {
            Err(ResourceError::NotFound { .. }) => return Ok(()),
            Ok(object) => object,
            Err(error) => return Err(error),
        };
        if !object.metadata.is_pending_finalization() {
            return Err(ResourceError::other(format!("delete left {name} present without pending finalizers")));
        }

        let mut meta = InputMeta::from(&object.metadata);
        meta.finalizers.clear();
        match resolver.update(&meta, &object.metadata.resource_version, &object.spec).await {
            Ok(_) => return Ok(()),
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }

    Err(ResourceError::conflict(name, "finalizer removal retry budget exhausted"))
}

async fn retain_authoritative_name_tombstone<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
) -> Result<crate::watch::TombstoneWrite, ResourceError> {
    let write = backend.using::<T>(namespace).tombstone(name).await?;
    if write.created {
        backend
            .replica_writer::<T>(backend.local_root()?, namespace)
            .apply(WatchEvent::DeletedByName(write.tombstone.clone()), Utc::now())
            .await?;
    }
    Ok(write)
}

async fn collect_replica_typed<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    origin_root: &NodeId,
) -> Result<DynamicResourceObject, ResourceError> {
    let replica = backend
        .including_replicas::<T>(namespace)
        .list_sources()
        .await?
        .items
        .into_iter()
        .find(|source| {
            source.object.metadata.name == name
                && matches!(&source.provenance, ResourceProvenance::Replica { origin_root: source_origin, .. } if source_origin == origin_root)
        })
        .ok_or_else(|| ResourceError::not_found(name))?;
    let value = read_object_value(&replica)?;
    backend.replica_writer::<T>(origin_root.clone(), namespace).apply(WatchEvent::Deleted(replica.object), Utc::now()).await?;
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value,
    })
}

async fn patch_status_typed<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    name: &str,
    status: Value,
) -> Result<DynamicResourceObject, ResourceError> {
    let status = serde_json::from_value::<T::Status>(status)
        .map_err(|error| ResourceError::decode(format!("decode {} status: {error}", T::API_PATHS.kind)))?;
    let resolver = backend.using::<T>(namespace);
    for _ in 0..3 {
        let current = resolver.get(name).await?;
        match resolver.update_status(name, &current.metadata.resource_version, &status).await {
            Ok(object) => {
                return Ok(DynamicResourceObject {
                    kind: T::API_PATHS.kind.to_string(),
                    plural: T::API_PATHS.plural.to_string(),
                    namespace: namespace.to_string(),
                    value: object_value(&object)?,
                });
            }
            Err(ResourceError::Conflict { .. }) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(ResourceError::conflict(name, "status patch retry budget exhausted"))
}

async fn watch_typed<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    requested_start: Option<WatchStart>,
) -> Result<DynamicResourceWatch, ResourceError> {
    let resolver = backend.using::<T>(namespace);
    let (resource_version, generation, initial, start) = match requested_start {
        Some(start) => {
            let resource_version = match &start {
                WatchStart::Now => String::new(),
                WatchStart::FromVersion(resource_version) => resource_version.clone(),
                WatchStart::FromVersionInGeneration { resource_version, .. } => resource_version.clone(),
            };
            let generation = match &start {
                WatchStart::FromVersionInGeneration { generation, .. } => Some(generation.clone()),
                _ => None,
            };
            (resource_version, generation, Vec::new(), start)
        }
        None => {
            let listed = resolver.list().await?;
            let initial = listed.items.iter().map(|object| watch_event_value("ADDED", object)).collect::<Result<Vec<_>, _>>()?;
            (listed.resource_version.clone(), listed.generation.clone(), initial, WatchStart::resuming_from(&listed))
        }
    };
    let watch = resolver.watch(start).await?;
    let stream_generation = watch.generation().map(ToOwned::to_owned).or(generation);
    let stream = watch.map(|event| event.and_then(|event| typed_watch_event_value(&event))).boxed();
    Ok(DynamicResourceWatch {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        resource_version,
        generation: stream_generation,
        initial,
        stream,
    })
}

async fn watch_typed_including_replicas<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<DynamicResourceWatch, ResourceError> {
    let resolver = backend.including_replicas::<T>(namespace);
    let stream = resolver.watch().await?.map(|event| event.and_then(|event| read_watch_event_value(&event))).boxed();
    let initial = resolver
        .list()
        .await?
        .items
        .iter()
        .map(|object| Ok(json!({"type": "ADDED", "object": read_object_value(object)?})))
        .collect::<Result<Vec<_>, ResourceError>>()?;
    Ok(DynamicResourceWatch {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        resource_version: "overlay".to_string(),
        generation: None,
        initial,
        stream,
    })
}

async fn watch_typed_replica_sources<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
) -> Result<DynamicResourceWatch, ResourceError> {
    let resolver = backend.including_replicas::<T>(namespace);
    let stream = resolver.watch_sources().await?.map(|event| event.and_then(|event| read_watch_event_value(&event))).boxed();
    let initial = resolver
        .list_sources()
        .await?
        .items
        .iter()
        .map(|object| Ok(json!({"type": "ADDED", "object": read_object_value(object)?})))
        .collect::<Result<Vec<_>, ResourceError>>()?;
    Ok(DynamicResourceWatch {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        resource_version: "overlay".to_string(),
        generation: None,
        initial,
        stream,
    })
}

async fn apply_typed<T: Resource>(
    backend: &ResourceBackend,
    namespace: &str,
    metadata: DynamicApplyMetadata,
    spec: Value,
) -> Result<DynamicResourceObject, ResourceError> {
    let spec = serde_json::from_value::<T::Spec>(spec)
        .map_err(|error| ResourceError::decode(format!("decode {} spec: {error}", T::API_PATHS.kind)))?;
    if T::REPLICATION_CLASS == crate::ReplicationClass::Definitions {
        let meta = match backend.definitions::<T>(namespace).get(&metadata.name).await {
            Ok(existing) => metadata.input_meta_for_update(&existing.metadata),
            Err(ResourceError::NotFound { .. }) => metadata.input_meta_for_create(),
            Err(error) => return Err(error),
        };
        let object = backend.definitions::<T>(namespace).apply(&meta, &spec).await?;
        return Ok(DynamicResourceObject {
            kind: T::API_PATHS.kind.to_string(),
            plural: T::API_PATHS.plural.to_string(),
            namespace: namespace.to_string(),
            value: object_value(&object)?,
        });
    }
    let resolver = backend.using::<T>(namespace);
    let object = match resolver.get(&metadata.name).await {
        Ok(existing) => {
            let meta = metadata.input_meta_for_update(&existing.metadata);
            resolver.update(&meta, &existing.metadata.resource_version, &spec).await?
        }
        Err(ResourceError::NotFound { .. }) => resolver.create(&metadata.input_meta_for_create(), &spec).await?,
        Err(error) => return Err(error),
    };
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: object_value(&object)?,
    })
}

async fn apply_owned_typed<T: FieldOwnedResource>(
    backend: &ResourceBackend,
    namespace: &str,
    metadata: DynamicApplyMetadata,
    spec: Value,
) -> Result<DynamicResourceObject, ResourceError> {
    let spec =
        serde_json::from_value(spec).map_err(|error| ResourceError::decode(format!("decode {} spec: {error}", T::API_PATHS.kind)))?;
    let resolver = backend.using::<T>(namespace);
    let object = match resolver.get(&metadata.name).await {
        Ok(existing) => {
            let meta = metadata.input_meta_for_update(&existing.metadata);
            resolver.write_spec(&WriterIdentity::operator(), &meta, &existing.metadata.resource_version, &spec).await?
        }
        Err(ResourceError::NotFound { .. }) => resolver.create(&metadata.input_meta_for_create(), &spec).await?,
        Err(error) => return Err(error),
    };
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: object_value(&object)?,
    })
}

async fn apply_manifest_owned_typed<T: FieldOwnedResource>(
    backend: &ResourceBackend,
    namespace: &str,
    metadata: DynamicApplyMetadata,
    spec: Value,
) -> Result<DynamicResourceObject, ResourceError> {
    let desired =
        serde_json::from_value(spec).map_err(|error| ResourceError::decode(format!("decode {} spec: {error}", T::API_PATHS.kind)))?;
    let resolver = backend.using::<T>(namespace);
    let object = match resolver.get(&metadata.name).await {
        Ok(existing) => {
            let operator = WriterIdentity::operator();
            let (operator_spec, _) = merge_owned_spec::<T>(&existing.spec, &desired, &operator, namespace, &metadata.name)?;
            let meta = metadata.input_meta_for_update(&existing.metadata);
            let operator_written = resolver.write_spec(&operator, &meta, &existing.metadata.resource_version, &operator_spec).await?;

            let reconcile_loop = WriterIdentity::reconcile_loop();
            let (loop_spec, _) = merge_owned_spec::<T>(&operator_written.spec, &desired, &reconcile_loop, namespace, &metadata.name)?;
            let current_value = serde_json::to_value(&operator_written.spec)
                .map_err(|error| ResourceError::decode(format!("encode current {} spec: {error}", T::API_PATHS.kind)))?;
            let loop_value = serde_json::to_value(&loop_spec)
                .map_err(|error| ResourceError::decode(format!("encode projected {} spec: {error}", T::API_PATHS.kind)))?;
            if current_value == loop_value {
                operator_written
            } else {
                let meta = metadata.input_meta_for_update(&operator_written.metadata);
                resolver.write_spec(&reconcile_loop, &meta, &operator_written.metadata.resource_version, &loop_spec).await?
            }
        }
        Err(ResourceError::NotFound { .. }) => resolver.create(&metadata.input_meta_for_create(), &desired).await?,
        Err(error) => return Err(error),
    };
    Ok(DynamicResourceObject {
        kind: T::API_PATHS.kind.to_string(),
        plural: T::API_PATHS.plural.to_string(),
        namespace: namespace.to_string(),
        value: object_value(&object)?,
    })
}

fn list_value<T: Resource>(listed: &ResourceList<T>) -> Result<Value, ResourceError> {
    let items = listed.items.iter().map(object_value).collect::<Result<Vec<_>, _>>()?;
    let mut metadata = json!({"resourceVersion": listed.resource_version.clone()});
    if let Some(generation) = &listed.generation {
        metadata["generation"] = Value::String(generation.clone());
    }
    Ok(json!({
        "apiVersion": api_version(T::API_PATHS),
        "kind": format!("{}List", T::API_PATHS.kind),
        "metadata": metadata,
        "items": items,
    }))
}

fn object_value<T: Resource>(object: &ResourceObject<T>) -> Result<Value, ResourceError> {
    let mut value =
        serde_json::to_value(object.to_k8s_object()).map_err(|error| ResourceError::decode(format!("encode resource object: {error}")))?;
    apply_host_heartbeat_readiness::<T>(&mut value)?;
    Ok(value)
}

fn read_object_value<T: Resource>(object: &crate::ReadResourceObject<T>) -> Result<Value, ResourceError> {
    let mut value = object_value(&object.object)?;
    if let ResourceProvenance::Replica { origin_root, last_synced_at } = &object.provenance {
        let annotations = value["metadata"]["annotations"]
            .as_object_mut()
            .ok_or_else(|| ResourceError::decode("resource metadata annotations are not an object"))?;
        annotations.insert(ORIGIN_ROOT_ANNOTATION.to_string(), Value::String(origin_root.to_string()));
        annotations.insert(LAST_SYNCED_AT_ANNOTATION.to_string(), Value::String(last_synced_at.to_rfc3339()));
    }
    Ok(value)
}

fn read_watch_event_value<T: Resource>(event: &ReadWatchEvent<T>) -> Result<Value, ResourceError> {
    let (event_type, object) = match event {
        ReadWatchEvent::Added(object) => ("ADDED", object),
        ReadWatchEvent::Modified(object) => ("MODIFIED", object),
        ReadWatchEvent::Deleted(object) => ("DELETED", object),
        ReadWatchEvent::DeletedByName { tombstone, provenance } => {
            let mut tombstone = tombstone.clone();
            if let ResourceProvenance::Replica { origin_root, last_synced_at } = provenance {
                tombstone.annotations.insert(ORIGIN_ROOT_ANNOTATION.to_string(), origin_root.to_string());
                tombstone.annotations.insert(LAST_SYNCED_AT_ANNOTATION.to_string(), last_synced_at.to_rfc3339());
            }
            return Ok(json!({"type": "DELETED", "object": tombstone_value::<T>(&tombstone)}));
        }
    };
    Ok(json!({"type": event_type, "object": read_object_value(object)?}))
}

fn typed_watch_event_value<T: Resource>(event: &WatchEvent<T>) -> Result<Value, ResourceError> {
    match event {
        WatchEvent::Added(object) => watch_event_value("ADDED", object),
        WatchEvent::Modified(object) => watch_event_value("MODIFIED", object),
        WatchEvent::Deleted(object) => watch_event_value("DELETED", object),
        WatchEvent::DeletedByName(tombstone) => Ok(json!({"type": "DELETED", "object": tombstone_value::<T>(tombstone)})),
    }
}

fn tombstone_value<T: Resource>(tombstone: &crate::ResourceTombstone) -> Value {
    json!({
        "apiVersion": api_version(T::API_PATHS),
        "kind": T::API_PATHS.kind,
        "metadata": {
            "name": tombstone.name,
            "namespace": tombstone.namespace,
            "resourceVersion": tombstone.resource_version,
            "annotations": tombstone.annotations,
        },
    })
}

fn watch_event_value<T: Resource>(event_type: &str, object: &ResourceObject<T>) -> Result<Value, ResourceError> {
    Ok(json!({
        "type": event_type,
        "object": object_value(object)?,
    }))
}

fn apply_host_heartbeat_readiness<T: Resource>(value: &mut Value) -> Result<(), ResourceError> {
    if T::API_PATHS.kind != Host::API_PATHS.kind {
        return Ok(());
    }
    let Some(status_value) = value.get_mut("status") else {
        return Ok(());
    };
    if status_value.is_null() {
        return Ok(());
    }
    let mut status: HostStatus = serde_json::from_value(status_value.clone())
        .map_err(|error| ResourceError::decode(format!("decode Host status for heartbeat readiness: {error}")))?;
    status.apply_heartbeat_readiness(chrono::Utc::now());
    *status_value = serde_json::to_value(status)
        .map_err(|error| ResourceError::decode(format!("encode Host status for heartbeat readiness: {error}")))?;
    Ok(())
}

pub fn resource_list_api_version(kind: &str) -> Result<String, ResourceError> {
    Ok(dispatch_resource_kind!(lookup_resource_kind(kind)?.resource, api_version_typed()))
}

fn api_version_typed<T: Resource>() -> String {
    api_version(T::API_PATHS)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{Duration, Utc};
    use flotilla_protocol::{NodeId, ResourceRef};

    use super::*;
    use crate::{
        Convoy, ConvoySpec, Demand, DemandAddressee, DemandKind, DemandSpec, HostSpec, InMemoryBackend, InputMeta, PrincipalRef, Regard,
        RegardExpiryPolicy, RegardSource, RegardSpec,
    };

    #[tokio::test]
    async fn list_resource_kind_returns_k8s_wire_list() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .using::<Convoy>("flotilla")
            .create(&InputMeta::builder().name("demo".to_string()).build(), &ConvoySpec::builder().workflow_ref("wf".to_string()).build())
            .await
            .expect("create convoy");

        let listed = list_resource_kind(&backend, "flotilla", "convoys").await.expect("list convoys");

        assert_eq!(listed.kind, "Convoy");
        assert_eq!(listed.value["items"][0]["apiVersion"], "flotilla.work/v1");
        assert_eq!(listed.value["items"][0]["kind"], "Convoy");
        assert_eq!(listed.value["items"][0]["metadata"]["name"], "demo");
    }

    #[tokio::test]
    async fn patch_resource_status_decodes_the_registered_kind() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .using::<Usage>("flotilla")
            .create(&InputMeta::builder().name("usage-account".to_string()).build(), &crate::UsageSpec {
                provider: "codex".to_string(),
                account: "ada@example.com".to_string(),
            })
            .await
            .expect("create usage");

        let patched = patch_resource_status(
            &backend,
            "flotilla",
            "usages",
            "usage-account",
            serde_json::json!({
                "windows": [{"name": "weekly", "used_percent": 42.0}],
                "observed_at": "2026-08-10T10:00:00Z",
            }),
        )
        .await
        .expect("patch typed Usage status");

        assert_eq!(patched.kind, "Usage");
        assert_eq!(patched.value["status"]["windows"][0]["used_percent"], 42.0);

        let error = patch_resource_status(&backend, "flotilla", "usages", "usage-account", serde_json::json!({"windows": []}))
            .await
            .expect_err("malformed status should fail typed decoding");
        assert!(error.to_string().contains("decode Usage status"));
    }

    #[tokio::test]
    async fn dynamic_delete_emits_the_normal_event_and_drops_an_in_memory_peer_replica() {
        let source = ResourceBackend::InMemory(InMemoryBackend::default());
        let peer = ResourceBackend::InMemory(InMemoryBackend::default());
        let convoys = source.using::<Convoy>("flotilla");
        convoys
            .create(&InputMeta::builder().name("ghost".to_string()).build(), &ConvoySpec::builder().workflow_ref("wf".to_string()).build())
            .await
            .expect("create source convoy");
        let listed = convoys.list().await.expect("list source convoys");
        let origin = NodeId::new("kiwi-root");
        let replica = peer.replica_writer::<Convoy>(origin, "flotilla");
        replica.replace(&listed, Utc::now()).await.expect("seed peer replica");
        let mut watch = convoys.watch(WatchStart::resuming_from(&listed)).await.expect("watch source");

        let deleted = delete_resource_kind(&source, "flotilla", "convoys", "ghost").await.expect("delete exact convoy");
        assert_eq!(deleted.object.value["metadata"]["name"], "ghost");
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), watch.next())
            .await
            .expect("delete watch event timeout")
            .expect("delete watch ended")
            .expect("delete watch event");
        assert!(matches!(event, WatchEvent::Deleted(ref object) if object.metadata.name == "ghost"));
        replica.apply(event, Utc::now()).await.expect("apply delete to peer");

        let replicas = peer.including_replicas::<Convoy>("flotilla").list().await.expect("list peer replicas");
        assert!(replicas.items.is_empty(), "normal delete propagation must remove the peer replica");
    }

    #[tokio::test]
    async fn host_resource_list_marks_stale_heartbeat_not_ready() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let hosts = backend.using::<Host>("flotilla");
        let host = hosts.create(&InputMeta::builder().name("feta".to_string()).build(), &HostSpec::default()).await.expect("create host");
        hosts
            .update_status("feta", &host.metadata.resource_version, &HostStatus {
                capabilities: BTreeMap::new(),
                heartbeat_at: Some(Utc::now() - Duration::seconds(61)),
                ready: true,
                resource_store: None,
                ..HostStatus::default()
            })
            .await
            .expect("write stale heartbeat");

        let listed = list_resource_kind(&backend, "flotilla", "hosts").await.expect("list hosts");

        assert_eq!(listed.value["items"][0]["status"]["ready"], false);
        assert!(listed.value["items"][0]["status"]["heartbeat_at"].is_string());
    }

    #[tokio::test]
    async fn host_replica_resource_list_derives_ready_from_origin_heartbeat() {
        let source = ResourceBackend::InMemory(InMemoryBackend::default());
        let source_hosts = source.using::<Host>("flotilla");
        let host = source_hosts
            .create(&InputMeta::builder().name("feta".to_string()).build(), &HostSpec::default())
            .await
            .expect("create source host");
        source_hosts
            .update_status("feta", &host.metadata.resource_version, &HostStatus {
                capabilities: BTreeMap::new(),
                heartbeat_at: Some(Utc::now() - Duration::seconds(61)),
                ready: true,
                resource_store: None,
                ..HostStatus::default()
            })
            .await
            .expect("write source stale heartbeat");
        let source_list = source_hosts.list().await.expect("list source hosts");
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .replica_writer::<Host>(NodeId::new("feta-node"), "flotilla")
            .replace(&source_list, Utc::now())
            .await
            .expect("write host replica");

        let listed = list_resource_kind_including_replicas(&backend, "flotilla", "hosts").await.expect("list host replicas");

        assert_eq!(listed.value["items"][0]["status"]["ready"], false);
        assert_eq!(listed.value["items"][0]["metadata"]["annotations"][ORIGIN_ROOT_ANNOTATION], "feta-node");
    }

    #[tokio::test]
    async fn unknown_kind_reports_registered_plural_names() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let error = list_resource_kind(&backend, "flotilla", "nope").await.expect_err("unknown kind should fail");
        let message = error.to_string();
        assert!(message.contains("unknown resource kind 'nope'"));
        assert!(message.contains("convoys"));
    }

    #[tokio::test]
    async fn attention_resource_kinds_are_registered_for_dynamic_listing() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        backend
            .using::<Regard>("flotilla")
            .create(
                &InputMeta::builder().name("principal-default-convoy-demo".to_string()).build(),
                &RegardSpec::builder()
                    .principal_ref(PrincipalRef::implicit_for_namespace("flotilla"))
                    .target(ResourceRef::new("flotilla.work/v1", "Convoy", "flotilla", "demo"))
                    .source(RegardSource::Expressed)
                    .expiry(RegardExpiryPolicy::Pin)
                    .build(),
            )
            .await
            .expect("create regard");
        backend
            .using::<Demand>("flotilla")
            .create(
                &InputMeta::builder().name("demo-permission".to_string()).build(),
                &DemandSpec::builder()
                    .originating_work_ref(ResourceRef::new("flotilla.work/v1", "Vessel", "flotilla", "demo-implement"))
                    .kind(DemandKind::Permission)
                    .addressee(DemandAddressee::Principal { principal_ref: PrincipalRef::implicit_for_namespace("flotilla") })
                    .build(),
            )
            .await
            .expect("create demand");

        let regards = list_resource_kind(&backend, "flotilla", "regards").await.expect("list regards dynamically");
        let demands = list_resource_kind(&backend, "flotilla", "demands").await.expect("list demands dynamically");

        assert_eq!(regards.kind, "Regard");
        assert_eq!(regards.value["items"][0]["kind"], "Regard");
        assert_eq!(demands.kind, "Demand");
        assert_eq!(demands.value["items"][0]["kind"], "Demand");
    }

    #[tokio::test]
    async fn apply_resource_document_creates_registered_resources() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let applied = apply_resource_document(
            &backend,
            "flotilla",
            json!({
                "apiVersion": "flotilla.work/v1",
                "kind": "Demand",
                "metadata": {
                    "name": "demo-review"
                },
                "spec": {
                    "originating_work_ref": {
                        "api_version": "flotilla.work/v1",
                        "kind": "Vessel",
                        "namespace": "flotilla",
                        "name": "demo-review"
                    },
                    "kind": "review",
                    "addressee": {
                        "kind": "principal",
                        "principal_ref": { "namespace": "flotilla", "name": "implicit" }
                    }
                }
            }),
        )
        .await
        .expect("apply demand document");

        assert_eq!(applied.kind, "Demand");
        assert_eq!(applied.value["metadata"]["name"], "demo-review");
        assert_eq!(backend.using::<Demand>("flotilla").get("demo-review").await.expect("stored demand").spec.kind, DemandKind::Review);
    }

    #[tokio::test]
    async fn apply_resource_document_reapply_preserves_existing_metadata_when_omitted() {
        let backend = ResourceBackend::InMemory(InMemoryBackend::default());
        let resolver = backend.using::<Demand>("flotilla");
        let owner_reference = OwnerReference {
            api_version: "flotilla.work/v1".to_string(),
            kind: "Vessel".to_string(),
            name: "demo-implement".to_string(),
            controller: true,
        };
        resolver
            .create(
                &InputMeta::builder()
                    .name("demo-review".to_string())
                    .labels(BTreeMap::from([("controller".to_string(), "attention".to_string())]))
                    .owner_references(vec![owner_reference.clone()])
                    .finalizers(vec!["attention.flotilla.work/finalizer".to_string()])
                    .build(),
                &DemandSpec::builder()
                    .originating_work_ref(ResourceRef::new("flotilla.work/v1", "Vessel", "flotilla", "demo-implement"))
                    .kind(DemandKind::Permission)
                    .addressee(DemandAddressee::Principal { principal_ref: PrincipalRef::implicit_for_namespace("flotilla") })
                    .build(),
            )
            .await
            .expect("create demand with metadata");

        let reapply_review_document = || {
            json!({
                "apiVersion": "flotilla.work/v1",
                "kind": "Demand",
                "metadata": {
                    "name": "demo-review"
                },
                "spec": {
                    "originating_work_ref": {
                        "api_version": "flotilla.work/v1",
                        "kind": "Vessel",
                        "namespace": "flotilla",
                        "name": "demo-implement"
                    },
                    "kind": "review",
                    "addressee": {
                        "kind": "principal",
                        "principal_ref": { "namespace": "flotilla", "name": "implicit" }
                    }
                }
            })
        };

        apply_resource_document(&backend, "flotilla", reapply_review_document()).await.expect("reapply demand document");

        let updated = resolver.get("demo-review").await.expect("updated demand");
        assert_eq!(updated.spec.kind, DemandKind::Review);
        assert_eq!(updated.metadata.owner_references, vec![owner_reference]);
        assert_eq!(updated.metadata.labels.get("controller").map(String::as_str), Some("attention"));
        assert_eq!(updated.metadata.finalizers, vec!["attention.flotilla.work/finalizer"]);

        resolver.delete("demo-review").await.expect("mark demand for deletion");
        let deleting = resolver.get("demo-review").await.expect("deleting demand");
        let deletion_timestamp = deleting.metadata.deletion_timestamp.expect("deletion timestamp should be set");

        apply_resource_document(&backend, "flotilla", reapply_review_document()).await.expect("reapply pending-delete demand document");

        let reapplied = resolver.get("demo-review").await.expect("reapplied pending-delete demand");
        assert_eq!(reapplied.metadata.deletion_timestamp, Some(deletion_timestamp));
    }
}
