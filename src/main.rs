mod actions;
mod app;
mod data;
mod event;
mod ui;

use std::path::PathBuf;
use std::time::Duration;
use color_eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // Detect repo root
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    let repo_root = if output.status.success() {
        PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
    } else {
        eprintln!("Error: not in a git repository");
        std::process::exit(1);
    };

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, repo_root).await;
    ratatui::restore();
    result
}

async fn run(terminal: &mut ratatui::DefaultTerminal, repo_root: PathBuf) -> Result<()> {
    let mut app = app::App::new(repo_root);
    let mut events = event::EventHandler::new(Duration::from_millis(250));

    // Initial data load
    app.refresh_data().await;

    loop {
        terminal.draw(|f| ui::render(&mut app, f))?;

        if let Some(evt) = events.next().await {
            match evt {
                event::Event::Key(k) => {
                    if k.code == crossterm::event::KeyCode::Char('r') {
                        app.refresh_data().await;
                    } else {
                        app.handle_key(k);
                    }
                }
                event::Event::Tick => {
                    // Check if auto-refresh is due
                    // (tick-based refresh will be refined later)
                }
            }
        }

        // Process pending actions
        match app.take_pending_action() {
            app::PendingAction::SwitchWorktree(i) => {
                if let Some(wt) = app.data.worktrees.get(i) {
                    let _ = actions::switch_to_worktree(&wt.path).await;
                }
            }
            app::PendingAction::RemoveWorktree(i) => {
                if let Some(wt) = app.data.worktrees.get(i) {
                    let branch = wt.branch.clone();
                    let repo = app.repo_root.clone();
                    let _ = actions::remove_worktree(&branch, &repo).await;
                    app.refresh_data().await;
                }
            }
            app::PendingAction::OpenPr(number) => {
                let repo = app.repo_root.clone();
                let _ = actions::open_pr_in_browser(number, &repo).await;
            }
            app::PendingAction::CreateWorktree(branch) => {
                let repo = app.repo_root.clone();
                let _ = actions::create_worktree(&branch, &repo).await;
                app.refresh_data().await;
            }
            app::PendingAction::Refresh => {
                app.refresh_data().await;
            }
            app::PendingAction::None => {}
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
