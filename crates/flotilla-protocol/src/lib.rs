pub mod commands;
pub mod provider_data;
pub mod snapshot;

use serde::{Deserialize, Serialize};

pub use commands::{Command, CommandResult, DeleteInfo};
pub use provider_data::{
    AheadBehind, AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession,
    CommitInfo, CorrelationKey, Issue, ProviderData, SessionStatus, Workspace, WorkingTreeStatus,
};
pub use snapshot::{
    CategoryLabels, CheckoutRef, ProviderError, RepoInfo, RepoLabels, Snapshot, WorkItem,
    WorkItemIdentity, WorkItemKind,
};

/// Top-level message envelope for the JSON protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "request")]
    Request {
        id: u64,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    #[serde(rename = "response")]
    Response { id: u64, result: CommandResult },
    #[serde(rename = "event")]
    Event { event: Box<DaemonEvent> },
}

/// Events pushed from daemon to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    #[serde(rename = "snapshot")]
    Snapshot(Box<Snapshot>),
    #[serde(rename = "repo_added")]
    RepoAdded(Box<RepoInfo>),
    #[serde(rename = "repo_removed")]
    RepoRemoved { path: std::path::PathBuf },
    /// Async command completion notification for socket subscribers (Step 2).
    /// Not emitted in the in-process path where results are returned directly.
    #[serde(rename = "command_result")]
    CommandResult {
        repo: std::path::PathBuf,
        result: commands::CommandResult,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn message_request_roundtrip() {
        let msg = Message::Request {
            id: 42,
            method: "subscribe".to_string(),
            params: serde_json::json!({"repo": "/tmp/my-repo"}),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Request { id, method, params } => {
                assert_eq!(id, 42);
                assert_eq!(method, "subscribe");
                assert_eq!(params["repo"], "/tmp/my-repo");
            }
            other => panic!("expected Request, got {:?}", other),
        }
    }

    #[test]
    fn message_event_snapshot_roundtrip() {
        let snapshot = Snapshot {
            seq: 7,
            repo: PathBuf::from("/tmp/my-repo"),
            work_items: vec![WorkItem {
                kind: WorkItemKind::Checkout,
                identity: WorkItemIdentity::Checkout(PathBuf::from("/tmp/my-repo/wt")),
                branch: Some("feature-x".to_string()),
                description: "Feature X".to_string(),
                checkout: Some(CheckoutRef {
                    key: PathBuf::from("/tmp/my-repo/wt"),
                    is_main_worktree: false,
                }),
                pr_key: Some("PR#10".to_string()),
                session_key: None,
                issue_keys: vec!["ISSUE-1".to_string()],
                workspace_refs: vec![],
                is_main_worktree: false,
            }],
            providers: ProviderData::default(),
            provider_health: HashMap::from([
                ("git".to_string(), true),
                ("github".to_string(), false),
            ]),
            errors: vec![ProviderError {
                category: "github".to_string(),
                message: "rate limited".to_string(),
            }],
        };
        let msg = Message::Event {
            event: Box::new(DaemonEvent::Snapshot(Box::new(snapshot))),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        let deserialized: Message = serde_json::from_str(&json).expect("deserialize");
        match deserialized {
            Message::Event { event } => match *event {
                DaemonEvent::Snapshot(snap) => {
                    let snap = *snap;
                    assert_eq!(snap.seq, 7);
                    assert_eq!(snap.repo, PathBuf::from("/tmp/my-repo"));
                    assert_eq!(snap.work_items.len(), 1);
                    assert_eq!(snap.work_items[0].branch.as_deref(), Some("feature-x"));
                    assert_eq!(snap.work_items[0].kind, WorkItemKind::Checkout);
                    assert_eq!(snap.provider_health["git"], true);
                    assert_eq!(snap.provider_health["github"], false);
                    assert_eq!(snap.errors.len(), 1);
                    assert_eq!(snap.errors[0].category, "github");
                }
                other => panic!("expected Snapshot event, got {:?}", other),
            },
            other => panic!("expected Event, got {:?}", other),
        }
    }

    #[test]
    fn command_result_variants_roundtrip() {
        let variants = vec![
            CommandResult::Ok,
            CommandResult::WorktreeCreated {
                branch: "feat-abc".to_string(),
            },
            CommandResult::BranchNameGenerated {
                name: "feat/cool-thing".to_string(),
                issue_ids: vec![
                    ("github".to_string(), "42".to_string()),
                    ("linear".to_string(), "ABC-123".to_string()),
                ],
            },
            CommandResult::DeleteInfo(DeleteInfo {
                branch: "old-branch".to_string(),
                pr_status: Some("merged".to_string()),
                merge_commit_sha: Some("abc123".to_string()),
                unpushed_commits: vec!["def456".to_string()],
                has_uncommitted: false,
                base_detection_warning: None,
            }),
            CommandResult::Error {
                message: "something went wrong".to_string(),
            },
        ];

        for variant in &variants {
            let json = serde_json::to_string(variant).expect("serialize");
            let deserialized: CommandResult = serde_json::from_str(&json).expect("deserialize");
            // Verify by re-serializing and comparing JSON
            let json2 = serde_json::to_string(&deserialized).expect("re-serialize");
            assert_eq!(json, json2, "roundtrip mismatch for variant");
        }

        // Also spot-check specific fields
        if let CommandResult::WorktreeCreated { branch } = &variants[1] {
            assert_eq!(branch, "feat-abc");
        }
        if let CommandResult::DeleteInfo(info) = &variants[3] {
            assert_eq!(info.branch, "old-branch");
            assert_eq!(info.pr_status.as_deref(), Some("merged"));
            assert!(!info.has_uncommitted);
        }
    }

    #[test]
    fn work_item_roundtrip() {
        let item = WorkItem {
            kind: WorkItemKind::Checkout,
            identity: WorkItemIdentity::Checkout(PathBuf::from("/repos/my-project/wt-1")),
            branch: Some("feature-login".to_string()),
            description: "Implement login flow".to_string(),
            checkout: Some(CheckoutRef {
                key: PathBuf::from("/repos/my-project/wt-1"),
                is_main_worktree: false,
            }),
            pr_key: Some("PR#55".to_string()),
            session_key: Some("sess-abc".to_string()),
            issue_keys: vec!["GH-10".to_string(), "LIN-20".to_string()],
            workspace_refs: vec!["cmux-1".to_string()],
            is_main_worktree: false,
        };

        let json = serde_json::to_string(&item).expect("serialize");
        let deserialized: WorkItem = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.kind, WorkItemKind::Checkout);
        assert_eq!(
            deserialized.identity,
            WorkItemIdentity::Checkout(PathBuf::from("/repos/my-project/wt-1"))
        );
        assert_eq!(deserialized.branch.as_deref(), Some("feature-login"));
        assert_eq!(deserialized.description, "Implement login flow");
        assert!(deserialized.checkout.is_some());
        let checkout = deserialized.checkout.unwrap();
        assert_eq!(checkout.key, PathBuf::from("/repos/my-project/wt-1"));
        assert!(!checkout.is_main_worktree);
        assert_eq!(deserialized.pr_key.as_deref(), Some("PR#55"));
        assert_eq!(deserialized.session_key.as_deref(), Some("sess-abc"));
        assert_eq!(deserialized.issue_keys, vec!["GH-10", "LIN-20"]);
        assert_eq!(deserialized.workspace_refs, vec!["cmux-1"]);
        assert!(!deserialized.is_main_worktree);
    }
}
