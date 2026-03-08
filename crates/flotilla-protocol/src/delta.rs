use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{
    ChangeRequest, Checkout, CloudAgentSession, Issue, ProviderError, WorkItem, WorkItemIdentity,
    Workspace,
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
pub enum Change {
    Checkout {
        key: PathBuf,
        op: EntryOp<Checkout>,
    },
    ChangeRequest {
        key: String,
        op: EntryOp<ChangeRequest>,
    },
    Issue {
        key: String,
        op: EntryOp<Issue>,
    },
    Session {
        key: String,
        op: EntryOp<CloudAgentSession>,
    },
    Workspace {
        key: String,
        op: EntryOp<Workspace>,
    },
    Branch {
        key: String,
        op: EntryOp<Branch>,
    },
    WorkItem {
        identity: WorkItemIdentity,
        op: EntryOp<WorkItem>,
    },
    ProviderHealth {
        provider: String,
        op: EntryOp<bool>,
    },
    /// Full replacement — errors lack stable identity, so keyed deltas don't apply.
    ErrorsChanged(Vec<ProviderError>),
}

/// A single entry in the per-repo delta log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaEntry {
    pub seq: u64,
    pub prev_seq: u64,
    pub changes: Vec<Change>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_op_added_roundtrip() {
        let op: EntryOp<bool> = EntryOp::Added(true);
        let json = serde_json::to_string(&op).unwrap();
        let decoded: EntryOp<bool> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn entry_op_removed_roundtrip() {
        let op: EntryOp<String> = EntryOp::Removed;
        let json = serde_json::to_string(&op).unwrap();
        let decoded: EntryOp<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, op);
    }

    #[test]
    fn branch_status_roundtrip() {
        for status in [BranchStatus::Remote, BranchStatus::Merged] {
            let json = serde_json::to_string(&status).unwrap();
            let decoded: BranchStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, status);
        }
    }

    #[test]
    fn change_checkout_roundtrip() {
        let change = Change::Checkout {
            key: PathBuf::from("/repos/wt-1"),
            op: EntryOp::Added(Checkout {
                branch: "feat-x".into(),
                is_trunk: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![],
                association_keys: vec![],
            }),
        };
        let json = serde_json::to_string(&change).unwrap();
        let decoded: Change = serde_json::from_str(&json).unwrap();
        // Verify it round-trips (Change doesn't derive PartialEq, so check JSON)
        let json2 = serde_json::to_string(&decoded).unwrap();
        assert_eq!(json, json2);
    }

    #[test]
    fn change_branch_removed_roundtrip() {
        let change = Change::Branch {
            key: "feature/old".into(),
            op: EntryOp::Removed,
        };
        let json = serde_json::to_string(&change).unwrap();
        let decoded: Change = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&decoded).unwrap();
        assert_eq!(json, json2);
    }
}
