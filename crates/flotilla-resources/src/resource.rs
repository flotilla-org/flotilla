use std::{collections::BTreeMap, fmt::Debug};

use chrono::{DateTime, Utc};
use facet::Facet;
use flotilla_protocol::NodeId;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    error::ResourceError,
    labels::{LifecycleAuthority, AUTHORITY_LABEL},
    status_patch::StatusPatch,
    watch::{ResourceList, WatchEvent},
};

macro_rules! define_resource {
    ($name:ident, $plural:literal, $spec:ty, $status:ty, $patch:ty, replication = $replication:expr, facet) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, facet::Facet)]
        pub struct $name;

        impl $crate::resource::Resource for $name {
            type Spec = $spec;
            type Status = $status;
            type StatusPatch = $patch;

            const API_PATHS: $crate::resource::ApiPaths =
                $crate::resource::ApiPaths { group: "flotilla.work", version: "v1", plural: $plural, kind: stringify!($name) };
            const REPLICATION_CLASS: $crate::ReplicationClass = $replication;
        }
    };
    ($name:ident, $plural:literal, $spec:ty, $status:ty, $patch:ty, validate_spec_update = $validator:path) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name;

        impl $crate::resource::Resource for $name {
            type Spec = $spec;
            type Status = $status;
            type StatusPatch = $patch;

            const API_PATHS: $crate::resource::ApiPaths =
                $crate::resource::ApiPaths { group: "flotilla.work", version: "v1", plural: $plural, kind: stringify!($name) };

            fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), $crate::ResourceError> {
                $validator(current, requested)
            }
        }
    };
    ($name:ident, $plural:literal, $spec:ty, $status:ty, $patch:ty, replication = $replication:expr) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name;

        impl $crate::resource::Resource for $name {
            type Spec = $spec;
            type Status = $status;
            type StatusPatch = $patch;

            const API_PATHS: $crate::resource::ApiPaths =
                $crate::resource::ApiPaths { group: "flotilla.work", version: "v1", plural: $plural, kind: stringify!($name) };
            const REPLICATION_CLASS: $crate::ReplicationClass = $replication;
        }
    };
    ($name:ident, $plural:literal, $spec:ty, $status:ty, $patch:ty, immutable_spec) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name;

        impl $crate::resource::Resource for $name {
            type Spec = $spec;
            type Status = $status;
            type StatusPatch = $patch;

            const API_PATHS: $crate::resource::ApiPaths =
                $crate::resource::ApiPaths { group: "flotilla.work", version: "v1", plural: $plural, kind: stringify!($name) };

            fn validate_spec_update(current: &Self::Spec, requested: &Self::Spec) -> Result<(), $crate::ResourceError> {
                let current = serde_json::to_value(current)
                    .map_err(|error| $crate::ResourceError::decode(format!("serialize current spec: {error}")))?;
                let requested = serde_json::to_value(requested)
                    .map_err(|error| $crate::ResourceError::decode(format!("serialize requested spec: {error}")))?;
                if current == requested {
                    Ok(())
                } else {
                    Err($crate::ResourceError::invalid(concat!(stringify!($name), " spec is immutable after creation")))
                }
            }
        }
    };
    ($name:ident, $plural:literal, $spec:ty, $status:ty, $patch:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name;

        impl $crate::resource::Resource for $name {
            type Spec = $spec;
            type Status = $status;
            type StatusPatch = $patch;

            const API_PATHS: $crate::resource::ApiPaths =
                $crate::resource::ApiPaths { group: "flotilla.work", version: "v1", plural: $plural, kind: stringify!($name) };
        }
    };
}

pub(crate) use define_resource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiPaths {
    pub group: &'static str,
    pub version: &'static str,
    pub plural: &'static str,
    pub kind: &'static str,
}

pub trait Resource: Send + Sync + 'static {
    type Spec: Serialize + DeserializeOwned + Send + Sync + Debug + Clone;
    type Status: Serialize + DeserializeOwned + Send + Sync + Debug + Clone;
    type StatusPatch: StatusPatch<Self::Status>;

    const API_PATHS: ApiPaths;
    const REPLICATION_CLASS: crate::ReplicationClass = crate::ReplicationClass::None;

    fn validate_spec_update(_current: &Self::Spec, _requested: &Self::Spec) -> Result<(), ResourceError> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Facet)]
pub struct OwnerReference {
    #[serde(rename = "apiVersion")]
    #[facet(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub controller: bool,
}

/// One causally unique write in a definitions-class record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Facet)]
pub struct CausalDot {
    pub author_root: NodeId,
    pub author_counter: u64,
}

/// Causal state captured when a field was last authored.
///
/// The field-local context keeps a later disjoint-field edit from
/// accidentally resolving an existing conflict in this field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Facet)]
pub struct FieldMergeMetadata {
    pub dot: CausalDot,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<NodeId, u64>,
    pub written_at: DateTime<Utc>,
}

/// One causally-maximal value shown when a definitions field conflicts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Facet)]
pub struct MergeConflictSibling {
    #[facet(opaque)]
    pub value: serde_json::Value,
    pub dot: CausalDot,
    pub written_at: DateTime<Utc>,
}

/// Store-authored causal metadata for a definitions-class object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Facet)]
pub struct MergeMetadata {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, FieldMergeMetadata>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<NodeId, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub conflicts: BTreeMap<String, Vec<MergeConflictSibling>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, bon::Builder)]
pub struct InputMeta {
    pub name: String,
    #[builder(default)]
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[builder(default)]
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[builder(default)]
    #[serde(default, rename = "ownerReferences", skip_serializing_if = "Vec::is_empty")]
    pub owner_references: Vec<OwnerReference>,
    #[builder(default)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finalizers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_timestamp: Option<DateTime<Utc>>,
}

impl InputMeta {
    pub fn lifecycle_authority(&self) -> Result<Option<LifecycleAuthority>, ResourceError> {
        self.labels
            .get(AUTHORITY_LABEL)
            .map(|value| LifecycleAuthority::from_label_value(value).map_err(ResourceError::invalid))
            .transpose()
    }

    pub fn set_lifecycle_authority(&mut self, authority: LifecycleAuthority) {
        self.labels.insert(AUTHORITY_LABEL.to_string(), authority.as_label_value().to_string());
    }

    pub fn with_lifecycle_authority(mut self, authority: LifecycleAuthority) -> Self {
        self.set_lifecycle_authority(authority);
        self
    }

    pub fn with_added_finalizer(mut self, finalizer: impl Into<String>) -> Self {
        let finalizer = finalizer.into();
        if self.finalizers.iter().all(|existing| existing != &finalizer) {
            self.finalizers.push(finalizer);
        }
        self
    }

    pub fn without_finalizer(mut self, finalizer: &str) -> Self {
        self.finalizers.retain(|existing| existing != finalizer);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Facet)]
pub struct ObjectMeta {
    pub name: String,
    pub namespace: String,
    pub resource_version: String,
    pub labels: BTreeMap<String, String>,
    pub annotations: BTreeMap<String, String>,
    #[serde(default, rename = "ownerReferences", skip_serializing_if = "Vec::is_empty")]
    #[facet(rename = "ownerReferences")]
    pub owner_references: Vec<OwnerReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finalizers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deletion_timestamp: Option<DateTime<Utc>>,
    pub creation_timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeMetadata>,
}

impl ObjectMeta {
    pub fn lifecycle_authority(&self) -> Result<Option<LifecycleAuthority>, ResourceError> {
        self.labels
            .get(AUTHORITY_LABEL)
            .map(|value| LifecycleAuthority::from_label_value(value).map_err(ResourceError::invalid))
            .transpose()
    }

    pub fn is_pending_finalization(&self) -> bool {
        !self.finalizers.is_empty() && self.deletion_timestamp.is_some()
    }

    pub fn set_lifecycle_authority(&mut self, authority: LifecycleAuthority) {
        self.labels.insert(AUTHORITY_LABEL.to_string(), authority.as_label_value().to_string());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Facet)]
#[facet(where T::Spec: Facet<'static>, T::Status: Facet<'static>)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub struct ResourceObject<T: Resource> {
    pub metadata: ObjectMeta,
    pub spec: T::Spec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<T::Status>,
}

impl<T: Resource> ResourceObject<T> {
    pub(crate) fn matches_update(&self, meta: &InputMeta, spec: &T::Spec) -> Result<bool, ResourceError> {
        let current_spec =
            serde_json::to_value(&self.spec).map_err(|err| ResourceError::decode(format!("serialize current spec: {err}")))?;
        let requested_spec = serde_json::to_value(spec).map_err(|err| ResourceError::decode(format!("serialize requested spec: {err}")))?;
        Ok(self.metadata.name == meta.name
            && self.metadata.labels == meta.labels
            && self.metadata.annotations == meta.annotations
            && self.metadata.owner_references == meta.owner_references
            && self.metadata.finalizers == meta.finalizers
            && self.metadata.deletion_timestamp == meta.deletion_timestamp
            && current_spec == requested_spec)
    }

    pub(crate) fn matches_status(&self, status: &T::Status) -> Result<bool, ResourceError> {
        let Some(current_status) = &self.status else { return Ok(false) };
        let current =
            serde_json::to_value(current_status).map_err(|err| ResourceError::decode(format!("serialize current status: {err}")))?;
        let requested = serde_json::to_value(status).map_err(|err| ResourceError::decode(format!("serialize requested status: {err}")))?;
        Ok(current == requested)
    }

    pub fn to_k8s_object(&self) -> K8sResourceObject<T> {
        K8sResourceObject {
            api_version: api_version(T::API_PATHS),
            kind: T::API_PATHS.kind.to_string(),
            metadata: K8sObjectMeta::from(&self.metadata),
            spec: self.spec.clone(),
            status: self.status.clone(),
        }
    }

    pub fn from_k8s_object(value: K8sResourceObject<T>) -> Result<Self, ResourceError> {
        let expected_api_version = api_version(T::API_PATHS);
        if value.api_version != expected_api_version {
            return Err(ResourceError::decode(format!(
                "unexpected apiVersion '{}', expected '{}'",
                value.api_version, expected_api_version
            )));
        }
        if value.kind != T::API_PATHS.kind {
            return Err(ResourceError::decode(format!("unexpected kind '{}', expected '{}'", value.kind, T::API_PATHS.kind)));
        }
        Ok(Self { metadata: value.metadata.into(), spec: value.spec, status: value.status })
    }
}

pub fn api_version(paths: ApiPaths) -> String {
    if paths.group.is_empty() {
        paths.version.to_string()
    } else {
        format!("{}/{}", paths.group, paths.version)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct K8sObjectMeta {
    pub name: String,
    pub namespace: String,
    #[serde(rename = "resourceVersion")]
    pub resource_version: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub annotations: BTreeMap<String, String>,
    #[serde(default, rename = "ownerReferences", skip_serializing_if = "Vec::is_empty")]
    pub owner_references: Vec<OwnerReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finalizers: Vec<String>,
    #[serde(default, rename = "deletionTimestamp", skip_serializing_if = "Option::is_none")]
    pub deletion_timestamp: Option<DateTime<Utc>>,
    #[serde(rename = "creationTimestamp")]
    pub creation_timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge: Option<MergeMetadata>,
}

impl From<&ObjectMeta> for K8sObjectMeta {
    fn from(value: &ObjectMeta) -> Self {
        Self {
            name: value.name.clone(),
            namespace: value.namespace.clone(),
            resource_version: value.resource_version.clone(),
            labels: value.labels.clone(),
            annotations: value.annotations.clone(),
            owner_references: value.owner_references.clone(),
            finalizers: value.finalizers.clone(),
            deletion_timestamp: value.deletion_timestamp,
            creation_timestamp: value.creation_timestamp,
            merge: value.merge.clone(),
        }
    }
}

impl From<K8sObjectMeta> for ObjectMeta {
    fn from(value: K8sObjectMeta) -> Self {
        Self {
            name: value.name,
            namespace: value.namespace,
            resource_version: value.resource_version,
            labels: value.labels,
            annotations: value.annotations,
            owner_references: value.owner_references,
            finalizers: value.finalizers,
            deletion_timestamp: value.deletion_timestamp,
            creation_timestamp: value.creation_timestamp,
            merge: value.merge,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub struct K8sResourceObject<T: Resource> {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    pub metadata: K8sObjectMeta,
    pub spec: T::Spec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<T::Status>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct K8sListMeta {
    #[serde(rename = "resourceVersion")]
    pub resource_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(
    serialize = "T::Spec: Serialize, T::Status: Serialize",
    deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"
))]
pub struct K8sResourceList<T: Resource> {
    pub metadata: K8sListMeta,
    pub items: Vec<K8sResourceObject<T>>,
}

impl<T: Resource> K8sResourceList<T> {
    pub fn into_resource_list(self) -> Result<ResourceList<T>, ResourceError> {
        Ok(ResourceList {
            items: self.items.into_iter().map(ResourceObject::from_k8s_object).collect::<Result<_, _>>()?,
            resource_version: self.metadata.resource_version,
            generation: self.metadata.generation,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct K8sInputMetadata<'a> {
    pub(crate) name: &'a str,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) labels: &'a BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) annotations: &'a BTreeMap<String, String>,
    #[serde(default, rename = "ownerReferences", skip_serializing_if = "Vec::is_empty")]
    pub(crate) owner_references: &'a Vec<OwnerReference>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) finalizers: &'a Vec<String>,
    #[serde(rename = "deletionTimestamp", skip_serializing_if = "Option::is_none")]
    pub(crate) deletion_timestamp: &'a Option<DateTime<Utc>>,
    #[serde(rename = "resourceVersion", skip_serializing_if = "Option::is_none")]
    pub(crate) resource_version: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(bound(serialize = "T::Spec: Serialize"))]
pub(crate) struct K8sInputResourceObject<'a, T: Resource> {
    #[serde(rename = "apiVersion")]
    pub(crate) api_version: String,
    pub(crate) kind: &'static str,
    pub(crate) metadata: K8sInputMetadata<'a>,
    pub(crate) spec: &'a T::Spec,
}

impl<'a, T: Resource> K8sInputResourceObject<'a, T> {
    pub(crate) fn for_spec(meta: &'a InputMeta, resource_version: Option<&'a str>, spec: &'a T::Spec) -> Self {
        Self {
            api_version: api_version(T::API_PATHS),
            kind: T::API_PATHS.kind,
            metadata: K8sInputMetadata {
                name: &meta.name,
                labels: &meta.labels,
                annotations: &meta.annotations,
                owner_references: &meta.owner_references,
                finalizers: &meta.finalizers,
                deletion_timestamp: &meta.deletion_timestamp,
                resource_version,
            },
            spec,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct K8sStatusMetadata<'a> {
    pub(crate) name: &'a str,
    #[serde(rename = "resourceVersion")]
    pub(crate) resource_version: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(bound(serialize = "T::Status: Serialize"))]
pub(crate) struct K8sStatusResourceObject<'a, T: Resource> {
    #[serde(rename = "apiVersion")]
    pub(crate) api_version: String,
    pub(crate) kind: &'static str,
    pub(crate) metadata: K8sStatusMetadata<'a>,
    pub(crate) status: &'a T::Status,
}

impl<'a, T: Resource> K8sStatusResourceObject<'a, T> {
    pub(crate) fn new(name: &'a str, resource_version: &'a str, status: &'a T::Status) -> Self {
        Self {
            api_version: api_version(T::API_PATHS),
            kind: T::API_PATHS.kind,
            metadata: K8sStatusMetadata { name, resource_version },
            status,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(bound(deserialize = "T::Spec: DeserializeOwned, T::Status: DeserializeOwned"))]
pub struct K8sWatchEvent<T: Resource> {
    #[serde(rename = "type")]
    pub event_type: String,
    pub object: K8sResourceObject<T>,
}

impl<T: Resource> K8sWatchEvent<T> {
    pub fn into_watch_event(self) -> Result<WatchEvent<T>, ResourceError> {
        let object = ResourceObject::from_k8s_object(self.object)?;
        match self.event_type.as_str() {
            "ADDED" => Ok(WatchEvent::Added(object)),
            "MODIFIED" => Ok(WatchEvent::Modified(object)),
            "DELETED" => Ok(WatchEvent::Deleted(object)),
            other => Err(ResourceError::decode(format!("unknown watch event type '{other}'"))),
        }
    }
}

impl From<&ObjectMeta> for InputMeta {
    fn from(value: &ObjectMeta) -> Self {
        Self {
            name: value.name.clone(),
            labels: value.labels.clone(),
            annotations: value.annotations.clone(),
            owner_references: value.owner_references.clone(),
            finalizers: value.finalizers.clone(),
            deletion_timestamp: value.deletion_timestamp,
        }
    }
}
