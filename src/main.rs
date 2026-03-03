mod actions;
mod app;
mod data;
mod event;
mod template;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;
use clap::Parser;
use color_eyre::Result;
use crossterm::{execute, event::{EnableMouseCapture, DisableMouseCapture}};

/// TUI dashboard for managing development workspaces across cmux, worktrunk, and GitHub.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Git repo root (auto-detected from cwd if omitted)
    #[arg(long)]
    repo_root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    let repo_root = if let Some(root) = cli.repo_root {
        root
    } else {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        if output.status.success() {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        } else {
            eprintln!("Error: not in a git repository (use --repo-root to specify)");
            std::process::exit(1);
        }
    };

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, repo_root).await;
    execute!(stdout(), DisableMouseCapture)?;
    ratatui::restore();
    result
}

async fn run(terminal: &mut ratatui::DefaultTerminal, repo_root: PathBuf) -> Result<()> {
    let mut app = app::App::new(repo_root);
    let mut events = event::EventHandler::new(Duration::from_millis(250));
    let mut last_refresh = std::time::Instant::now();
    let refresh_interval = Duration::from_secs(10);

    // Initial data load
    let errors = app.refresh_data().await;
    if !errors.is_empty() {
        app.status_message = Some(errors.join("; "));
    }

    loop {
        terminal.draw(|f| ui::render(&mut app, f))?;

        if let Some(evt) = events.next().await {
            match evt {
                event::Event::Key(k) => {
                    if k.code == crossterm::event::KeyCode::Char('r')
                        && !app.show_action_menu
                        && app.input_mode == app::InputMode::Normal
                        && !app.show_help
                        && !app.show_delete_confirm
                    {
                        let errors = app.refresh_data().await;
                        if !errors.is_empty() {
                            app.status_message = Some(errors.join("; "));
                        }
                        last_refresh = std::time::Instant::now();
                    } else {
                        app.handle_key(k);
                    }
                }
                event::Event::Mouse(m) => {
                    app.handle_mouse(m);
                }
                event::Event::Tick => {
                    if last_refresh.elapsed() >= refresh_interval {
                        let errors = app.refresh_data().await;
                        if !errors.is_empty() {
                            app.status_message = Some(errors.join("; "));
                        }
                        last_refresh = std::time::Instant::now();
                    }
                }
            }
        }

        // Process pending actions — clear status only when user triggers an action
        let pending = app.take_pending_action();
        if !matches!(pending, app::PendingAction::None) {
            app.status_message = None;
        }
        match pending {
            app::PendingAction::SwitchWorktree(i) => {
                if let Some(wt) = app.data.worktrees.get(i) {
                    let tmpl = template::WorkspaceTemplate::load(&app.repo_root);
                    if let Err(e) = actions::create_cmux_workspace(
                        &tmpl,
                        &wt.path,
                        "claude",
                        &wt.branch,
                    ).await {
                        app.status_message = Some(e);
                    }
                    let errors = app.refresh_data().await;
                    if !errors.is_empty() {
                        let msg = errors.join("; ");
                        app.status_message = Some(match app.status_message.take() {
                            Some(prev) => format!("{prev}; {msg}"),
                            None => msg,
                        });
                    }
                }
            }
            app::PendingAction::SelectWorkspace(ws_ref) => {
                if let Err(e) = actions::select_cmux_workspace(&ws_ref).await {
                    app.status_message = Some(e);
                }
            }
            app::PendingAction::FetchDeleteInfo(si) => {
                if let Some(&table_idx) = app.data.selectable_indices.get(si) {
                    if let Some(data::TableEntry::Item(item)) = app.data.table_entries.get(table_idx) {
                        let branch = item.branch.clone().unwrap_or_default();
                        let wt_path = item.worktree_idx
                            .and_then(|idx| app.data.worktrees.get(idx))
                            .map(|wt| wt.path.clone());
                        let pr_number = item.pr_idx
                            .and_then(|idx| app.data.prs.get(idx))
                            .map(|pr| pr.number);
                        let info = data::fetch_delete_confirm_info(
                            &branch,
                            wt_path.as_ref(),
                            pr_number,
                            &app.repo_root,
                        ).await;
                        app.delete_confirm_info = Some(info);
                        app.delete_confirm_loading = false;
                    }
                }
            }
            app::PendingAction::ConfirmDelete => {
                if let Some(info) = app.delete_confirm_info.take() {
                    let repo = app.repo_root.clone();
                    if let Err(e) = actions::remove_worktree(&info.branch, &repo).await {
                        app.status_message = Some(e);
                    }
                    let errors = app.refresh_data().await;
                    if !errors.is_empty() {
                        let msg = errors.join("; ");
                        app.status_message = Some(match app.status_message.take() {
                            Some(prev) => format!("{prev}; {msg}"),
                            None => msg,
                        });
                    }
                }
            }
            app::PendingAction::OpenPr(number) => {
                let repo = app.repo_root.clone();
                let _ = actions::open_pr_in_browser(number, &repo).await;
            }
            app::PendingAction::OpenIssueBrowser(number) => {
                let repo = app.repo_root.clone();
                let _ = actions::open_issue_in_browser(number, &repo).await;
            }
            app::PendingAction::CreateWorktree(branch) => {
                let repo = app.repo_root.clone();
                match actions::create_worktree(&branch, &repo).await {
                    Ok(wt_path) => {
                        let tmpl = template::WorkspaceTemplate::load(&app.repo_root);
                        if let Err(e) = actions::create_cmux_workspace(
                            &tmpl,
                            &wt_path,
                            "claude",
                            &branch,
                        ).await {
                            app.status_message = Some(e);
                        }
                    }
                    Err(e) => app.status_message = Some(e),
                }
                app.refresh_data().await;
            }
            app::PendingAction::ArchiveSession(ses_idx) => {
                if let Some(session) = app.data.sessions.get(ses_idx) {
                    if let Err(e) = data::archive_session(&session.id).await {
                        app.status_message = Some(e);
                    }
                    let errors = app.refresh_data().await;
                    if !errors.is_empty() {
                        let msg = errors.join("; ");
                        app.status_message = Some(match app.status_message.take() {
                            Some(prev) => format!("{prev}; {msg}"),
                            None => msg,
                        });
                    }
                }
            }
            app::PendingAction::TeleportSession { session_id, branch, worktree_idx } => {
                let teleport_cmd = format!("claude --teleport {}", session_id);
                let tmpl = template::WorkspaceTemplate::load(&app.repo_root);
                let wt_path = if let Some(wt_idx) = worktree_idx {
                    app.data.worktrees.get(wt_idx).map(|wt| wt.path.clone())
                } else if let Some(branch_name) = &branch {
                    let repo = app.repo_root.clone();
                    actions::create_worktree(branch_name, &repo).await.ok()
                } else {
                    None
                };
                if let Some(path) = wt_path {
                    let name = branch.as_deref().unwrap_or("session");
                    if let Err(e) = actions::create_cmux_workspace(
                        &tmpl, &path, &teleport_cmd, name,
                    ).await {
                        app.status_message = Some(e);
                    }
                }
                app.refresh_data().await;
            }
            app::PendingAction::GenerateBranchName(issue_idxs) => {
                let issues: Vec<(i64, &str)> = issue_idxs
                    .iter()
                    .filter_map(|&idx| app.data.issues.get(idx))
                    .map(|issue| (issue.number, issue.title.as_str()))
                    .collect();
                let repo = app.repo_root.clone();
                match actions::generate_branch_name(&issues, &repo).await {
                    Ok(branch) => app.prefill_branch_input(&branch),
                    Err(_) => {
                        let fallback: Vec<String> = issues
                            .iter()
                            .map(|(num, _)| format!("issue-{}", num))
                            .collect();
                        app.prefill_branch_input(&fallback.join("-"));
                    }
                }
            }
            app::PendingAction::None => {}
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
