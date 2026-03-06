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
    WorktreeCreated { branch: String },
    BranchNameGenerated { name: String, issue_ids: Vec<(String, String)> },
    DeleteInfo(DeleteInfo),
    Error { message: String },
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
