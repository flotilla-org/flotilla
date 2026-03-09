//! Daemon-side command executor.
//!
//! Takes a `Command`, the repo context, and returns a `CommandResult`.
//! No UI state mutation — all results are carried in the return value.

use std::path::Path;

use flotilla_protocol::{Command, CommandResult};
use tracing::{debug, error, info};

use crate::data;
use crate::provider_data::ProviderData;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::WorkspaceConfig;
use crate::providers::types::{CloudAgentSession, CorrelationKey};
use crate::providers::CommandRunner;

/// Execute a `Command` against the given repo context.
///
/// Commands that are handled at the daemon level (AddRepo, RemoveRepo, Refresh)
/// should not reach this function — the caller should handle them directly.
pub async fn execute(
    cmd: Command,
    repo_root: &Path,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
    runner: &dyn CommandRunner,
    config_base: &Path,
) -> CommandResult {
    match cmd {
        Command::CreateWorkspaceForCheckout { checkout_path } => {
            if let Some(co) = providers_data.checkouts.get(&checkout_path).cloned() {
                info!("entering workspace for {}", co.branch);
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let config = workspace_config(
                        repo_root,
                        &co.branch,
                        &checkout_path,
                        "claude",
                        config_base,
                    );
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        return CommandResult::Error { message: e };
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error {
                    message: format!("checkout not found: {}", checkout_path.display()),
                }
            }
        }

        Command::SelectWorkspace { ws_ref } => {
            info!("switching to workspace {ws_ref}");
            if let Some((_, ws_mgr)) = &registry.workspace_manager {
                if let Err(e) = ws_mgr.select_workspace(&ws_ref).await {
                    return CommandResult::Error { message: e };
                }
            }
            CommandResult::Ok
        }

        Command::CreateCheckout {
            branch,
            create_branch,
            issue_ids,
        } => {
            info!("creating checkout {branch}");
            let checkout_result = if let Some(cm) = registry.checkout_managers.values().next() {
                Some(cm.create_checkout(repo_root, &branch, create_branch).await)
            } else {
                None
            };
            match checkout_result {
                Some(Ok((checkout_path, _checkout))) => {
                    // Write issue links to git config
                    if !issue_ids.is_empty() {
                        write_branch_issue_links(repo_root, &branch, &issue_ids, runner).await;
                    }
                    info!("created checkout at {}", checkout_path.display());
                    // Create workspace if manager available
                    if let Some((_, ws_mgr)) = &registry.workspace_manager {
                        let config = workspace_config(
                            repo_root,
                            &branch,
                            &checkout_path,
                            "claude",
                            config_base,
                        );
                        if let Err(e) = ws_mgr.create_workspace(&config).await {
                            // Checkout was created but workspace failed — report as error
                            // but the checkout still exists
                            error!("workspace creation failed after checkout: {e}");
                        }
                    }
                    CommandResult::CheckoutCreated {
                        branch: branch.clone(),
                    }
                }
                Some(Err(e)) => {
                    error!("create checkout failed: {e}");
                    CommandResult::Error { message: e }
                }
                None => CommandResult::Error {
                    message: "No checkout manager available".to_string(),
                },
            }
        }

        Command::RemoveCheckout { branch } => {
            info!("removing checkout {branch}");
            let result = if let Some(cm) = registry.checkout_managers.values().next() {
                Some(cm.remove_checkout(repo_root, &branch).await)
            } else {
                None
            };
            match result {
                Some(Ok(())) => CommandResult::Ok,
                Some(Err(e)) => CommandResult::Error { message: e },
                None => CommandResult::Error {
                    message: "No checkout manager available".to_string(),
                },
            }
        }

        Command::FetchCheckoutStatus {
            branch,
            checkout_path,
            change_request_id,
        } => {
            let info = data::fetch_checkout_status(
                &branch,
                checkout_path.as_deref(),
                change_request_id.as_deref(),
                repo_root,
                runner,
            )
            .await;
            CommandResult::CheckoutStatus(info)
        }

        Command::OpenChangeRequest { id } => {
            debug!("opening change request {id} in browser");
            if let Some(cr) = registry.code_review.values().next() {
                let _ = cr.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }

        Command::OpenIssue { id } => {
            debug!("opening issue {id} in browser");
            if let Some(it) = registry.issue_trackers.values().next() {
                let _ = it.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }

        Command::LinkIssuesToChangeRequest {
            change_request_id,
            issue_ids,
        } => {
            info!(
                "linking issues {:?} to change request #{change_request_id}",
                issue_ids
            );
            let body_result = runner
                .run(
                    "gh",
                    &[
                        "pr",
                        "view",
                        &change_request_id,
                        "--json",
                        "body",
                        "--jq",
                        ".body",
                    ],
                    repo_root,
                )
                .await;
            match body_result {
                Ok(current_body) => {
                    let fixes_lines: Vec<String> =
                        issue_ids.iter().map(|id| format!("Fixes #{id}")).collect();
                    let new_body = if current_body.trim().is_empty() {
                        fixes_lines.join("\n")
                    } else {
                        format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                    };
                    let result = runner
                        .run(
                            "gh",
                            &["pr", "edit", &change_request_id, "--body", &new_body],
                            repo_root,
                        )
                        .await;
                    match result {
                        Ok(_) => {
                            info!("linked issues to change request #{change_request_id}");
                            CommandResult::Ok
                        }
                        Err(e) => {
                            error!("failed to edit change request: {e}");
                            CommandResult::Error { message: e }
                        }
                    }
                }
                Err(e) => {
                    error!("failed to read change request body: {e}");
                    CommandResult::Error { message: e }
                }
            }
        }

        Command::ArchiveSession { session_id } => {
            if let Some(session) = providers_data.sessions.get(session_id.as_str()) {
                info!("archiving session {session_id}");
                let provider_key = session_provider_key(session, &session_id)
                    .or_else(|| registry.coding_agents.keys().next().map(String::as_str));
                if let Some(key) = provider_key {
                    if let Some(ca) = registry.coding_agents.get(key) {
                        match ca.archive_session(&session_id).await {
                            Ok(()) => CommandResult::Ok,
                            Err(e) => CommandResult::Error { message: e },
                        }
                    } else {
                        CommandResult::Error {
                            message: format!("No coding agent available for provider: {key}"),
                        }
                    }
                } else {
                    CommandResult::Error {
                        message: "No coding agent available".to_string(),
                    }
                }
            } else {
                CommandResult::Error {
                    message: format!("session not found: {session_id}"),
                }
            }
        }

        Command::GenerateBranchName { issue_keys } => {
            let issues: Vec<(String, String)> = issue_keys
                .iter()
                .filter_map(|k| {
                    providers_data
                        .issues
                        .get(k.as_str())
                        .map(|issue| (k.clone(), issue.title.clone()))
                })
                .collect();

            // Collect (provider_name, issue_id) pairs for the created branch
            let issue_id_pairs: Vec<(String, String)> = {
                let provider = registry
                    .issue_trackers
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "issues".to_string());
                issues
                    .iter()
                    .map(|(id, _title)| (provider.clone(), id.clone()))
                    .collect()
            };

            info!("generating branch name");
            let branch_result = if let Some(ai) = registry.ai_utilities.values().next() {
                let context: Vec<String> = issues
                    .iter()
                    .map(|(id, title)| format!("{} #{}", title, id))
                    .collect();
                let prompt_text = if context.len() == 1 {
                    context[0].clone()
                } else {
                    context.join("; ")
                };
                Some(ai.generate_branch_name(&prompt_text).await)
            } else {
                None
            };
            match branch_result {
                Some(Ok(name)) => {
                    info!("AI suggested: {name}");
                    CommandResult::BranchNameGenerated {
                        name,
                        issue_ids: issue_id_pairs,
                    }
                }
                _ => {
                    let fallback: Vec<String> = issues
                        .iter()
                        .map(|(id, _)| format!("issue-{}", id))
                        .collect();
                    let name = fallback.join("-");
                    CommandResult::BranchNameGenerated {
                        name,
                        issue_ids: issue_id_pairs,
                    }
                }
            }
        }

        Command::TeleportSession {
            session_id,
            branch,
            checkout_key,
        } => {
            info!("teleporting to session {session_id}");
            let teleport_cmd =
                match resolve_attach_command(&session_id, registry, providers_data).await {
                    Ok(cmd) => cmd,
                    Err(message) => return CommandResult::Error { message },
                };
            let wt_path = if let Some(ref key) = checkout_key {
                providers_data.checkouts.get(key).map(|_| key.clone())
            } else if let Some(branch_name) = &branch {
                let checkout_result = if let Some(cm) = registry.checkout_managers.values().next() {
                    cm.create_checkout(repo_root, branch_name, false).await.ok()
                } else {
                    None
                };
                checkout_result.map(|(path, _)| path)
            } else {
                None
            };
            if let Some(path) = wt_path {
                let name = branch.as_deref().unwrap_or("session");
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let config =
                        workspace_config(repo_root, name, &path, &teleport_cmd, config_base);
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        // Unlike CreateCheckout, teleport fails entirely if the workspace
                        // can't be created — the checkout may already have existed.
                        return CommandResult::Error { message: e };
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error {
                    message: "Could not determine checkout path for teleport".to_string(),
                }
            }
        }

        // These are handled at the daemon level (InProcessDaemon / SocketDaemon),
        // not by the per-repo executor. If they reach here, it's a routing bug.
        Command::AddRepo { .. }
        | Command::RemoveRepo { .. }
        | Command::Refresh
        | Command::SetIssueViewport { .. }
        | Command::FetchMoreIssues { .. }
        | Command::SearchIssues { .. }
        | Command::ClearIssueSearch { .. } => CommandResult::Error {
            message: "bug: daemon-level command reached per-repo executor".to_string(),
        },
    }
}

fn session_provider_key<'a>(session: &'a CloudAgentSession, session_id: &str) -> Option<&'a str> {
    session
        .correlation_keys
        .iter()
        .find_map(|k| match k {
            CorrelationKey::SessionRef(provider, id) if id == session_id => Some(provider.as_str()),
            _ => None,
        })
        .or_else(|| {
            session.correlation_keys.iter().find_map(|k| match k {
                CorrelationKey::SessionRef(provider, _) => Some(provider.as_str()),
                _ => None,
            })
        })
}

async fn resolve_attach_command(
    session_id: &str,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
) -> Result<String, String> {
    let session_provider = providers_data
        .sessions
        .get(session_id)
        .and_then(|s| session_provider_key(s, session_id));

    if let Some(provider_key) = session_provider {
        if let Some(ca) = registry.coding_agents.get(provider_key) {
            return ca.attach_command(session_id).await;
        }
    };

    if let Some(ca) = registry.coding_agents.values().next() {
        return ca.attach_command(session_id).await;
    }

    // Preserve legacy behavior when no coding-agent provider is configured.
    Ok(format!("claude --teleport {session_id}"))
}

/// Build a WorkspaceConfig from repo/branch/dir/command.
pub(crate) fn workspace_config(
    repo_root: &Path,
    name: &str,
    working_dir: &Path,
    main_command: &str,
    config_base: &Path,
) -> WorkspaceConfig {
    let tmpl_path = repo_root.join(".flotilla/workspace.yaml");
    let template_yaml = std::fs::read_to_string(&tmpl_path).ok().or_else(|| {
        let global_path = config_base.join("workspace.yaml");
        std::fs::read_to_string(global_path).ok()
    });
    let mut template_vars = std::collections::HashMap::new();
    template_vars.insert("main_command".to_string(), main_command.to_string());
    WorkspaceConfig {
        name: name.to_string(),
        working_directory: working_dir.to_path_buf(),
        template_vars,
        template_yaml,
    }
}

/// Write branch-to-issue links into git config.
async fn write_branch_issue_links(
    repo_root: &Path,
    branch: &str,
    issue_ids: &[(String, String)],
    runner: &dyn CommandRunner,
) {
    use std::collections::HashMap;
    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider
            .entry(provider.as_str())
            .or_default()
            .push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        if let Err(e) = runner
            .run("git", &["config", &key, &value], repo_root)
            .await
        {
            tracing::warn!("failed to write issue link: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::providers::code_review::CodeReview;
    use crate::providers::coding_agent::CodingAgent;
    use crate::providers::issue_tracker::IssueTracker;
    use crate::providers::testing::MockRunner;
    use crate::providers::types::*;
    use crate::providers::vcs::CheckoutManager;
    use crate::providers::workspace::WorkspaceManager;
    use async_trait::async_trait;

    // -----------------------------------------------------------------------
    // Mock providers
    // -----------------------------------------------------------------------

    /// A mock CheckoutManager that returns a canned checkout or error.
    struct MockCheckoutManager {
        create_result: tokio::sync::Mutex<Option<Result<(PathBuf, Checkout), String>>>,
        remove_result: tokio::sync::Mutex<Option<Result<(), String>>>,
    }

    impl MockCheckoutManager {
        fn succeeding(branch: &str, path: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Some(Ok((
                    PathBuf::from(path),
                    Checkout {
                        branch: branch.to_string(),
                        is_trunk: false,
                        trunk_ahead_behind: None,
                        remote_ahead_behind: None,
                        working_tree: None,
                        last_commit: None,
                        correlation_keys: vec![],
                        association_keys: vec![],
                    },
                )))),
                remove_result: tokio::sync::Mutex::new(Some(Ok(()))),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
                remove_result: tokio::sync::Mutex::new(Some(Err(msg.to_string()))),
            }
        }
    }

    #[async_trait]
    impl CheckoutManager for MockCheckoutManager {
        fn display_name(&self) -> &str {
            "mock-checkout"
        }
        async fn list_checkouts(
            &self,
            _repo_root: &Path,
        ) -> Result<Vec<(PathBuf, Checkout)>, String> {
            Ok(vec![])
        }
        async fn create_checkout(
            &self,
            _repo_root: &Path,
            _branch: &str,
            _create_branch: bool,
        ) -> Result<(PathBuf, Checkout), String> {
            self.create_result
                .lock()
                .await
                .take()
                .expect("create_checkout called more than expected")
        }
        async fn remove_checkout(&self, _repo_root: &Path, _branch: &str) -> Result<(), String> {
            self.remove_result
                .lock()
                .await
                .take()
                .expect("remove_checkout called more than expected")
        }
    }

    /// A mock WorkspaceManager that records calls and returns configurable results.
    struct MockWorkspaceManager {
        create_result: tokio::sync::Mutex<Result<(), String>>,
        select_result: tokio::sync::Mutex<Result<(), String>>,
    }

    impl MockWorkspaceManager {
        fn succeeding() -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Ok(())),
                select_result: tokio::sync::Mutex::new(Ok(())),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                create_result: tokio::sync::Mutex::new(Err(msg.to_string())),
                select_result: tokio::sync::Mutex::new(Err(msg.to_string())),
            }
        }
    }

    #[async_trait]
    impl WorkspaceManager for MockWorkspaceManager {
        fn display_name(&self) -> &str {
            "mock-ws"
        }
        async fn list_workspaces(&self) -> Result<Vec<(String, Workspace)>, String> {
            Ok(vec![])
        }
        async fn create_workspace(
            &self,
            _config: &WorkspaceConfig,
        ) -> Result<(String, Workspace), String> {
            let result = self.create_result.lock().await;
            match &*result {
                Ok(()) => Ok((
                    "mock-ref".to_string(),
                    Workspace {
                        name: "mock".to_string(),
                        directories: vec![],
                        correlation_keys: vec![],
                    },
                )),
                Err(e) => Err(e.clone()),
            }
        }
        async fn select_workspace(&self, _ws_ref: &str) -> Result<(), String> {
            let result = self.select_result.lock().await;
            result.clone()
        }
    }

    /// A mock CodeReview provider.
    struct MockCodeReview;

    #[async_trait]
    impl CodeReview for MockCodeReview {
        fn display_name(&self) -> &str {
            "mock-cr"
        }
        async fn list_change_requests(
            &self,
            _repo_root: &Path,
            _limit: usize,
        ) -> Result<Vec<(String, ChangeRequest)>, String> {
            Ok(vec![])
        }
        async fn get_change_request(
            &self,
            _repo_root: &Path,
            _id: &str,
        ) -> Result<(String, ChangeRequest), String> {
            Err("not implemented".to_string())
        }
        async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
        async fn list_merged_branch_names(
            &self,
            _repo_root: &Path,
            _limit: usize,
        ) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
    }

    /// A mock IssueTracker provider.
    struct MockIssueTracker;

    #[async_trait]
    impl IssueTracker for MockIssueTracker {
        fn display_name(&self) -> &str {
            "mock-issues"
        }
        async fn list_issues(
            &self,
            _repo_root: &Path,
            _limit: usize,
        ) -> Result<Vec<(String, Issue)>, String> {
            Ok(vec![])
        }
        async fn open_in_browser(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// A mock CodingAgent provider.
    struct MockCodingAgent {
        archive_result: tokio::sync::Mutex<Result<(), String>>,
        attach_command: String,
    }

    impl MockCodingAgent {
        fn succeeding() -> Self {
            Self {
                archive_result: tokio::sync::Mutex::new(Ok(())),
                attach_command: "claude --attach".to_string(),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                archive_result: tokio::sync::Mutex::new(Err(msg.to_string())),
                attach_command: "claude --attach".to_string(),
            }
        }

        fn with_attach(attach_command: &str) -> Self {
            Self {
                archive_result: tokio::sync::Mutex::new(Ok(())),
                attach_command: attach_command.to_string(),
            }
        }
    }

    #[async_trait]
    impl CodingAgent for MockCodingAgent {
        fn display_name(&self) -> &str {
            "mock-agent"
        }
        async fn list_sessions(
            &self,
            _criteria: &RepoCriteria,
        ) -> Result<Vec<(String, CloudAgentSession)>, String> {
            Ok(vec![])
        }
        async fn archive_session(&self, _session_id: &str) -> Result<(), String> {
            let result = self.archive_result.lock().await;
            result.clone()
        }
        async fn attach_command(&self, _session_id: &str) -> Result<String, String> {
            Ok(self.attach_command.clone())
        }
    }

    /// A mock AiUtility provider.
    struct MockAiUtility {
        result: tokio::sync::Mutex<Result<String, String>>,
    }

    impl MockAiUtility {
        fn succeeding(name: &str) -> Self {
            Self {
                result: tokio::sync::Mutex::new(Ok(name.to_string())),
            }
        }

        fn failing(msg: &str) -> Self {
            Self {
                result: tokio::sync::Mutex::new(Err(msg.to_string())),
            }
        }
    }

    #[async_trait]
    impl crate::providers::ai_utility::AiUtility for MockAiUtility {
        fn display_name(&self) -> &str {
            "mock-ai"
        }
        async fn generate_branch_name(&self, _context: &str) -> Result<String, String> {
            let result = self.result.lock().await;
            result.clone()
        }
    }

    // -----------------------------------------------------------------------
    // Helper to build test fixtures
    // -----------------------------------------------------------------------

    fn empty_registry() -> ProviderRegistry {
        ProviderRegistry::new()
    }

    fn empty_data() -> ProviderData {
        ProviderData::default()
    }

    fn repo_root() -> PathBuf {
        PathBuf::from("/tmp/test-repo")
    }

    fn config_base() -> PathBuf {
        PathBuf::from("/tmp/test-config")
    }

    fn make_checkout(branch: &str, _path: &str) -> Checkout {
        Checkout {
            branch: branch.to_string(),
            is_trunk: false,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![],
            association_keys: vec![],
        }
    }

    fn make_session(_id: &str) -> CloudAgentSession {
        CloudAgentSession {
            title: "test session".to_string(),
            status: SessionStatus::Running,
            model: None,
            updated_at: None,
            correlation_keys: vec![],
        }
    }

    fn make_issue(_id: &str, title: &str) -> Issue {
        Issue {
            title: title.to_string(),
            labels: vec![],
            association_keys: vec![],
        }
    }

    fn runner_ok() -> MockRunner {
        MockRunner::new(vec![])
    }

    async fn run_execute(
        command: Command,
        registry: &ProviderRegistry,
        providers_data: &ProviderData,
        runner: &MockRunner,
    ) -> CommandResult {
        execute(
            command,
            &repo_root(),
            registry,
            providers_data,
            runner,
            &config_base(),
        )
        .await
    }

    fn assert_error_contains(result: CommandResult, expected_substring: &str) {
        match result {
            CommandResult::Error { message } => {
                assert!(
                    message.contains(expected_substring),
                    "expected error containing {expected_substring:?}, got {message:?}"
                );
            }
            other => panic!("expected Error, got {:?}", other),
        }
    }

    fn assert_error_eq(result: CommandResult, expected: &str) {
        match result {
            CommandResult::Error { message } => assert_eq!(message, expected),
            other => panic!("expected Error, got {:?}", other),
        }
    }

    fn assert_checkout_created_branch(result: CommandResult, expected_branch: &str) {
        match result {
            CommandResult::CheckoutCreated { branch } => {
                assert_eq!(branch, expected_branch);
            }
            other => panic!("expected CheckoutCreated, got {:?}", other),
        }
    }

    fn assert_checkout_status_branch(result: CommandResult, expected_branch: &str) {
        match result {
            CommandResult::CheckoutStatus(info) => {
                assert_eq!(info.branch, expected_branch);
            }
            other => panic!("expected CheckoutStatus, got {:?}", other),
        }
    }

    fn assert_branch_name_generated(
        result: CommandResult,
        expected_name: &str,
        expected_issue_ids: &[(&str, &str)],
    ) {
        match result {
            CommandResult::BranchNameGenerated { name, issue_ids } => {
                assert_eq!(name, expected_name);
                let expected_issue_ids: Vec<_> = expected_issue_ids
                    .iter()
                    .map(|(provider, id)| (provider.to_string(), id.to_string()))
                    .collect();
                assert_eq!(issue_ids, expected_issue_ids);
            }
            other => panic!("expected BranchNameGenerated, got {:?}", other),
        }
    }

    fn assert_ok(result: CommandResult) {
        assert!(matches!(result, CommandResult::Ok));
    }

    // -----------------------------------------------------------------------
    // Tests: CreateWorkspaceForCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_workspace_for_checkout_success_without_ws_manager() {
        let registry = empty_registry();
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts
            .insert(path.clone(), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateWorkspaceForCheckout {
                checkout_path: path,
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn archive_session_uses_provider_from_session_ref() {
        let mut registry = empty_registry();
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(MockCodingAgent::failing("wrong provider")),
        );
        registry.coding_agents.insert(
            "cursor".to_string(),
            Arc::new(MockCodingAgent::succeeding()),
        );
        let mut data = empty_data();
        let mut session = make_session("sess-1");
        session.correlation_keys = vec![CorrelationKey::SessionRef(
            "cursor".to_string(),
            "sess-1".to_string(),
        )];
        data.sessions.insert("sess-1".to_string(), session);
        let runner = runner_ok();

        let result = run_execute(
            Command::ArchiveSession {
                session_id: "sess-1".to_string(),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_success_with_ws_manager() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::succeeding()),
        ));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts
            .insert(path.clone(), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateWorkspaceForCheckout {
                checkout_path: path,
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_not_found() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateWorkspaceForCheckout {
                checkout_path: PathBuf::from("/nonexistent"),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_contains(result, "checkout not found");
    }

    #[tokio::test]
    async fn create_workspace_for_checkout_ws_manager_fails() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::failing("ws creation failed")),
        ));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts
            .insert(path.clone(), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateWorkspaceForCheckout {
                checkout_path: path,
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_eq(result, "ws creation failed");
    }

    // -----------------------------------------------------------------------
    // Tests: SelectWorkspace
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn select_workspace_no_manager() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(
            Command::SelectWorkspace {
                ws_ref: "my-ws".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn select_workspace_success() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::succeeding()),
        ));
        let runner = runner_ok();

        let result = run_execute(
            Command::SelectWorkspace {
                ws_ref: "my-ws".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn select_workspace_failure() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::failing("select failed")),
        ));
        let runner = runner_ok();

        let result = run_execute(
            Command::SelectWorkspace {
                ws_ref: "bad-ws".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "select failed");
    }

    // -----------------------------------------------------------------------
    // Tests: CreateCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_checkout_no_manager() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout {
                branch: "feat-x".to_string(),
                create_branch: true,
                issue_ids: vec![],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_contains(result, "No checkout manager available");
    }

    #[tokio::test]
    async fn create_checkout_success() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")),
        );
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout {
                branch: "feat-x".to_string(),
                create_branch: true,
                issue_ids: vec![],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_created_branch(result, "feat-x");
    }

    #[tokio::test]
    async fn create_checkout_with_issue_ids_writes_git_config() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")),
        );
        // The runner needs one Ok response for the git config write
        let runner = MockRunner::new(vec![Ok(String::new())]);

        let result = run_execute(
            Command::CreateCheckout {
                branch: "feat-x".to_string(),
                create_branch: true,
                issue_ids: vec![("github".to_string(), "42".to_string())],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_created_branch(result, "feat-x");
    }

    #[tokio::test]
    async fn create_checkout_failure() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::failing("branch already exists")),
        );
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout {
                branch: "feat-x".to_string(),
                create_branch: true,
                issue_ids: vec![],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "branch already exists");
    }

    #[tokio::test]
    async fn create_checkout_success_ws_manager_fails_still_returns_created() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::succeeding("feat-x", "/repo/wt-feat-x")),
        );
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::failing("ws failed")),
        ));
        let runner = runner_ok();

        let result = run_execute(
            Command::CreateCheckout {
                branch: "feat-x".to_string(),
                create_branch: true,
                issue_ids: vec![],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        // Workspace failure is logged but checkout still reports success
        assert_checkout_created_branch(result, "feat-x");
    }

    // -----------------------------------------------------------------------
    // Tests: RemoveCheckout
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn remove_checkout_no_manager() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(
            Command::RemoveCheckout {
                branch: "old".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_contains(result, "No checkout manager available");
    }

    #[tokio::test]
    async fn remove_checkout_success() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::succeeding("old", "/repo/wt-old")),
        );
        let runner = runner_ok();

        let result = run_execute(
            Command::RemoveCheckout {
                branch: "old".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn remove_checkout_failure() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::failing("cannot remove trunk")),
        );
        let runner = runner_ok();

        let result = run_execute(
            Command::RemoveCheckout {
                branch: "main".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "cannot remove trunk");
    }

    // -----------------------------------------------------------------------
    // Tests: FetchCheckoutStatus
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn fetch_checkout_status_returns_checkout_status() {
        let registry = empty_registry();
        // fetch_checkout_status runs multiple git/gh commands concurrently via
        // tokio::join!. Provide enough error responses for all subprocess calls:
        //   - git rev-parse (upstream) -> Err
        //   - git rev-parse (origin/HEAD) -> Err
        //   - git status --porcelain -> Err
        //   - gh pr view -> Err
        let runner = MockRunner::new(vec![
            Err("err".to_string()),
            Err("err".to_string()),
            Err("err".to_string()),
            Err("err".to_string()),
        ]);

        let result = run_execute(
            Command::FetchCheckoutStatus {
                branch: "feat".to_string(),
                checkout_path: Some(PathBuf::from("/repo/wt")),
                change_request_id: Some("42".to_string()),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_checkout_status_branch(result, "feat");
    }

    // -----------------------------------------------------------------------
    // Tests: OpenChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn open_change_request_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(
            Command::OpenChangeRequest {
                id: "42".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn open_change_request_with_provider() {
        let mut registry = empty_registry();
        registry
            .code_review
            .insert("github".to_string(), Arc::new(MockCodeReview));
        let runner = runner_ok();

        let result = run_execute(
            Command::OpenChangeRequest {
                id: "42".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: OpenIssue
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn open_issue_no_provider() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(
            Command::OpenIssue {
                id: "10".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn open_issue_with_provider() {
        let mut registry = empty_registry();
        registry
            .issue_trackers
            .insert("github".to_string(), Arc::new(MockIssueTracker));
        let runner = runner_ok();

        let result = run_execute(
            Command::OpenIssue {
                id: "10".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: LinkIssuesToChangeRequest
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn link_issues_success_with_existing_body() {
        let registry = empty_registry();
        // First call: gh pr view returns existing body
        // Second call: gh pr edit succeeds
        let runner = MockRunner::new(vec![Ok("Existing PR body".to_string()), Ok(String::new())]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest {
                change_request_id: "55".to_string(),
                issue_ids: vec!["10".to_string(), "20".to_string()],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn link_issues_success_with_empty_body() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![
            Ok("  \n".to_string()), // empty/whitespace body
            Ok(String::new()),      // edit succeeds
        ]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest {
                change_request_id: "55".to_string(),
                issue_ids: vec!["10".to_string()],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn link_issues_view_fails() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![Err("gh not found".to_string())]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest {
                change_request_id: "55".to_string(),
                issue_ids: vec!["10".to_string()],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "gh not found");
    }

    #[tokio::test]
    async fn link_issues_edit_fails() {
        let registry = empty_registry();
        let runner = MockRunner::new(vec![
            Ok("body text".to_string()),
            Err("permission denied".to_string()),
        ]);

        let result = run_execute(
            Command::LinkIssuesToChangeRequest {
                change_request_id: "55".to_string(),
                issue_ids: vec!["10".to_string()],
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_eq(result, "permission denied");
    }

    // -----------------------------------------------------------------------
    // Tests: ArchiveSession
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn archive_session_not_found() {
        let registry = empty_registry();
        let runner = runner_ok();

        let result = run_execute(
            Command::ArchiveSession {
                session_id: "nonexistent".to_string(),
            },
            &registry,
            &empty_data(),
            &runner,
        )
        .await;

        assert_error_contains(result, "session not found");
    }

    #[tokio::test]
    async fn archive_session_no_agent_provider() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.sessions
            .insert("sess-1".to_string(), make_session("sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::ArchiveSession {
                session_id: "sess-1".to_string(),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_contains(result, "No coding agent available");
    }

    #[tokio::test]
    async fn archive_session_success() {
        let mut registry = empty_registry();
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(MockCodingAgent::succeeding()),
        );
        let mut data = empty_data();
        data.sessions
            .insert("sess-1".to_string(), make_session("sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::ArchiveSession {
                session_id: "sess-1".to_string(),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn archive_session_agent_fails() {
        let mut registry = empty_registry();
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(MockCodingAgent::failing("archive failed")),
        );
        let mut data = empty_data();
        data.sessions
            .insert("sess-1".to_string(), make_session("sess-1"));
        let runner = runner_ok();

        let result = run_execute(
            Command::ArchiveSession {
                session_id: "sess-1".to_string(),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_eq(result, "archive failed");
    }

    // -----------------------------------------------------------------------
    // Tests: GenerateBranchName
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn generate_branch_name_ai_success() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert(
            "claude".to_string(),
            Arc::new(MockAiUtility::succeeding("feat/add-login")),
        );
        registry
            .issue_trackers
            .insert("github".to_string(), Arc::new(MockIssueTracker));
        let mut data = empty_data();
        data.issues
            .insert("42".to_string(), make_issue("42", "Add login feature"));
        let runner = runner_ok();

        let result = run_execute(
            Command::GenerateBranchName {
                issue_keys: vec!["42".to_string()],
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_branch_name_generated(result, "feat/add-login", &[("github", "42")]);
    }

    #[tokio::test]
    async fn generate_branch_name_ai_failure_uses_fallback() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert(
            "claude".to_string(),
            Arc::new(MockAiUtility::failing("API error")),
        );
        let mut data = empty_data();
        data.issues
            .insert("42".to_string(), make_issue("42", "Add login"));
        let runner = runner_ok();

        let result = run_execute(
            Command::GenerateBranchName {
                issue_keys: vec!["42".to_string()],
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_branch_name_generated(result, "issue-42", &[("issues", "42")]);
    }

    #[tokio::test]
    async fn generate_branch_name_no_ai_provider_uses_fallback() {
        let registry = empty_registry();
        let mut data = empty_data();
        data.issues
            .insert("7".to_string(), make_issue("7", "Fix bug"));
        let runner = runner_ok();

        let result = run_execute(
            Command::GenerateBranchName {
                issue_keys: vec!["7".to_string()],
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        // No issue tracker registered, defaults to "issues"
        assert_branch_name_generated(result, "issue-7", &[("issues", "7")]);
    }

    #[tokio::test]
    async fn generate_branch_name_multiple_issues() {
        let mut registry = empty_registry();
        registry.ai_utilities.insert(
            "claude".to_string(),
            Arc::new(MockAiUtility::succeeding("feat/login-and-signup")),
        );
        registry
            .issue_trackers
            .insert("github".to_string(), Arc::new(MockIssueTracker));
        let mut data = empty_data();
        data.issues
            .insert("1".to_string(), make_issue("1", "Login feature"));
        data.issues
            .insert("2".to_string(), make_issue("2", "Signup feature"));
        let runner = runner_ok();

        let result = run_execute(
            Command::GenerateBranchName {
                issue_keys: vec!["1".to_string(), "2".to_string()],
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_branch_name_generated(
            result,
            "feat/login-and-signup",
            &[("github", "1"), ("github", "2")],
        );
    }

    #[tokio::test]
    async fn generate_branch_name_unknown_issue_key() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let result = run_execute(
            Command::GenerateBranchName {
                issue_keys: vec!["nonexistent".to_string()],
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        // No issues found, so empty fallback
        assert_branch_name_generated(result, "", &[]);
    }

    // -----------------------------------------------------------------------
    // Tests: TeleportSession
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn teleport_session_with_checkout_key() {
        let mut registry = empty_registry();
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(MockCodingAgent::with_attach("agent --resume claude")),
        );
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::succeeding()),
        ));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts
            .insert(path.clone(), make_checkout("feat", "/repo/wt-feat"));
        // resolve_claude_path calls runner.exists which returns true for MockRunner
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession {
                session_id: "sess-1".to_string(),
                branch: Some("feat".to_string()),
                checkout_key: Some(path),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_uses_provider_specific_attach_command() {
        let mut registry = empty_registry();
        registry.coding_agents.insert(
            "claude".to_string(),
            Arc::new(MockCodingAgent::with_attach("agent --resume claude")),
        );
        registry.coding_agents.insert(
            "cursor".to_string(),
            Arc::new(MockCodingAgent::with_attach("agent --resume cursor")),
        );
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::succeeding()),
        ));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts
            .insert(path.clone(), make_checkout("feat", "/repo/wt-feat"));
        let mut session = make_session("sess-1");
        session.correlation_keys = vec![CorrelationKey::SessionRef(
            "cursor".to_string(),
            "sess-1".to_string(),
        )];
        data.sessions.insert("sess-1".to_string(), session);
        let runner = runner_ok();

        let attach = resolve_attach_command("sess-1", &registry, &data)
            .await
            .expect("resolve attach command");
        assert_eq!(attach, "agent --resume cursor");

        let result = run_execute(
            Command::TeleportSession {
                session_id: "sess-1".to_string(),
                branch: Some("feat".to_string()),
                checkout_key: Some(path),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_with_branch_creates_checkout() {
        let mut registry = empty_registry();
        registry.checkout_managers.insert(
            "wt".to_string(),
            Arc::new(MockCheckoutManager::succeeding("feat", "/repo/wt-feat")),
        );
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::succeeding()),
        ));
        let data = empty_data();
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession {
                session_id: "sess-1".to_string(),
                branch: Some("feat".to_string()),
                checkout_key: None,
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    #[tokio::test]
    async fn teleport_session_no_path_no_branch() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession {
                session_id: "sess-1".to_string(),
                branch: None,
                checkout_key: None,
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_contains(result, "Could not determine checkout path");
    }

    #[tokio::test]
    async fn teleport_session_ws_manager_fails() {
        let mut registry = empty_registry();
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::failing("ws failed")),
        ));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts
            .insert(path.clone(), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession {
                session_id: "sess-1".to_string(),
                branch: Some("feat".to_string()),
                checkout_key: Some(path),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_error_eq(result, "ws failed");
    }

    #[tokio::test]
    async fn teleport_session_uses_session_as_name_when_no_branch() {
        // When checkout_key is present but branch is None, uses "session" as name.
        let mut registry = empty_registry();
        registry.workspace_manager = Some((
            "cmux".to_string(),
            Arc::new(MockWorkspaceManager::succeeding()),
        ));
        let mut data = empty_data();
        let path = PathBuf::from("/repo/wt-feat");
        data.checkouts
            .insert(path.clone(), make_checkout("feat", "/repo/wt-feat"));
        let runner = runner_ok();

        let result = run_execute(
            Command::TeleportSession {
                session_id: "sess-1".to_string(),
                branch: None,
                checkout_key: Some(path),
            },
            &registry,
            &data,
            &runner,
        )
        .await;

        assert_ok(result);
    }

    // -----------------------------------------------------------------------
    // Tests: Daemon-level commands rejected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn daemon_level_commands_return_error() {
        let registry = empty_registry();
        let data = empty_data();
        let runner = runner_ok();

        let daemon_commands = vec![
            Command::AddRepo {
                path: PathBuf::from("/repo"),
            },
            Command::RemoveRepo {
                path: PathBuf::from("/repo"),
            },
            Command::Refresh,
            Command::SetIssueViewport {
                repo: PathBuf::from("/repo"),
                visible_count: 10,
            },
            Command::FetchMoreIssues {
                repo: PathBuf::from("/repo"),
                desired_count: 20,
            },
            Command::SearchIssues {
                repo: PathBuf::from("/repo"),
                query: "bug".to_string(),
            },
            Command::ClearIssueSearch {
                repo: PathBuf::from("/repo"),
            },
        ];

        for cmd in daemon_commands {
            let result = run_execute(cmd, &registry, &data, &runner).await;
            assert_error_contains(result, "daemon-level command");
        }
    }

    // -----------------------------------------------------------------------
    // Tests: workspace_config helper
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_config_builds_correct_struct() {
        let config = workspace_config(
            Path::new("/nonexistent-repo"),
            "my-branch",
            Path::new("/repo/wt"),
            "claude",
            &config_base(),
        );

        assert_eq!(config.name, "my-branch");
        assert_eq!(config.working_directory, PathBuf::from("/repo/wt"));
        assert_eq!(
            config.template_vars.get("main_command"),
            Some(&"claude".to_string())
        );
        assert!(
            config.template_yaml.is_none(),
            "no template file should exist at test paths"
        );
    }
}
