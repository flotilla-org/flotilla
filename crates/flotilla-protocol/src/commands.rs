use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Commands the client can send to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    SwitchWorktree {
        path: PathBuf,
    },
    SelectWorkspace {
        ws_ref: String,
    },
    CreateWorktree {
        branch: String,
        create_branch: bool,
        issue_ids: Vec<(String, String)>,
    },
    RemoveCheckout {
        branch: String,
    },
    FetchDeleteInfo {
        branch: String,
        worktree_path: Option<PathBuf>,
        pr_number: Option<String>,
    },
    OpenPr {
        id: String,
    },
    OpenIssueBrowser {
        id: String,
    },
    LinkIssuesToPr {
        pr_id: String,
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
    WorktreeCreated {
        branch: String,
    },
    BranchNameGenerated {
        name: String,
        issue_ids: Vec<(String, String)>,
    },
    DeleteInfo(DeleteInfo),
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteInfo {
    pub branch: String,
    pub pr_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
    pub base_detection_warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{de::DeserializeOwned, Serialize};

    fn assert_json_roundtrip<T>(value: &T)
    where
        T: Serialize + DeserializeOwned + std::fmt::Debug,
    {
        let json = serde_json::to_string(value).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        let json2 = serde_json::to_string(&decoded).expect("re-serialize");
        assert_eq!(json2, json, "JSON roundtrip mismatch");
    }

    #[test]
    fn command_roundtrip_covers_all_variants() {
        let cases = vec![
            Command::SwitchWorktree {
                path: PathBuf::from("/repos/project/wt-1"),
            },
            Command::SelectWorkspace {
                ws_ref: "cmux-session-1".into(),
            },
            Command::CreateWorktree {
                branch: "feat-new".into(),
                create_branch: true,
                issue_ids: vec![("github".into(), "42".into())],
            },
            Command::CreateWorktree {
                branch: "fix/bug".into(),
                create_branch: false,
                issue_ids: vec![],
            },
            Command::RemoveCheckout {
                branch: "old-branch".into(),
            },
            Command::FetchDeleteInfo {
                branch: "feat-done".into(),
                worktree_path: Some(PathBuf::from("/repos/proj/wt")),
                pr_number: Some("123".into()),
            },
            Command::FetchDeleteInfo {
                branch: "x".into(),
                worktree_path: None,
                pr_number: None,
            },
            Command::OpenPr { id: "55".into() },
            Command::OpenIssueBrowser { id: "GH-10".into() },
            Command::LinkIssuesToPr {
                pr_id: "PR-7".into(),
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
        let cmd = Command::SwitchWorktree {
            path: PathBuf::from("/x"),
        };
        let json = serde_json::to_value(&cmd).expect("serialize");
        assert_eq!(json.get("command").and_then(|v| v.as_str()), Some("switch_worktree"));
    }

    #[test]
    fn command_result_roundtrip_covers_all_variants() {
        let cases = vec![
            CommandResult::Ok,
            CommandResult::WorktreeCreated {
                branch: "feat-new".into(),
            },
            CommandResult::BranchNameGenerated {
                name: "feat/cool-thing".into(),
                issue_ids: vec![("gh".into(), "1".into())],
            },
            CommandResult::DeleteInfo(DeleteInfo {
                branch: "old".into(),
                pr_status: Some("merged".into()),
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
        let result = CommandResult::WorktreeCreated { branch: "x".into() };
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(
            json.get("status").and_then(|v| v.as_str()),
            Some("worktree_created")
        );
    }

    #[test]
    fn delete_info_default() {
        let info = DeleteInfo::default();
        assert_eq!(info.branch, "");
        assert!(info.pr_status.is_none());
        assert!(info.merge_commit_sha.is_none());
        assert!(info.unpushed_commits.is_empty());
        assert!(!info.has_uncommitted);
        assert!(info.base_detection_warning.is_none());
    }

    #[test]
    fn delete_info_roundtrip_preserves_fields() {
        let info = DeleteInfo {
            branch: "old-feat".into(),
            pr_status: Some("closed".into()),
            merge_commit_sha: Some("deadbeef".into()),
            unpushed_commits: vec!["aaa".into(), "bbb".into()],
            has_uncommitted: true,
            base_detection_warning: Some("ambiguous base".into()),
        };
        assert_json_roundtrip(&info);
    }
}
