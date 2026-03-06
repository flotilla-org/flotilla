//! Conversion functions from core types to protocol types.
//!
//! This module is the serialization boundary between the rich in-process
//! core types and the flat, serde-friendly protocol types.

use std::path::Path;

use flotilla_protocol::{
    ProtoCheckoutRef, ProtoError, ProtoWorkItem, ProtoWorkItemIdentity, ProtoWorkItemKind, Snapshot,
};

use crate::data::{ProviderError, WorkItem, WorkItemIdentity, WorkItemKind};
use crate::refresh::RefreshSnapshot;

pub fn work_item_kind_to_proto(kind: WorkItemKind) -> ProtoWorkItemKind {
    match kind {
        WorkItemKind::Checkout => ProtoWorkItemKind::Checkout,
        WorkItemKind::Session => ProtoWorkItemKind::Session,
        WorkItemKind::Pr => ProtoWorkItemKind::Pr,
        WorkItemKind::RemoteBranch => ProtoWorkItemKind::RemoteBranch,
        WorkItemKind::Issue => ProtoWorkItemKind::Issue,
    }
}

pub fn work_item_identity_to_proto(identity: &WorkItemIdentity) -> ProtoWorkItemIdentity {
    match identity {
        WorkItemIdentity::Checkout(path) => ProtoWorkItemIdentity::Checkout(path.clone()),
        WorkItemIdentity::ChangeRequest(id) => ProtoWorkItemIdentity::ChangeRequest(id.clone()),
        WorkItemIdentity::Session(id) => ProtoWorkItemIdentity::Session(id.clone()),
        WorkItemIdentity::Issue(id) => ProtoWorkItemIdentity::Issue(id.clone()),
        WorkItemIdentity::RemoteBranch(branch) => {
            ProtoWorkItemIdentity::RemoteBranch(branch.clone())
        }
    }
}

pub fn work_item_to_proto(item: &WorkItem) -> ProtoWorkItem {
    let kind = work_item_kind_to_proto(item.kind());

    let identity = work_item_identity_to_proto(&item.identity());

    let checkout = item.checkout().map(|co| ProtoCheckoutRef {
        key: co.key.clone(),
        is_main_worktree: co.is_main_worktree,
    });

    ProtoWorkItem {
        kind,
        identity,
        branch: item.branch().map(|s| s.to_string()),
        description: item.description().to_string(),
        checkout,
        pr_key: item.pr_key().map(|s| s.to_string()),
        session_key: item.session_key().map(|s| s.to_string()),
        issue_keys: item.issue_keys().to_vec(),
        workspace_refs: item.workspace_refs().to_vec(),
        is_main_worktree: item.is_main_worktree(),
    }
}

pub fn error_to_proto(error: &ProviderError) -> ProtoError {
    ProtoError {
        category: error.category.to_string(),
        message: error.message.clone(),
    }
}

pub fn snapshot_to_proto(repo: &Path, seq: u64, refresh: &RefreshSnapshot) -> Snapshot {
    Snapshot {
        seq,
        repo: repo.to_path_buf(),
        work_items: refresh.work_items.iter().map(work_item_to_proto).collect(),
        provider_health: refresh
            .provider_health
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect(),
        errors: refresh.errors.iter().map(error_to_proto).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{
        CheckoutRef, CorrelatedAnchor, CorrelatedWorkItem, StandaloneWorkItem, WorkItem,
    };
    use std::path::PathBuf;

    #[test]
    fn convert_correlated_checkout() {
        let item = WorkItem::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from("/repos/my-project/wt-1"),
                is_main_worktree: false,
            }),
            branch: Some("feature-login".to_string()),
            description: "Implement login flow".to_string(),
            linked_pr: Some("PR#55".to_string()),
            linked_session: Some("sess-abc".to_string()),
            linked_issues: vec!["GH-10".to_string(), "LIN-20".to_string()],
            workspace_refs: vec!["cmux-1".to_string()],
            correlation_group_idx: 0,
        });

        let proto = work_item_to_proto(&item);

        assert_eq!(proto.kind, ProtoWorkItemKind::Checkout);
        assert_eq!(
            proto.identity,
            ProtoWorkItemIdentity::Checkout(PathBuf::from("/repos/my-project/wt-1"))
        );
        assert_eq!(proto.branch.as_deref(), Some("feature-login"));
        assert_eq!(proto.description, "Implement login flow");

        let checkout = proto.checkout.expect("should have checkout ref");
        assert_eq!(checkout.key, PathBuf::from("/repos/my-project/wt-1"));
        assert!(!checkout.is_main_worktree);

        assert_eq!(proto.pr_key.as_deref(), Some("PR#55"));
        assert_eq!(proto.session_key.as_deref(), Some("sess-abc"));
        assert_eq!(proto.issue_keys, vec!["GH-10", "LIN-20"]);
        assert_eq!(proto.workspace_refs, vec!["cmux-1"]);
        assert!(!proto.is_main_worktree);
    }

    #[test]
    fn convert_standalone_issue() {
        let item = WorkItem::Standalone(StandaloneWorkItem::Issue {
            key: "42".to_string(),
            description: "Fix the login bug".to_string(),
        });

        let proto = work_item_to_proto(&item);

        assert_eq!(proto.kind, ProtoWorkItemKind::Issue);
        assert_eq!(
            proto.identity,
            ProtoWorkItemIdentity::Issue("42".to_string())
        );
        assert_eq!(proto.description, "Fix the login bug");
        assert_eq!(proto.issue_keys, vec!["42"]);
        assert!(proto.branch.is_none());
        assert!(proto.checkout.is_none());
        assert!(proto.pr_key.is_none());
        assert!(proto.session_key.is_none());
        assert!(proto.workspace_refs.is_empty());
        assert!(!proto.is_main_worktree);
    }
}
