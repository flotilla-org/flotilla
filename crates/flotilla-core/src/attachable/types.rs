pub use flotilla_protocol::{AttachableId, AttachableSet, AttachableSetId};
use flotilla_protocol::{PaneExitAttention, PaneExitAttentionFlavor, TerminalStatus};
use serde::{Deserialize, Serialize};

use crate::path_context::ExecutionEnvironmentPath;

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
    pub working_directory: ExecutionEnvironmentPath,
    pub status: TerminalStatus,
    #[serde(default)]
    pub expected_to_persist: bool,
}

impl TerminalAttachable {
    pub fn exit_attention(&self) -> Option<PaneExitAttention> {
        let TerminalStatus::Exited(exit_code) = self.status else {
            return None;
        };
        Some(PaneExitAttention {
            flavor: if self.expected_to_persist { PaneExitAttentionFlavor::Failure } else { PaneExitAttentionFlavor::Completion },
            exit_code,
        })
    }
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
