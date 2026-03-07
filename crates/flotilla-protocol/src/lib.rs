pub mod commands;
pub mod provider_data;
pub mod snapshot;

use serde::{Deserialize, Serialize};

pub use commands::{Command, CommandResult, DeleteInfo};
pub use provider_data::{
    AheadBehind, AssociationKey, ChangeRequest, ChangeRequestStatus, Checkout, CloudAgentSession,
    CommitInfo, CorrelationKey, Issue, ProviderData, SessionStatus, WorkingTreeStatus, Workspace,
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
                debug_group: vec![],
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

}
