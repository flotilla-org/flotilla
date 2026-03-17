use std::path::PathBuf;

use flotilla_protocol::TerminalStatus;
pub use flotilla_protocol::{AttachableId, AttachableSet, AttachableSetId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachableContent {
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
pub struct Attachable {
    pub id: AttachableId,
    pub set_id: AttachableSetId,
    pub content: AttachableContent,
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
