use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepoSelector {
    Path(PathBuf),
    Query(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckoutSelector {
    Path(PathBuf),
    Query(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckoutTarget {
    Branch(String),
    FreshBranch(String),
}

/// Routed command envelope shared by all frontends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<crate::HostName>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_repo: Option<RepoSelector>,
    #[serde(flatten)]
    pub action: CommandAction,
}

/// Commands the client can send to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CommandAction {
    CreateWorkspaceForCheckout {
        checkout_path: PathBuf,
    },
    SelectWorkspace {
        ws_ref: String,
    },
    Checkout {
        repo: RepoSelector,
        target: CheckoutTarget,
        #[serde(default)]
        issue_ids: Vec<(String, String)>,
    },
    RemoveCheckout {
        checkout: CheckoutSelector,
        #[serde(default)]
        terminal_keys: Vec<crate::ManagedTerminalId>,
    },
    FetchCheckoutStatus {
        branch: String,
        checkout_path: Option<PathBuf>,
        change_request_id: Option<String>,
    },
    OpenChangeRequest {
        id: String,
    },
    CloseChangeRequest {
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
        repo: RepoSelector,
    },
    Refresh {
        repo: Option<RepoSelector>,
    },
    SetIssueViewport {
        repo: PathBuf,
        visible_count: usize,
    },
    FetchMoreIssues {
        repo: PathBuf,
        desired_count: usize,
    },
    SearchIssues {
        repo: PathBuf,
        query: String,
    },
    ClearIssueSearch {
        repo: PathBuf,
    },
}

impl Command {
    pub fn description(&self) -> &'static str {
        match &self.action {
            CommandAction::CreateWorkspaceForCheckout { .. } => "Creating workspace...",
            CommandAction::SelectWorkspace { .. } => "Switching workspace...",
            CommandAction::Checkout { target, .. } => match target {
                CheckoutTarget::Branch(_) => "Checking out branch...",
                CheckoutTarget::FreshBranch(_) => "Creating checkout...",
            },
            CommandAction::RemoveCheckout { .. } => "Removing checkout...",
            CommandAction::FetchCheckoutStatus { .. } => "Fetching checkout status...",
            CommandAction::OpenChangeRequest { .. } => "Opening in browser...",
            CommandAction::CloseChangeRequest { .. } => "Closing PR...",
            CommandAction::OpenIssue { .. } => "Opening in browser...",
            CommandAction::LinkIssuesToChangeRequest { .. } => "Linking issues...",
            CommandAction::ArchiveSession { .. } => "Archiving session...",
            CommandAction::GenerateBranchName { .. } => "Generating branch name...",
            CommandAction::TeleportSession { .. } => "Teleporting session...",
            CommandAction::AddRepo { .. } => "Adding repository...",
            CommandAction::RemoveRepo { .. } => "Removing repository...",
            CommandAction::Refresh { .. } => "Refreshing...",
            CommandAction::SetIssueViewport { .. } => "Loading issues...",
            CommandAction::FetchMoreIssues { .. } => "Fetching issues...",
            CommandAction::SearchIssues { .. } => "Searching issues...",
            CommandAction::ClearIssueSearch { .. } => "Clearing search...",
        }
    }
}

/// Result returned from command execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CommandResult {
    Ok,
    RepoAdded {
        path: PathBuf,
    },
    RepoRemoved {
        path: PathBuf,
    },
    Refreshed {
        repos: Vec<PathBuf>,
    },
    CheckoutCreated {
        branch: String,
        path: PathBuf,
    },
    CheckoutRemoved {
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
    Cancelled,
}

/// Status of an individual step within a multi-step command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StepStatus {
    Skipped,
    Started,
    Succeeded,
    Failed { message: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutStatus {
    pub branch: String,
    pub change_request_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
    #[serde(default)]
    pub uncommitted_files: Vec<String>,
    pub base_detection_warning: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostName;
    use crate::test_helpers::assert_json_roundtrip;

    #[test]
    fn command_roundtrip_covers_all_variants() {
        let cases = vec![
            Command {
                host: Some(HostName::new("feta")),
                context_repo: None,
                action: CommandAction::Refresh { repo: Some(RepoSelector::Query("flotilla".into())) },
            },
            Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Path(PathBuf::from("/repo")),
                    target: CheckoutTarget::FreshBranch("feat-x".into()),
                    issue_ids: vec![("github".into(), "42".into())],
                },
            },
            Command {
                host: None,
                context_repo: None,
                action: CommandAction::RemoveCheckout {
                    checkout: CheckoutSelector::Query("feat-x".into()),
                    terminal_keys: vec![],
                },
            },
        ];

        for cmd in cases {
            assert_json_roundtrip(&cmd);
        }
    }

    #[test]
    fn command_uses_snake_case_tag() {
        let cmd = Command { host: None, context_repo: None, action: CommandAction::SelectWorkspace { ws_ref: "x".into() } };
        let json = serde_json::to_value(&cmd).expect("serialize");
        assert_eq!(json.get("action").and_then(|v| v.as_str()), Some("select_workspace"));
    }

    #[test]
    fn command_result_roundtrip_covers_all_variants() {
        let cases = vec![
            CommandResult::Ok,
            CommandResult::RepoAdded { path: PathBuf::from("/new/repo") },
            CommandResult::RepoRemoved { path: PathBuf::from("/old/repo") },
            CommandResult::Refreshed { repos: vec![PathBuf::from("/repo-a"), PathBuf::from("/repo-b")] },
            CommandResult::CheckoutCreated { branch: "feat-new".into(), path: PathBuf::from("/repos/project/wt-1") },
            CommandResult::CheckoutRemoved { branch: "feat-old".into() },
            CommandResult::BranchNameGenerated { name: "feat/cool-thing".into(), issue_ids: vec![("gh".into(), "1".into())] },
            CommandResult::CheckoutStatus(CheckoutStatus {
                branch: "old".into(),
                change_request_status: Some("merged".into()),
                merge_commit_sha: Some("abc123".into()),
                unpushed_commits: vec!["def456".into()],
                has_uncommitted: true,
                uncommitted_files: vec!["M  src/main.rs".into(), "?? TODO.txt".into()],
                base_detection_warning: Some("warning text".into()),
            }),
            CommandResult::Error { message: "something failed".into() },
            CommandResult::Cancelled,
        ];

        for result in cases {
            assert_json_roundtrip(&result);
        }
    }

    #[test]
    fn command_result_uses_snake_case_tag() {
        let result = CommandResult::CheckoutCreated { branch: "x".into(), path: PathBuf::from("/tmp/x") };
        let json = serde_json::to_value(&result).expect("serialize");
        assert_eq!(json.get("status").and_then(|v| v.as_str()), Some("checkout_created"));
    }

    #[test]
    fn step_status_roundtrip() {
        use crate::test_helpers::assert_roundtrip;

        let cases = vec![StepStatus::Skipped, StepStatus::Started, StepStatus::Succeeded, StepStatus::Failed {
            message: "workspace creation failed".into(),
        }];
        for case in cases {
            assert_roundtrip(&case);
        }
    }

    #[test]
    fn checkout_status_default() {
        let info = CheckoutStatus::default();
        assert_eq!(info.branch, "");
        assert!(info.change_request_status.is_none());
        assert!(info.merge_commit_sha.is_none());
        assert!(info.unpushed_commits.is_empty());
        assert!(!info.has_uncommitted);
        assert!(info.uncommitted_files.is_empty());
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
            uncommitted_files: vec!["M  src/lib.rs".into()],
            base_detection_warning: Some("ambiguous base".into()),
        };
        assert_json_roundtrip(&info);
    }

    #[test]
    fn command_description_covers_all_variants() {
        let cases: Vec<Command> = vec![
            Command { host: None, context_repo: None, action: CommandAction::CreateWorkspaceForCheckout { checkout_path: PathBuf::from("/tmp") } },
            Command { host: None, context_repo: None, action: CommandAction::SelectWorkspace { ws_ref: "x".into() } },
            Command {
                host: None,
                context_repo: None,
                action: CommandAction::Checkout {
                    repo: RepoSelector::Query("repo".into()),
                    target: CheckoutTarget::Branch("b".into()),
                    issue_ids: vec![],
                },
            },
            Command {
                host: None,
                context_repo: None,
                action: CommandAction::RemoveCheckout {
                    checkout: CheckoutSelector::Query("b".into()),
                    terminal_keys: vec![],
                },
            },
            Command {
                host: None,
                context_repo: None,
                action: CommandAction::FetchCheckoutStatus {
                    branch: "b".into(),
                    checkout_path: None,
                    change_request_id: None,
                },
            },
            Command { host: None, context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))), action: CommandAction::OpenChangeRequest { id: "1".into() } },
            Command { host: None, context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))), action: CommandAction::CloseChangeRequest { id: "1".into() } },
            Command { host: None, context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))), action: CommandAction::OpenIssue { id: "1".into() } },
            Command { host: None, context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))), action: CommandAction::LinkIssuesToChangeRequest { change_request_id: "1".into(), issue_ids: vec![] } },
            Command { host: None, context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))), action: CommandAction::ArchiveSession { session_id: "s".into() } },
            Command { host: None, context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))), action: CommandAction::GenerateBranchName { issue_keys: vec![] } },
            Command { host: None, context_repo: Some(RepoSelector::Path(PathBuf::from("/tmp"))), action: CommandAction::TeleportSession { session_id: "s".into(), branch: None, checkout_key: None } },
            Command { host: None, context_repo: None, action: CommandAction::AddRepo { path: PathBuf::from("/tmp") } },
            Command { host: None, context_repo: None, action: CommandAction::RemoveRepo { repo: RepoSelector::Path(PathBuf::from("/tmp")) } },
            Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } },
            Command { host: None, context_repo: None, action: CommandAction::SetIssueViewport { repo: PathBuf::from("/tmp"), visible_count: 10 } },
            Command { host: None, context_repo: None, action: CommandAction::FetchMoreIssues { repo: PathBuf::from("/tmp"), desired_count: 10 } },
            Command { host: None, context_repo: None, action: CommandAction::SearchIssues { repo: PathBuf::from("/tmp"), query: "q".into() } },
            Command { host: None, context_repo: None, action: CommandAction::ClearIssueSearch { repo: PathBuf::from("/tmp") } },
        ];
        for cmd in cases {
            let desc = cmd.description();
            assert!(!desc.is_empty(), "empty description for {:?}", cmd);
        }
    }
}
