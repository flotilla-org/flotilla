use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Commands the client can send to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    CreateWorkspaceForCheckout {
        checkout_path: PathBuf,
    },
    SelectWorkspace {
        ws_ref: String,
    },
    CreateCheckout {
        branch: String,
        create_branch: bool,
        issue_ids: Vec<(String, String)>,
    },
    RemoveCheckout {
        branch: String,
    },
    FetchCheckoutStatus {
        branch: String,
        checkout_path: Option<PathBuf>,
        change_request_id: Option<String>,
    },
    OpenChangeRequest {
        id: String,
    },
    OpenIssue {
        id: String,
    },
    LinkIssuesToChangeRequest {
        change_request_id: String,
        issue_ids: Vec<String>,
    },
    ArchiveSession {
        session_id: String,
    },
    GenerateBranchName {
        issue_keys: Vec<String>,
    },
    TeleportSession {
        session_id: String,
        branch: Option<String>,
        checkout_key: Option<PathBuf>,
    },
    AddRepo {
        path: PathBuf,
    },
    RemoveRepo {
        path: PathBuf,
    },
    Refresh,
}

/// Result returned from command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandResult {
    Ok,
    CheckoutCreated {
        branch: String,
    },
    BranchNameGenerated {
        name: String,
        issue_ids: Vec<(String, String)>,
    },
    CheckoutStatus(CheckoutStatus),
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckoutStatus {
    pub branch: String,
    pub change_request_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
    pub base_detection_warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::assert_json_roundtrip;

    #[test]
    fn command_roundtrip_covers_all_variants() {
        let cases = vec![
            Command::CreateWorkspaceForCheckout {
                checkout_path: PathBuf::from("/repos/project/wt-1"),
            },
            Command::SelectWorkspace {
                ws_ref: "cmux-session-1".into(),
            },
            Command::CreateCheckout {
                branch: "feat-new".into(),
                create_branch: true,
                issue_ids: vec![("github".into(), "42".into())],
            },
            Command::CreateCheckout {
                branch: "fix/bug".into(),
                create_branch: false,
                issue_ids: vec![],
            },
            Command::RemoveCheckout {
                branch: "old-branch".into(),
            },
            Command::FetchCheckoutStatus {
                branch: "feat-done".into(),
                checkout_path: Some(PathBuf::from("/repos/proj/wt")),
                change_request_id: Some("123".into()),
            },
            Command::FetchCheckoutStatus {
                branch: "x".into(),
                checkout_path: None,
                change_request_id: None,
            },
            Command::OpenChangeRequest { id: "55".into() },
            Command::OpenIssue { id: "GH-10".into() },
            Command::LinkIssuesToChangeRequest {
                change_request_id: "PR-7".into(),
                issue_ids: vec!["I-1".into(), "I-2".into()],
            },
            Command::ArchiveSession {
                session_id: "sess-abc".into(),
            },
            Command::GenerateBranchName {
                issue_keys: vec!["GH-1".into(), "LIN-5".into()],
            },
            Command::TeleportSession {
                session_id: "sess-1".into(),
                branch: Some("feat-x".into()),
                checkout_key: Some(PathBuf::from("/repos/wt")),
            },
            Command::TeleportSession {
                session_id: "sess-2".into(),
                branch: None,
                checkout_key: None,
            },
            Command::AddRepo {
                path: PathBuf::from("/new/repo"),
            },
            Command::RemoveRepo {
                path: PathBuf::from("/old/repo"),
            },
            Command::Refresh,
        ];

        for cmd in cases {
            assert_json_roundtrip(&cmd);
        }
    }

    #[test]
    fn command_uses_snake_case_tag() {
        let cmd = Command::SelectWorkspace { ws_ref: "x".into() };
        let json = serde_json::to_value(&cmd).expect("serialize");
        assert_eq!(
            json.get("command").and_then(|v| v.as_str()),
            Some("select_workspace")
        );
    }

    #[test]
    fn command_result_roundtrip_covers_all_variants() {
        let cases = vec![
            CommandResult::Ok,
            CommandResult::CheckoutCreated {
                branch: "feat-new".into(),
            },
            CommandResult::BranchNameGenerated {
                name: "feat/cool-thing".into(),
                issue_ids: vec![("gh".into(), "1".into())],
            },
            CommandResult::CheckoutStatus(CheckoutStatus {
                branch: "old".into(),
                change_request_status: Some("merged".into()),
                merge_commit_sha: Some("abc123".into()),
                unpushed_commits: vec!["def456".into()],
                has_uncommitted: true,
                base_detection_warning: Some("warning text".into()),
            }),
            CommandResult::Error {
                message: "something failed".into(),
            },
        ];

        for result in cases {
            assert_json_roundtrip(&result);
        }
    }

    #[test]
    fn command_result_uses_snake_case_tag() {
        let result = CommandResult::CheckoutCreated { branch: "x".into() };
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            json.get("status").and_then(|v| v.as_str()),
            Some("checkout_created")
        );
    }

    #[test]
    fn checkout_status_default() {
        let info = CheckoutStatus::default();
        assert_eq!(info.branch, "");
        assert!(info.change_request_status.is_none());
        assert!(info.merge_commit_sha.is_none());
        assert!(info.unpushed_commits.is_empty());
        assert!(!info.has_uncommitted);
        assert!(info.base_detection_warning.is_none());
    }

    #[test]
    fn checkout_status_roundtrip_preserves_fields() {
        let info = CheckoutStatus {
            branch: "old-feat".into(),
            change_request_status: Some("closed".into()),
            merge_commit_sha: Some("deadbeef".into()),
            unpushed_commits: vec!["aaa".into(), "bbb".into()],
            has_uncommitted: true,
            base_detection_warning: Some("ambiguous base".into()),
        };
        assert_json_roundtrip(&info);
    }
}
