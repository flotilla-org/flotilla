use tracing::{info, debug, error};

use crate::data;
use crate::config;
use crate::providers;
use super::command::Command;
use super::ui_state::UiMode;
use super::App;

/// Execute a single command against the app state.
pub async fn execute(cmd: Command, app: &mut App) {
    app.model.status_message = None;
    match cmd {
        Command::SwitchWorktree(path) => {
            if let Some(co) = app.model.active().data.providers.checkouts.get(&path).cloned() {
                info!("entering workspace for {}", co.branch);
                let ws_result = if let Some((_, ws_mgr)) = &app.model.active().registry.workspace_manager {
                    let config = workspace_config(app.model.active_repo_root(), &co.branch, &co.path, "claude");
                    Some(ws_mgr.create_workspace(&config).await)
                } else {
                    None
                };
                if let Some(Err(e)) = ws_result {
                    app.model.status_message = Some(e);
                }
                trigger_active_refresh(app);
            }
        }
        Command::SelectWorkspace(ws_ref) => {
            info!("switching to workspace {ws_ref}");
            if let Some((_, ws_mgr)) = &app.model.active().registry.workspace_manager {
                if let Err(e) = ws_mgr.select_workspace(&ws_ref).await {
                    app.model.status_message = Some(e);
                }
            }
        }
        Command::FetchDeleteInfo(si) => {
            let table_idx = app.active_ui().table_view.selectable_indices.get(si).copied();
            if let Some(table_idx) = table_idx {
                if let Some(data::TableEntry::Item(item)) = app.active_ui().table_view.table_entries.get(table_idx).cloned() {
                    let branch = item.branch.clone().unwrap_or_default();
                    let wt_path = item.checkout_key.clone();
                    let pr_id = item.pr_key.clone();
                    let repo_root = app.model.active_repo_root().clone();
                    let info = data::fetch_delete_confirm_info(
                        &branch,
                        wt_path.as_deref(),
                        pr_id.as_deref(),
                        &repo_root,
                    ).await;
                    if let UiMode::DeleteConfirm { info: ref mut slot, ref mut loading } = app.ui.mode {
                        *slot = Some(info);
                        *loading = false;
                    }
                }
            }
        }
        Command::ConfirmDelete => {
            let delete_info = if let UiMode::DeleteConfirm { ref mut info, .. } = app.ui.mode {
                info.take()
            } else {
                None
            };
            if let Some(info) = delete_info {
                info!("deleting {} {}", app.model.active_labels().checkouts.noun, info.branch);
                let repo = app.model.active_repo_root().clone();
                let result = if let Some(cm) = app.model.active().registry.checkout_managers.values().next() {
                    Some(cm.remove_checkout(repo.as_path(), &info.branch).await)
                } else {
                    None
                };
                if let Some(Err(e)) = result {
                    app.model.status_message = Some(e);
                }
                trigger_active_refresh(app);
            }
        }
        Command::OpenPr(id) => {
            debug!("opening {} {id} in browser", app.model.active_labels().code_review.abbr);
            let repo = app.model.active_repo_root().clone();
            if let Some(cr) = app.model.active().registry.code_review.values().next() {
                let _ = cr.open_in_browser(&repo, &id).await;
            }
        }
        Command::OpenIssueBrowser(id) => {
            debug!("opening issue {id} in browser");
            let repo = app.model.active_repo_root().clone();
            if let Some(it) = app.model.active().registry.issue_trackers.values().next() {
                let _ = it.open_in_browser(&repo, &id).await;
            }
        }
        Command::LinkIssuesToPr { pr_id, issue_ids } => {
            info!("linking issues {:?} to PR #{pr_id}", issue_ids);
            let repo = app.model.active_repo_root().clone();
            let body_result = providers::run_cmd(
                "gh",
                &["pr", "view", &pr_id, "--json", "body", "--jq", ".body"],
                &repo,
            ).await;
            match body_result {
                Ok(current_body) => {
                    let fixes_lines: Vec<String> = issue_ids.iter()
                        .map(|id| format!("Fixes #{id}"))
                        .collect();
                    let new_body = if current_body.trim().is_empty() {
                        fixes_lines.join("\n")
                    } else {
                        format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                    };
                    let result = providers::run_cmd(
                        "gh",
                        &["pr", "edit", &pr_id, "--body", &new_body],
                        &repo,
                    ).await;
                    if let Err(e) = result {
                        error!("failed to edit PR: {e}");
                        app.model.status_message = Some(e);
                    } else {
                        info!("linked issues to PR #{pr_id}");
                    }
                }
                Err(e) => {
                    error!("failed to read PR body: {e}");
                    app.model.status_message = Some(e);
                }
            }
            trigger_active_refresh(app);
        }
        Command::CreateWorktree { branch, create_branch, issue_ids } => {
            info!("creating {} {branch}", app.model.active_labels().checkouts.noun);
            let repo = app.model.active_repo_root().clone();
            let checkout_result = if let Some(cm) = app.model.active().registry.checkout_managers.values().next() {
                Some(cm.create_checkout(repo.as_path(), &branch, create_branch).await)
            } else {
                None
            };
            match checkout_result {
                Some(Ok(checkout)) => {
                    // Write issue links to git config
                    if !issue_ids.is_empty() {
                        write_branch_issue_links(app.model.active_repo_root(), &branch, &issue_ids).await;
                    }
                    info!("created {} at {}", app.model.active_labels().checkouts.noun, checkout.path.display());
                    let ws_result = if let Some((_, ws_mgr)) = &app.model.active().registry.workspace_manager {
                        let config = workspace_config(app.model.active_repo_root(), &branch, &checkout.path, "claude");
                        Some(ws_mgr.create_workspace(&config).await)
                    } else {
                        None
                    };
                    if let Some(Err(e)) = ws_result {
                        app.model.status_message = Some(e);
                    }
                }
                Some(Err(e)) => {
                    error!("create worktree failed: {e}");
                    app.model.status_message = Some(e);
                }
                None => app.model.status_message = Some("No checkout manager available".to_string()),
            }
            trigger_active_refresh(app);
        }
        Command::ArchiveSession(session_id) => {
            if app.model.active().data.providers.sessions.contains_key(session_id.as_str()) {
                info!("archiving session {}", session_id);
                let result = if let Some(ca) = app.model.active().registry.coding_agents.values().next() {
                    Some(ca.archive_session(&session_id).await)
                } else {
                    None
                };
                if let Some(Err(e)) = result {
                    app.model.status_message = Some(e);
                }
                trigger_active_refresh(app);
            }
        }
        Command::TeleportSession { session_id, branch, checkout_key } => {
            info!("teleporting to session {session_id}");
            let claude_bin = providers::resolve_claude_path().unwrap_or_else(|| "claude".into());
            let teleport_cmd = format!("{} --teleport {}", claude_bin, session_id);
            let wt_path = if let Some(ref key) = checkout_key {
                app.model.active().data.providers.checkouts.get(key).map(|co| co.path.clone())
            } else if let Some(branch_name) = &branch {
                let repo = app.model.active_repo_root().clone();
                let checkout_result = if let Some(cm) = app.model.active().registry.checkout_managers.values().next() {
                    cm.create_checkout(repo.as_path(), branch_name, false).await.ok()
                } else {
                    None
                };
                checkout_result.map(|c| c.path)
            } else {
                None
            };
            if let Some(path) = wt_path {
                let name = branch.as_deref().unwrap_or("session");
                let ws_result = if let Some((_, ws_mgr)) = &app.model.active().registry.workspace_manager {
                    let config = workspace_config(app.model.active_repo_root(), name, &path, &teleport_cmd);
                    Some(ws_mgr.create_workspace(&config).await)
                } else {
                    None
                };
                if let Some(Err(e)) = ws_result {
                    app.model.status_message = Some(e);
                }
            }
            trigger_active_refresh(app);
        }
        Command::GenerateBranchName(issue_keys) => {
            let issues: Vec<(String, String)> = issue_keys
                .iter()
                .filter_map(|k| app.model.active().data.providers.issues.get(k.as_str()))
                .map(|issue| (issue.id.clone(), issue.title.clone()))
                .collect();

            // Collect (provider_name, issue_id) pairs for the created branch
            let issue_id_pairs: Vec<(String, String)> = {
                let provider = app.model.active().registry.issue_trackers
                    .keys().next().cloned().unwrap_or_else(|| "github".to_string());
                issues.iter()
                    .map(|(id, _title)| (provider.clone(), id.clone()))
                    .collect()
            };
            app.pending_issue_ids = issue_id_pairs;

            info!("generating branch name");
            let branch_result = if let Some(ai) = app.model.active().registry.ai_utilities.values().next() {
                let context: Vec<String> = issues.iter()
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
                Some(Ok(branch)) => {
                    info!("AI suggested: {branch}");
                    app.prefill_branch_input(&branch);
                }
                _ => {
                    let fallback: Vec<String> = issues.iter()
                        .map(|(id, _)| format!("issue-{}", id))
                        .collect();
                    app.prefill_branch_input(&fallback.join("-"));
                }
            }
        }
        Command::AddRepo(path) => {
            info!("adding repo {}", path.display());
            config::save_repo(&path);
            app.add_repo(path);
            app.switch_tab(app.model.repo_order.len() - 1);
            config::save_tab_order(&app.model.repo_order);
            trigger_active_refresh(app);
        }
    }
}

pub fn workspace_config(
    repo_root: &std::path::Path,
    name: &str,
    working_dir: &std::path::Path,
    main_command: &str,
) -> crate::providers::types::WorkspaceConfig {
    let tmpl_path = repo_root.join(".flotilla/workspace.yaml");
    let template_yaml = std::fs::read_to_string(&tmpl_path).ok().or_else(|| {
        let global_path = dirs::home_dir()?.join(".config/flotilla/workspace.yaml");
        std::fs::read_to_string(global_path).ok()
    });
    let mut template_vars = std::collections::HashMap::new();
    template_vars.insert("main_command".to_string(), main_command.to_string());
    crate::providers::types::WorkspaceConfig {
        name: name.to_string(),
        working_directory: working_dir.to_path_buf(),
        template_vars,
        template_yaml,
    }
}

/// Trigger an immediate background refresh on the active repo.
fn trigger_active_refresh(app: &App) {
    if let Some(handle) = &app.model.active().refresh_handle {
        handle.trigger_refresh();
    }
}

async fn write_branch_issue_links(repo_root: &std::path::Path, branch: &str, issue_ids: &[(String, String)]) {
    use std::collections::HashMap;
    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider.entry(provider.as_str()).or_default().push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        if let Err(e) = crate::providers::run_cmd("git", &["config", &key, &value], repo_root).await {
            tracing::warn!("failed to write issue link: {e}");
        }
    }
}
