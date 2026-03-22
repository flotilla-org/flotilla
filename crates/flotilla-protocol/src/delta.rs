use serde::{Deserialize, Serialize};

use crate::{
    AttachableSet, AttachableSetId, ChangeRequest, Checkout, CloudAgentSession, HostPath, Issue, ProviderError, WorkItem, WorkItemIdentity,
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
        key: HostPath,
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
    AttachableSet {
        key: AttachableSetId,
        op: EntryOp<AttachableSet>,
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
        category: String,
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
    /// Pre-correlated work items at this seq (needed for delta replay to clients).
    pub work_items: Vec<crate::snapshot::WorkItem>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        test_helpers::{assert_json_roundtrip, assert_roundtrip},
        HostName,
    };

    fn hp(path: &str) -> HostPath {
        HostPath::new(HostName::new("test-host"), PathBuf::from(path))
    }

    #[test]
    fn entry_op_added_roundtrip() {
        let op: EntryOp<bool> = EntryOp::Added(true);
        assert_roundtrip(&op);
    }

    #[test]
    fn entry_op_removed_roundtrip() {
        let op: EntryOp<String> = EntryOp::Removed;
        assert_roundtrip(&op);
    }

    #[test]
    fn branch_status_roundtrip() {
        for status in [BranchStatus::Remote, BranchStatus::Merged] {
            assert_roundtrip(&status);
        }
    }

    #[test]
    fn change_checkout_roundtrip() {
        let change = Change::Checkout {
            key: hp("/repos/wt-1"),
            op: EntryOp::Added(Checkout {
                branch: "feat-x".into(),
                is_main: false,
                trunk_ahead_behind: None,
                remote_ahead_behind: None,
                working_tree: None,
                last_commit: None,
                correlation_keys: vec![],
                association_keys: vec![],
            }),
        };
        assert_json_roundtrip(&change);
    }

    #[test]
    fn change_branch_removed_roundtrip() {
        let change = Change::Branch { key: "feature/old".into(), op: EntryOp::Removed };
        assert_json_roundtrip(&change);
    }

    #[test]
    fn change_branch_added_roundtrip() {
        let change = Change::Branch { key: "feat-new".into(), op: EntryOp::Added(Branch { status: BranchStatus::Remote }) };
        assert_json_roundtrip(&change);
    }
}
