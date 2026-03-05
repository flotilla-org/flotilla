use std::path::PathBuf;
use tracing::{info, debug, error};

use crate::data;
use crate::config;
use crate::providers;
use super::command::Command;
use super::model::AppModel;
use super::ui_state::UiMode;
use super::App;

/// Execute a single command against the app state.
pub async fn execute(cmd: Command, app: &mut App) {
    app.model.status_message = None;
    match cmd {
        Command::SwitchWorktree(i) => {
            if let Some(co) = app.model.active().data.checkouts.get(i).cloned() {
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
                refresh_all(app).await;
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
            let table_idx = app.model.active().data.selectable_indices.get(si).copied();
            if let Some(table_idx) = table_idx {
                if let Some(data::TableEntry::Item(item)) = app.model.active().data.table_entries.get(table_idx).cloned() {
                    let branch = item.branch.clone().unwrap_or_default();
                    let wt_path = item.worktree_idx
                        .and_then(|idx| app.model.active().data.checkouts.get(idx))
                        .map(|co| co.path.clone());
                    let pr_id = item.pr_idx
                        .and_then(|idx| app.model.active().data.change_requests.get(idx))
                        .map(|cr| cr.id.clone());
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
                refresh_all(app).await;
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
        Command::CreateWorktree { branch, create_branch } => {
            info!("creating {} {branch}", app.model.active_labels().checkouts.noun);
            let repo = app.model.active_repo_root().clone();
            let checkout_result = if let Some(cm) = app.model.active().registry.checkout_managers.values().next() {
                Some(cm.create_checkout(repo.as_path(), &branch, create_branch).await)
            } else {
                None
            };
            match checkout_result {
                Some(Ok(checkout)) => {
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
            refresh_all(app).await;
        }
        Command::ArchiveSession(ses_idx) => {
            if let Some(session) = app.model.active().data.sessions.get(ses_idx).cloned() {
                info!("archiving session {}", session.id);
                let result = if let Some(ca) = app.model.active().registry.coding_agents.values().next() {
                    Some(ca.archive_session(&session.id).await)
                } else {
                    None
                };
                if let Some(Err(e)) = result {
                    app.model.status_message = Some(e);
                }
                refresh_all(app).await;
            }
        }
        Command::TeleportSession { session_id, branch, worktree_idx } => {
            info!("teleporting to session {session_id}");
            let claude_bin = providers::resolve_claude_path().unwrap_or_else(|| "claude".into());
            let teleport_cmd = format!("{} --teleport {}", claude_bin, session_id);
            let wt_path = if let Some(wt_idx) = worktree_idx {
                app.model.active().data.checkouts.get(wt_idx).map(|co| co.path.clone())
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
            refresh_all(app).await;
        }
        Command::GenerateBranchName(issue_idxs) => {
            let issues: Vec<(String, String)> = issue_idxs
                .iter()
                .filter_map(|&idx| app.model.active().data.issues.get(idx))
                .map(|issue| (issue.id.clone(), issue.title.clone()))
                .collect();

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
            refresh_all(app).await;
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

pub async fn refresh_all(app: &mut App) {
    let t = std::time::Instant::now();
    // Snapshot all repos for change detection
    let snapshots: Vec<_> = app.model.repo_order.iter()
        .map(|path| app.model.repos[path].data_snapshot())
        .collect();

    // Extract data stores AND registries (both moved out)
    let items: Vec<(PathBuf, data::DataStore, providers::registry::ProviderRegistry, providers::types::RepoCriteria)> = app.model.repo_order.iter()
        .map(|path| {
            let rm = app.model.repos.get_mut(path).unwrap();
            let ds = std::mem::take(&mut rm.data);
            let reg = std::mem::take(&mut rm.registry);
            let criteria = rm.repo_criteria.clone();
            (path.clone(), ds, reg, criteria)
        })
        .collect();

    let results = futures::future::join_all(
        items.into_iter().map(|(root, mut ds, registry, criteria)| {
            async move {
                let errors = ds.refresh(&root, &registry, &criteria).await;
                (root, ds, registry, errors)
            }
        })
    ).await;

    let mut all_errors: Vec<String> = Vec::new();
    for (i, (path, data, registry, errors)) in results.into_iter().enumerate() {
        let rm = app.model.repos.get_mut(&path).unwrap();
        rm.data = data;
        rm.registry = registry;

        // Deregister issue tracker if issues are disabled on this repo
        let issues_disabled = errors.iter().any(|e|
            e.category == "issues" && e.message.contains("has disabled issues")
        );
        if issues_disabled {
            rm.registry.issue_trackers.clear();
            rm.data.provider_health.remove("issue_tracker");
        }

        // Populate labels from provider traits
        let repo_labels = super::model::RepoLabels {
            checkouts: rm.registry.checkout_managers.values().next()
                .map(|cm| super::model::CategoryLabels {
                    section: cm.section_label().into(),
                    noun: cm.item_noun().into(),
                    abbr: cm.abbreviation().into(),
                })
                .unwrap_or_default(),
            code_review: rm.registry.code_review.values().next()
                .map(|cr| super::model::CategoryLabels {
                    section: cr.section_label().into(),
                    noun: cr.item_noun().into(),
                    abbr: cr.abbreviation().into(),
                })
                .unwrap_or_default(),
            issues: rm.registry.issue_trackers.values().next()
                .map(|it| super::model::CategoryLabels {
                    section: it.section_label().into(),
                    noun: it.item_noun().into(),
                    abbr: it.abbreviation().into(),
                })
                .unwrap_or_default(),
            sessions: rm.registry.coding_agents.values().next()
                .map(|ca| super::model::CategoryLabels {
                    section: ca.section_label().into(),
                    noun: ca.item_noun().into(),
                    abbr: ca.abbreviation().into(),
                })
                .unwrap_or_default(),
        };
        app.model.labels.insert(path.clone(), repo_labels);

        // Change detection
        let new_snapshot = rm.data_snapshot();
        if snapshots[i] != new_snapshot && i != app.model.active_repo {
            app.ui.repo_ui.get_mut(&path).unwrap().has_unseen_changes = true;
        }

        // Restore selection (UI state)
        let rui = app.ui.repo_ui.get_mut(&path).unwrap();
        if rm.data.selectable_indices.is_empty() {
            rui.selected_selectable_idx = None;
            rui.table_state.select(None);
        } else if rui.selected_selectable_idx.is_none() {
            rui.selected_selectable_idx = Some(0);
            rui.table_state.select(Some(rm.data.selectable_indices[0]));
        } else if let Some(si) = rui.selected_selectable_idx {
            let clamped = si.min(rm.data.selectable_indices.len() - 1);
            rui.selected_selectable_idx = Some(clamped);
            rui.table_state.select(Some(rm.data.selectable_indices[clamped]));
        }

        // Copy provider health from DataStore into model-level statuses
        let name = AppModel::repo_name(&path);

        for (kind, healthy) in &rm.data.provider_health {
            let provider_name = match *kind {
                "coding_agent" => rm.registry.coding_agents.keys().next(),
                "code_review" => rm.registry.code_review.keys().next(),
                "issue_tracker" => rm.registry.issue_trackers.keys().next(),
                _ => None,
            };
            if let Some(pname) = provider_name {
                let key = (path.clone(), kind.to_string(), pname.clone());
                let status = if *healthy { super::ProviderStatus::Ok } else { super::ProviderStatus::Error };
                app.model.provider_statuses.insert(key, status);
            }
        }

        if !errors.is_empty() {
            for e in &errors {
                if issues_disabled && e.category == "issues" {
                    debug!("{name}: issues disabled, deregistered issue tracker");
                    continue;
                }
                error!("{name}: {}: {}", e.category, e.message);
                all_errors.push(format!("{name}: {}: {}", e.category, e.message));
            }
        }
    }

    debug!("refresh complete in {:.0?}", t.elapsed());

    if !all_errors.is_empty() {
        app.model.status_message = Some(all_errors.join("; "));
    }
}
