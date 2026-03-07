use flotilla_core::config;
use flotilla_core::daemon::DaemonHandle;
use flotilla_core::in_process::InProcessDaemon;
use flotilla_tui::app;
use flotilla_tui::event;
use flotilla_tui::event_log;
use flotilla_tui::event_log::LevelExt;
use flotilla_tui::socket::SocketDaemon;
use flotilla_tui::ui;

use clap::Parser;
use color_eyre::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
};
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

/// Flotilla: TUI dashboard for managing development workspaces
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Git repo roots (repeatable; auto-detected from cwd if omitted)
    #[arg(long)]
    repo_root: Vec<PathBuf>,

    /// Config directory
    #[arg(long)]
    config_dir: Option<PathBuf>,

    /// Socket path (default: ${config_dir}/flotilla.sock)
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Run in embedded mode (no daemon, in-process)
    #[arg(long)]
    embedded: bool,

    #[command(subcommand)]
    command: Option<SubCommand>,
}

#[derive(clap::Subcommand)]
enum SubCommand {
    /// Run the daemon server
    Daemon {
        /// Idle timeout in seconds (0 = no timeout)
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Print repo list and state
    Status,
    /// Stream daemon events to stdout
    Watch,
}

impl Cli {
    fn config_dir(&self) -> PathBuf {
        self.config_dir
            .clone()
            .unwrap_or_else(flotilla_core::config::flotilla_config_dir)
    }

    fn socket_path(&self) -> PathBuf {
        self.socket
            .clone()
            .unwrap_or_else(|| self.config_dir().join("flotilla.sock"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();

    match &cli.command {
        Some(SubCommand::Daemon { timeout }) => run_daemon(&cli, *timeout).await,
        Some(SubCommand::Status) => run_status(&cli).await,
        Some(SubCommand::Watch) => run_watch(&cli).await,
        None => run_tui(cli).await,
    }
}

async fn run_tui(cli: Cli) -> Result<()> {
    event_log::init();
    let startup = std::time::Instant::now();

    let daemon: Arc<dyn DaemonHandle> = if cli.embedded {
        // Embedded mode — current behavior
        let repo_roots = resolve_repo_roots(&cli.repo_root);
        if repo_roots.is_empty() {
            eprintln!("Error: no git repositories found (use --repo-root to specify)");
            std::process::exit(1);
        }
        info!(
            "config loaded: {} repo(s) in {:.0?}",
            repo_roots.len(),
            startup.elapsed()
        );
        let daemon = InProcessDaemon::new(repo_roots).await;
        info!("embedded daemon started in {:.0?}", startup.elapsed());
        daemon as Arc<dyn DaemonHandle>
    } else {
        // Socket mode — connect or auto-spawn
        let socket_path = cli.socket_path();
        let daemon = connect_or_spawn(&socket_path, &cli)
            .await
            .map_err(|e| color_eyre::eyre::eyre!(e))?;
        info!("connected to daemon in {:.0?}", startup.elapsed());
        daemon as Arc<dyn DaemonHandle>
    };

    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;
    show_splash(&mut terminal)?;

    let repos_info = daemon.list_repos().await.unwrap_or_default();
    let mut app = app::App::new(daemon.clone(), repos_info);

    // Set up event handler and attach daemon events
    let mut events = event::EventHandler::new(Duration::from_millis(250));
    events.attach_daemon(daemon.subscribe());

    loop {
        terminal.draw(|f| ui::render(&app.model, &mut app.ui, f))?;

        if let Some(evt) = events.next().await {
            match evt {
                event::Event::Daemon(daemon_evt) => {
                    app.handle_daemon_event(daemon_evt);
                }
                event::Event::Key(k) => {
                    let is_normal = matches!(app.ui.mode, app::UiMode::Normal);
                    if k.code == crossterm::event::KeyCode::Char('r') && is_normal {
                        // Trigger immediate refresh on active repo via daemon
                        let repo = app.model.active_repo_root().clone();
                        let daemon = app.daemon.clone();
                        tokio::spawn(async move {
                            let _ = daemon.refresh(&repo).await;
                        });
                    } else {
                        app.handle_key(k);
                    }
                }
                event::Event::Mouse(m) => {
                    use crossterm::event::{MouseButton, MouseEventKind};
                    match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            let x = m.column;
                            let y = m.row;
                            let mut tab_clicked = false;

                            // Check event log filter area (click cycles level)
                            let ef = app.ui.layout.event_log_filter_area;
                            if x >= ef.x && x < ef.x + ef.width && y >= ef.y && y < ef.y + ef.height
                            {
                                app.ui.event_log.filter = app.ui.event_log.filter.cycle();
                                app.ui.event_log.count = 0;
                                tab_clicked = true;
                            }

                            // Check which tab area was clicked
                            if !tab_clicked {
                                let hit = app
                                    .ui
                                    .layout
                                    .tab_areas
                                    .iter()
                                    .find(|(_, r)| {
                                        x >= r.x
                                            && x < r.x + r.width
                                            && y >= r.y
                                            && y < r.y + r.height
                                    })
                                    .map(|(id, _)| id.clone());

                                match hit {
                                    Some(app::TabId::Flotilla) => {
                                        app.ui.mode = app::UiMode::Config;
                                        app.ui.drag.dragging_tab = None;
                                        tab_clicked = true;
                                    }
                                    Some(app::TabId::Repo(i)) => {
                                        app.switch_tab(i);
                                        app.ui.drag.dragging_tab = Some(i);
                                        app.ui.drag.start_x = x;
                                        app.ui.drag.active = false;
                                        tab_clicked = true;
                                    }
                                    Some(app::TabId::Gear) if !app.ui.mode.is_config() => {
                                        let sp = app.active_ui().show_providers;
                                        app.active_ui_mut().show_providers = !sp;
                                        tab_clicked = true;
                                    }
                                    Some(app::TabId::Add) => {
                                        let mut input = tui_input::Input::default();
                                        if let Some(parent) = app.model.active_repo_root().parent()
                                        {
                                            let parent_str = format!("{}/", parent.display());
                                            input = tui_input::Input::from(parent_str.as_str());
                                        }
                                        app.ui.mode = app::UiMode::FilePicker {
                                            input,
                                            dir_entries: Vec::new(),
                                            selected: 0,
                                        };
                                        app.refresh_dir_listing();
                                        tab_clicked = true;
                                    }
                                    _ => {}
                                }
                            }
                            if !tab_clicked {
                                app.ui.drag.dragging_tab = None;
                                app.handle_mouse(m);
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if let Some(dragging_idx) = app.ui.drag.dragging_tab {
                                if !app.ui.drag.active {
                                    let dx = (m.column as i16 - app.ui.drag.start_x as i16)
                                        .unsigned_abs();
                                    if dx >= 2 {
                                        app.ui.drag.active = true;
                                    }
                                }
                                if app.ui.drag.active {
                                    for (id, r) in &app.ui.layout.tab_areas {
                                        if let app::TabId::Repo(i) = *id {
                                            if m.column >= r.x
                                                && m.column < r.x + r.width
                                                && m.row >= r.y
                                                && m.row < r.y + r.height
                                                && i != dragging_idx
                                            {
                                                app.model.repo_order.swap(dragging_idx, i);
                                                app.model.active_repo = i;
                                                app.ui.drag.dragging_tab = Some(i);
                                                break;
                                            }
                                        }
                                    }
                                }
                            } else {
                                app.handle_mouse(m);
                            }
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            if app.ui.drag.dragging_tab.take().is_some() {
                                if app.ui.drag.active {
                                    config::save_tab_order(&app.model.repo_order);
                                }
                                app.ui.drag.active = false;
                            }
                        }
                        _ => {
                            app.handle_mouse(m);
                        }
                    }
                }
                event::Event::Tick => {}
            }
        }

        // Process proto command queue — routed through daemon-side executor
        while let Some(cmd) = app.proto_commands.take_next() {
            app::executor::execute(cmd, &mut app).await;
        }

        if app.should_quit {
            break;
        }
    }

    execute!(stdout(), DisableMouseCapture)?;
    ratatui::restore();
    Ok(())
}

async fn connect_or_spawn(
    socket_path: &std::path::Path,
    cli: &Cli,
) -> Result<Arc<SocketDaemon>, String> {
    // Try to connect to existing daemon
    if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
        return Ok(daemon);
    }

    // Clean up stale socket
    let _ = std::fs::remove_file(socket_path);

    // Spawn daemon process
    let exe = std::env::current_exe().map_err(|e| format!("can't find self: {e}"))?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("daemon");
    if let Some(ref config_dir) = cli.config_dir {
        cmd.arg("--config-dir").arg(config_dir);
    }
    if let Some(ref socket) = cli.socket {
        cmd.arg("--socket").arg(socket);
    }
    // Detach: redirect stdio, log stderr to file for debugging
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    let log_file = cli.config_dir().join("daemon.log");
    let _ = std::fs::create_dir_all(cli.config_dir());
    let stderr = std::fs::File::create(&log_file)
        .map(std::process::Stdio::from)
        .unwrap_or_else(|_| std::process::Stdio::null());
    cmd.stderr(stderr);
    cmd.spawn()
        .map_err(|e| format!("failed to spawn daemon: {e}"))?;

    // Retry connection with backoff
    let delays = [50, 100, 200, 400, 800];
    for delay_ms in delays {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        if let Ok(daemon) = SocketDaemon::connect(socket_path).await {
            return Ok(daemon);
        }
    }

    Err("timed out waiting for daemon to start".into())
}

async fn run_daemon(cli: &Cli, timeout_secs: u64) -> Result<()> {
    // Initialize logging to stderr (no TUI here)
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let socket_path = cli.socket_path();
    let timeout = if timeout_secs == 0 {
        Duration::from_secs(u64::MAX)
    } else {
        Duration::from_secs(timeout_secs)
    };

    // Load repos from config
    let repo_roots = config::load_repos();
    info!("starting daemon with {} repo(s)", repo_roots.len());

    let server = flotilla_daemon::server::DaemonServer::new(repo_roots, socket_path, timeout).await;

    server.run().await.map_err(|e| color_eyre::eyre::eyre!(e))
}

async fn run_status(cli: &Cli) -> Result<()> {
    let socket_path = cli.socket_path();
    let daemon = SocketDaemon::connect(&socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("cannot connect to daemon: {e}"))?;

    let repos = daemon
        .list_repos()
        .await
        .map_err(|e| color_eyre::eyre::eyre!("{e}"))?;

    if repos.is_empty() {
        println!("No repos tracked.");
        return Ok(());
    }

    for repo in &repos {
        let name = &repo.name;
        let path = repo.path.display();
        let health: Vec<String> = repo
            .provider_health
            .iter()
            .map(|(k, v)| format!("{k}: {}", if *v { "ok" } else { "error" }))
            .collect();
        let loading = if repo.loading { " (loading)" } else { "" };
        println!("{name}{loading}  {path}");
        if !health.is_empty() {
            println!("  providers: {}", health.join(", "));
        }
    }

    Ok(())
}

async fn run_watch(cli: &Cli) -> Result<()> {
    let socket_path = cli.socket_path();
    let daemon = SocketDaemon::connect(&socket_path)
        .await
        .map_err(|e| color_eyre::eyre::eyre!("cannot connect to daemon: {e}"))?;

    let mut rx = daemon.subscribe();
    println!("watching events (Ctrl-C to stop)...");

    loop {
        match rx.recv().await {
            Ok(event) => {
                let json =
                    serde_json::to_string_pretty(&event).unwrap_or_else(|_| format!("{event:?}"));
                println!("{json}");
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("warning: skipped {n} events");
            }
            Err(_) => {
                eprintln!("daemon disconnected");
                break;
            }
        }
    }

    Ok(())
}

fn show_splash(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    use ratatui_image::{picker::Picker, StatefulImage};

    let img_bytes = include_bytes!("../assets/splash.png");
    let dyn_img = image::load_from_memory(img_bytes)
        .map_err(|e| color_eyre::eyre::eyre!("splash image: {e}"))?;

    let img_w = dyn_img.width() as f64;
    let img_h = dyn_img.height() as f64;

    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut protocol = picker.new_resize_protocol(dyn_img);

    let deadline = std::time::Instant::now() + Duration::from_millis(700);

    loop {
        terminal.draw(|f| {
            use ratatui::layout::{Constraint, Flex, Layout};
            let area = f.area();
            let scale = (area.width as f64 / img_w).min(area.height as f64 * 2.0 / img_h);
            let rw = (img_w * scale) as u16;
            let rh = (img_h * scale / 2.0) as u16;
            let [area] = Layout::horizontal([Constraint::Length(rw.min(area.width))])
                .flex(Flex::Center)
                .areas(area);
            let [area] = Layout::vertical([Constraint::Length(rh.min(area.height))])
                .flex(Flex::Center)
                .areas(area);
            let widget = StatefulImage::default();
            f.render_stateful_widget(widget, area, &mut protocol);
        })?;

        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        if crossterm::event::poll(remaining.min(Duration::from_millis(50)))? {
            let _ = crossterm::event::read()?;
            break;
        }
    }
    Ok(())
}

/// Collect repo roots: persisted (in saved tab order) first, then CLI args, then auto-detect from cwd.
/// Persists any new repos and saves tab order.
fn resolve_repo_roots(cli_roots: &[PathBuf]) -> Vec<PathBuf> {
    use flotilla_core::providers::vcs::git::GitVcs;
    use flotilla_core::providers::vcs::Vcs;
    use flotilla_core::providers::ProcessCommandRunner;

    let mut repo_roots: Vec<PathBuf> = Vec::new();

    // 1. Persisted repos in saved tab order
    let persisted = config::load_repos();
    let tab_order = config::load_tab_order();
    if let Some(order) = tab_order {
        for path in &order {
            if persisted.contains(path) && !repo_roots.contains(path) {
                repo_roots.push(path.clone());
            }
        }
        // Any persisted repos not in the order file go at the end
        for path in &persisted {
            if !repo_roots.contains(path) {
                repo_roots.push(path.clone());
            }
        }
    } else {
        repo_roots.extend(persisted);
    }

    // 2. CLI args (appended after persisted)
    for root in cli_roots {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if !repo_roots.contains(&canonical) {
            repo_roots.push(canonical);
        }
    }

    // 3. Auto-detect from cwd — resolve to main repo root (not worktree)
    let cwd = std::env::current_dir().ok();
    if let Some(ref cwd) = cwd {
        let git = GitVcs::new(Arc::new(ProcessCommandRunner));
        if let Some(repo_root) = git.resolve_repo_root(cwd) {
            if !repo_roots.contains(&repo_root) {
                repo_roots.push(repo_root);
            }
        }
    }

    // Persist any new repos and save tab order
    for path in &repo_roots {
        config::save_repo(path);
    }
    config::save_tab_order(&repo_roots);

    repo_roots
}
