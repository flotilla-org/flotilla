use serde::{Deserialize, Serialize};

use crate::{
    qualified_path::QualifiedPath, AttachableId, AttachableSet, AttachableSetId, ChangeRequest, Checkout, CloudAgentSession, Issue,
    ManagedTerminal, Workspace,
};

/// Operation on a keyed collection entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", content = "value")]
pub enum EntryOp<T> {
    #[serde(rename = "added")]
    Added(T),
    #[serde(rename = "updated")]
    Updated(T),
    #[serde(rename = "removed")]
    Removed,
}

/// Status of a git branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchStatus {
    Remote,
    Merged,
}

/// A git branch with status metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    pub status: BranchStatus,
}

/// A single change within a delta.
#[derive(Debug, Clone, Serialize, Deserialize)]
// TODO: revisit once checkout/work-item identity finishes migrating away from
// dual HostPath/QualifiedPath publication state.
#[allow(clippy::large_enum_variant)]
pub enum Change {
    Checkout { key: QualifiedPath, op: EntryOp<Checkout> },
    ChangeRequest { key: String, op: EntryOp<ChangeRequest> },
    Issue { key: String, op: EntryOp<Issue> },
    Session { key: String, op: EntryOp<CloudAgentSession> },
    Workspace { key: String, op: EntryOp<Workspace> },
    AttachableSet { key: AttachableSetId, op: EntryOp<AttachableSet> },
    Branch { key: String, op: EntryOp<Branch> },
    ManagedTerminal { key: AttachableId, op: EntryOp<ManagedTerminal> },
    ProviderHealth { category: String, provider: String, op: EntryOp<bool> },
}

/// A single entry in the per-repo delta log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaEntry {
    pub seq: u64,
    pub prev_seq: u64,
    pub changes: Vec<Change>,
}
