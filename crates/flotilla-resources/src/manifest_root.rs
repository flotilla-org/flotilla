use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

use crate::{resource::define_resource, status_patch::StatusPatch, ReplicationClass, StalledCondition};

define_resource!(
    ManifestRoot,
    "manifestroots",
    ManifestRootSpec,
    ManifestRootStatus,
    ManifestRootStatusPatch,
    replication = ReplicationClass::HomeBoundRuntime
);

/// A path and the identity declared by one document. The JSON tuple encoding
/// permits arbitrary path and name characters while remaining a map key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocumentKey {
    pub path: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

impl Serialize for DocumentKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde_json::to_string(&(&self.path, &self.kind, &self.namespace, &self.name))
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for DocumentKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let (path, kind, namespace, name) = serde_json::from_str(&encoded).map_err(D::Error::custom)?;
        Ok(Self { path, kind, namespace, name })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, bon::Builder)]
pub struct ManifestRootSpec {
    pub host: String,
    pub path: String,
    pub source: String,
    #[serde(default)]
    #[builder(default)]
    pub suspended: BTreeSet<DocumentKey>,
    #[serde(default)]
    #[builder(default)]
    pub resolutions: BTreeMap<DocumentKey, Resolution>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub action: ResolutionAction,
    pub token: String,
    pub requested_by: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionAction {
    Sync,
    Adopt,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRootStatus {
    #[serde(default)]
    pub documents: BTreeMap<DocumentKey, DocumentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stalled: Option<StalledCondition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentState {
    pub phase: DocumentPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_hash: Option<String>,
    pub observed_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_outcome: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentPhase {
    Applied,
    Refused,
    Drifted,
    Suspended,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestRootStatusPatch {
    Replace(ManifestRootStatus),
}

impl StatusPatch<ManifestRootStatus> for ManifestRootStatusPatch {
    fn apply(&self, status: &mut ManifestRootStatus) {
        match self {
            Self::Replace(replacement) => *status = replacement.clone(),
        }
    }
}
