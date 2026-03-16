use std::path::PathBuf;

use flotilla_protocol::{HostName, HostPath, TerminalStatus};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachableSetId(pub String);

impl std::fmt::Display for AttachableSetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachableId(pub String);

impl std::fmt::Display for AttachableId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachableKind {
    Terminal(TerminalAttachable),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalPurpose {
    pub checkout: String,
    pub role: String,
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalAttachable {
    pub purpose: TerminalPurpose,
    #[serde(default)]
    pub command: String,
    pub working_directory: PathBuf,
    pub status: TerminalStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachableSet {
    pub id: AttachableSetId,
    #[serde(default)]
    pub host_affinity: Option<HostName>,
    #[serde(default)]
    pub checkout: Option<HostPath>,
    #[serde(default)]
    pub template_identity: Option<String>,
    #[serde(default)]
    pub members: Vec<AttachableId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachable {
    pub id: AttachableId,
    pub set_id: AttachableSetId,
    pub kind: AttachableKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BindingObjectKind {
    AttachableSet,
    Attachable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBinding {
    pub provider_category: String,
    pub provider_name: String,
    pub object_kind: BindingObjectKind,
    pub object_id: String,
    pub external_ref: String,
}
