//! Daemon-side command executor.
//!
//! Takes a `ProtoCommand`, the repo context, and returns a `CommandResult`.
//! No UI state mutation — all results are carried in the return value.

use std::path::Path;

use flotilla_protocol::{CommandResult, ProtoCommand, ProtoDeleteInfo};
use tracing::{debug, error, info};

use crate::provider_data::ProviderData;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::WorkspaceConfig;
use crate::{data, providers};

/// Execute a `ProtoCommand` against the given repo context.
///
/// Commands that are handled at the daemon level (AddRepo, RemoveRepo, Refresh)
/// should not reach this function — the caller should handle them directly.
pub async fn execute(
    cmd: ProtoCommand,
    repo_root: &Path,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
) -> CommandResult {
    match cmd {
        ProtoCommand::SwitchWorktree { path } => {
            if let Some(co) = providers_data.checkouts.get(&path).cloned() {
                info!("entering workspace for {}", co.branch);
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let config = workspace_config(repo_root, &co.branch, &co.path, "claude");
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        return CommandResult::Error { message: e };
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error {
                    message: format!("checkout not found: {}", path.display()),
                }
            }
        }

        ProtoCommand::SelectWorkspace { ws_ref } => {
            info!("switching to workspace {ws_ref}");
            if let Some((_, ws_mgr)) = &registry.workspace_manager {
                if let Err(e) = ws_mgr.select_workspace(&ws_ref).await {
                    return CommandResult::Error { message: e };
                }
            }
            CommandResult::Ok
        }

        ProtoCommand::CreateWorktree {
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
                Some(Ok(checkout)) => {
                    // Write issue links to git config
                    if !issue_ids.is_empty() {
                        write_branch_issue_links(repo_root, &branch, &issue_ids).await;
                    }
                    info!("created checkout at {}", checkout.path.display());
                    // Create workspace if manager available
                    if let Some((_, ws_mgr)) = &registry.workspace_manager {
                        let config =
                            workspace_config(repo_root, &branch, &checkout.path, "claude");
                        if let Err(e) = ws_mgr.create_workspace(&config).await {
                            // Checkout was created but workspace failed — report as error
                            // but the worktree still exists
                            error!("workspace creation failed after checkout: {e}");
                        }
                    }
                    CommandResult::WorktreeCreated {
                        branch: branch.clone(),
                    }
                }
                Some(Err(e)) => {
                    error!("create worktree failed: {e}");
                    CommandResult::Error { message: e }
                }
                None => CommandResult::Error {
                    message: "No checkout manager available".to_string(),
                },
            }
        }

        ProtoCommand::RemoveCheckout { branch } => {
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

        ProtoCommand::FetchDeleteInfo {
            branch,
            worktree_path,
            pr_number,
        } => {
            let info = data::fetch_delete_confirm_info(
                &branch,
                worktree_path.as_deref(),
                pr_number.as_deref(),
                repo_root,
            )
            .await;
            CommandResult::DeleteInfo(ProtoDeleteInfo {
                branch: info.branch,
                pr_status: info.pr_status,
                merge_commit_sha: info.merge_commit_sha,
                unpushed_commits: info.unpushed_commits,
                has_uncommitted: info.has_uncommitted,
                base_detection_warning: info.base_detection_warning,
            })
        }

        ProtoCommand::OpenPr { id } => {
            debug!("opening PR {id} in browser");
            if let Some(cr) = registry.code_review.values().next() {
                let _ = cr.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }

        ProtoCommand::OpenIssueBrowser { id } => {
            debug!("opening issue {id} in browser");
            if let Some(it) = registry.issue_trackers.values().next() {
                let _ = it.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }

        ProtoCommand::LinkIssuesToPr { pr_id, issue_ids } => {
            info!("linking issues {:?} to PR #{pr_id}", issue_ids);
            let body_result = providers::run_cmd(
                "gh",
                &["pr", "view", &pr_id, "--json", "body", "--jq", ".body"],
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
                    let result = providers::run_cmd(
                        "gh",
                        &["pr", "edit", &pr_id, "--body", &new_body],
                        repo_root,
                    )
                    .await;
                    match result {
                        Ok(_) => {
                            info!("linked issues to PR #{pr_id}");
                            CommandResult::Ok
                        }
                        Err(e) => {
                            error!("failed to edit PR: {e}");
                            CommandResult::Error { message: e }
                        }
                    }
                }
                Err(e) => {
                    error!("failed to read PR body: {e}");
                    CommandResult::Error { message: e }
                }
            }
        }

        ProtoCommand::ArchiveSession { session_id } => {
            if providers_data.sessions.contains_key(session_id.as_str()) {
                info!("archiving session {session_id}");
                let result = if let Some(ca) = registry.coding_agents.values().next() {
                    Some(ca.archive_session(&session_id).await)
                } else {
                    None
                };
                match result {
                    Some(Ok(())) => CommandResult::Ok,
                    Some(Err(e)) => CommandResult::Error { message: e },
                    None => CommandResult::Error {
                        message: "No coding agent available".to_string(),
                    },
                }
            } else {
                CommandResult::Error {
                    message: format!("session not found: {session_id}"),
                }
            }
        }

        ProtoCommand::GenerateBranchName { issue_keys } => {
            let issues: Vec<(String, String)> = issue_keys
                .iter()
                .filter_map(|k| providers_data.issues.get(k.as_str()))
                .map(|issue| (issue.id.clone(), issue.title.clone()))
                .collect();

            // Collect (provider_name, issue_id) pairs for the created branch
            let issue_id_pairs: Vec<(String, String)> = {
                let provider = registry
                    .issue_trackers
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| "github".to_string());
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

        ProtoCommand::TeleportSession {
            session_id,
            branch,
            checkout_key,
        } => {
            info!("teleporting to session {session_id}");
            let claude_bin =
                providers::resolve_claude_path().await.unwrap_or_else(|| "claude".into());
            let teleport_cmd = format!("{} --teleport {}", claude_bin, session_id);
            let wt_path = if let Some(ref key) = checkout_key {
                providers_data
                    .checkouts
                    .get(key)
                    .map(|co| co.path.clone())
            } else if let Some(branch_name) = &branch {
                let checkout_result =
                    if let Some(cm) = registry.checkout_managers.values().next() {
                        cm.create_checkout(repo_root, branch_name, false).await.ok()
                    } else {
                        None
                    };
                checkout_result.map(|c| c.path)
            } else {
                None
            };
            if let Some(path) = wt_path {
                let name = branch.as_deref().unwrap_or("session");
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let config = workspace_config(repo_root, name, &path, &teleport_cmd);
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        return CommandResult::Error { message: e };
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error {
                    message: "Could not determine worktree path for teleport".to_string(),
                }
            }
        }

        // These are handled at the daemon level (InProcessDaemon / SocketDaemon),
        // not by the per-repo executor. If they reach here, it's a routing bug.
        ProtoCommand::AddRepo { .. } | ProtoCommand::RemoveRepo { .. } | ProtoCommand::Refresh => {
            CommandResult::Error {
                message: "bug: daemon-level command reached per-repo executor".to_string(),
            }
        }
    }
}

/// Build a WorkspaceConfig from repo/branch/dir/command.
pub fn workspace_config(
    repo_root: &Path,
    name: &str,
    working_dir: &Path,
    main_command: &str,
) -> WorkspaceConfig {
    let tmpl_path = repo_root.join(".flotilla/workspace.yaml");
    let template_yaml = std::fs::read_to_string(&tmpl_path).ok().or_else(|| {
        let global_path = dirs::home_dir()?.join(".config/flotilla/workspace.yaml");
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
        if let Err(e) = providers::run_cmd("git", &["config", &key, &value], repo_root).await {
            tracing::warn!("failed to write issue link: {e}");
        }
    }
}
