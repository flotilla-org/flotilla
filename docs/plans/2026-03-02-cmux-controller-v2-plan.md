# cmux-controller v2 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a persistent Ratatui TUI dashboard that aggregates worktrees, PRs, issues, and web sessions into a tabbed interface with preview pane, action menus, and workspace template support.

**Architecture:** Flat `App` struct with async event loop (tokio + crossterm EventStream). Data fetched by spawning CLI subprocesses (`wt`, `cmux`, `gh`) asynchronously. Actions dispatched back to those CLIs. Rendering via ratatui with tabs, stateful list, preview pane, and popup overlays.

**Tech Stack:** Rust, ratatui 0.29, crossterm 0.28, tokio 1, serde/serde_json, clap, tui-input, strum, color-eyre, serde_yaml

---

### Task 1: Scaffold Rust project with Cargo.toml and boilerplate

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`

**Step 1: Initialize the cargo project**

Run: `cargo init --name cmux-controller`
Expected: Creates `Cargo.toml` and `src/main.rs`

**Step 2: Set up Cargo.toml with all dependencies**

Replace `Cargo.toml` with:

```toml
[package]
name = "cmux-controller"
version = "0.1.0"
edition = "2021"

[dependencies]
ratatui = "0.29"
crossterm = { version = "0.28", features = ["event-stream"] }
tokio = { version = "1", features = ["full"] }
futures = "0.3"
color-eyre = "0.6"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
clap = { version = "4", features = ["derive"] }
strum = { version = "0.26", features = ["derive"] }
tui-input = "0.10"
```

**Step 3: Write minimal main.rs with async event loop**

```rust
use std::time::Duration;

use color_eyre::Result;
use crossterm::event::{EventStream, KeyCode, KeyEventKind};
use futures::{FutureExt, StreamExt};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style, Stylize},
    widgets::{Block, Paragraph},
    DefaultTerminal, Frame,
};

#[derive(Default)]
struct App {
    should_quit: bool,
}

impl App {
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            _ => {}
        }
    }

    fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let block = Block::bordered().title(" cmux-controller ");
        let text = Paragraph::new("Press q to quit")
            .block(block)
            .style(Style::default().fg(Color::White));
        frame.render_widget(text, area);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal).await;
    ratatui::restore();
    result
}

async fn run(terminal: &mut DefaultTerminal) -> Result<()> {
    let mut app = App::default();
    let mut reader = EventStream::new();
    let tick_rate = Duration::from_millis(250);
    let mut interval = tokio::time::interval(tick_rate);

    loop {
        terminal.draw(|f| app.render(f))?;

        let delay = interval.tick();
        let event = reader.next().fuse();

        tokio::select! {
            _ = delay => {}
            maybe = event => match maybe {
                Some(Ok(crossterm::event::Event::Key(k))) if k.kind == KeyEventKind::Press => {
                    app.handle_key(k);
                }
                _ => {}
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
```

**Step 4: Build and run**

Run: `cargo build`
Expected: Compiles without errors

Run: `cargo run`
Expected: Shows bordered box with "Press q to quit". Press q to exit cleanly.

**Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/
git commit -m "feat: scaffold Rust project with ratatui async event loop"
```

---

### Task 2: Add tab bar and app state structure

**Files:**
- Modify: `src/main.rs`

**Step 1: Add tab enum and app state**

Add above `App`:

```rust
use strum::{Display, EnumIter, FromRepr, IntoEnumIterator};

#[derive(Default, Clone, Copy, Display, FromRepr, EnumIter, PartialEq)]
enum Tab {
    #[default]
    #[strum(to_string = "Worktrees")]
    Worktrees,
    #[strum(to_string = "PRs")]
    Prs,
    #[strum(to_string = "Issues")]
    Issues,
    #[strum(to_string = "Sessions")]
    Sessions,
}

impl Tab {
    fn next(self) -> Self {
        let i = (self as usize + 1) % Self::iter().count();
        Self::from_repr(i).unwrap_or(self)
    }
    fn prev(self) -> Self {
        let count = Self::iter().count();
        let i = (self as usize + count - 1) % count;
        Self::from_repr(i).unwrap_or(self)
    }
}
```

Update `App`:

```rust
#[derive(Default)]
struct App {
    should_quit: bool,
    current_tab: Tab,
}

impl App {
    fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab => self.current_tab = self.current_tab.next(),
            KeyCode::BackTab => self.current_tab = self.current_tab.prev(),
            _ => {}
        }
    }
}
```

**Step 2: Render tabs and layout**

Replace `render`:

```rust
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    widgets::{Tabs, Block, Borders, Paragraph},
};

fn render(&self, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // tab bar
            Constraint::Min(0),    // main content
            Constraint::Length(1), // status bar
        ])
        .split(frame.area());

    // Tab bar
    let titles = Tab::iter().map(|t| t.to_string());
    let tabs = Tabs::new(titles)
        .select(self.current_tab as usize)
        .highlight_style(Style::default().bold().fg(Color::Cyan))
        .divider(" | ")
        .block(Block::bordered().title(" cmux-controller "));
    frame.render_widget(tabs, chunks[0]);

    // Main content (placeholder)
    let content = Paragraph::new(format!("Tab: {}", self.current_tab))
        .block(Block::bordered());
    frame.render_widget(content, chunks[1]);

    // Status bar
    let status = Paragraph::new(" tab:switch  q:quit")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, chunks[2]);
}
```

**Step 3: Build and test**

Run: `cargo run`
Expected: Tab bar shows [Worktrees | PRs | Issues | Sessions]. Tab/Shift-Tab switches between them. Content area shows current tab name. q quits.

**Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: add tab bar with worktrees/PRs/issues/sessions"
```

---

### Task 3: Split into modules

**Files:**
- Modify: `src/main.rs` (slim down to entry point)
- Create: `src/app.rs`
- Create: `src/event.rs`
- Create: `src/ui.rs`

**Step 1: Extract event handler to `src/event.rs`**

```rust
use crossterm::event::{EventStream, KeyEventKind};
use futures::{FutureExt, StreamExt};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum Event {
    Tick,
    Key(crossterm::event::KeyEvent),
}

pub struct EventHandler {
    rx: mpsc::UnboundedReceiver<Event>,
}

impl EventHandler {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            let mut interval = tokio::time::interval(tick_rate);
            loop {
                let delay = interval.tick();
                let event = reader.next().fuse();
                tokio::select! {
                    _ = delay => { let _ = tx.send(Event::Tick); }
                    maybe = event => match maybe {
                        Some(Ok(crossterm::event::Event::Key(k)))
                            if k.kind == KeyEventKind::Press =>
                        {
                            let _ = tx.send(Event::Key(k));
                        }
                        _ => {}
                    }
                }
            }
        });
        Self { rx }
    }

    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }
}
```

**Step 2: Extract app state and logic to `src/app.rs`**

Move the `Tab` enum, `App` struct, and `handle_key` to `src/app.rs`. Make fields and methods `pub`.

```rust
use crossterm::event::{KeyCode, KeyEvent};
use strum::{Display, EnumIter, FromRepr, IntoEnumIterator};

#[derive(Default, Clone, Copy, Display, FromRepr, EnumIter, PartialEq)]
pub enum Tab {
    #[default]
    #[strum(to_string = "Worktrees")]
    Worktrees,
    #[strum(to_string = "PRs")]
    Prs,
    #[strum(to_string = "Issues")]
    Issues,
    #[strum(to_string = "Sessions")]
    Sessions,
}

impl Tab {
    pub fn next(self) -> Self {
        let i = (self as usize + 1) % Self::iter().count();
        Self::from_repr(i).unwrap_or(self)
    }
    pub fn prev(self) -> Self {
        let count = Self::iter().count();
        let i = (self as usize + count - 1) % count;
        Self::from_repr(i).unwrap_or(self)
    }
}

#[derive(Default)]
pub struct App {
    pub should_quit: bool,
    pub current_tab: Tab,
}

impl App {
    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab => self.current_tab = self.current_tab.next(),
            KeyCode::BackTab => self.current_tab = self.current_tab.prev(),
            _ => {}
        }
    }

    pub fn tick(&mut self) {
        // Will be used for auto-refresh
    }
}
```

**Step 3: Extract rendering to `src/ui.rs`**

```rust
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style, Stylize},
    widgets::{Block, Paragraph, Tabs},
    Frame,
};
use strum::IntoEnumIterator;

use crate::app::App;
use crate::app::Tab;

pub fn render(app: &App, frame: &mut Frame) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_tabs(app, frame, chunks[0]);
    render_content(app, frame, chunks[1]);
    render_status_bar(frame, chunks[2]);
}

fn render_tabs(app: &App, frame: &mut Frame, area: ratatui::layout::Rect) {
    let titles = Tab::iter().map(|t| t.to_string());
    let tabs = Tabs::new(titles)
        .select(app.current_tab as usize)
        .highlight_style(Style::default().bold().fg(Color::Cyan))
        .divider(" | ")
        .block(Block::bordered().title(" cmux-controller "));
    frame.render_widget(tabs, area);
}

fn render_content(app: &App, frame: &mut Frame, area: ratatui::layout::Rect) {
    let content = Paragraph::new(format!("Tab: {}", app.current_tab))
        .block(Block::bordered());
    frame.render_widget(content, area);
}

fn render_status_bar(frame: &mut Frame, area: ratatui::layout::Rect) {
    let status = Paragraph::new(" tab:switch  enter:select  space:menu  q:quit")
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(status, area);
}
```

**Step 4: Slim main.rs to entry point**

```rust
mod app;
mod event;
mod ui;

use std::time::Duration;
use color_eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let mut terminal = ratatui::init();
    let result = run(&mut terminal).await;
    ratatui::restore();
    result
}

async fn run(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let mut app = app::App::default();
    let mut events = event::EventHandler::new(Duration::from_millis(250));

    loop {
        terminal.draw(|f| ui::render(&app, f))?;

        if let Some(evt) = events.next().await {
            match evt {
                event::Event::Key(k) => app.handle_key(k),
                event::Event::Tick => app.tick(),
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
```

**Step 5: Build and test**

Run: `cargo run`
Expected: Identical behavior to before — tabs, navigation, quit.

**Step 6: Commit**

```bash
git add src/
git commit -m "refactor: split into app/event/ui modules"
```

---

### Task 4: Add data layer — worktree fetching

**Files:**
- Create: `src/data.rs`
- Modify: `src/app.rs`

**Step 1: Create data types and fetcher for worktrees**

Create `src/data.rs`:

```rust
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Worktree {
    pub branch: String,
    pub path: PathBuf,
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    pub is_current: bool,
    pub main_state: Option<String>,
    pub main: Option<AheadBehind>,
    pub remote: Option<RemoteStatus>,
    pub working_tree: Option<WorkingTree>,
    pub commit: Option<CommitInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AheadBehind {
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteStatus {
    pub name: Option<String>,
    pub branch: Option<String>,
    pub ahead: i64,
    pub behind: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkingTree {
    #[serde(default)]
    pub staged: bool,
    #[serde(default)]
    pub modified: bool,
    #[serde(default)]
    pub untracked: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitInfo {
    pub short_sha: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubPr {
    pub number: i64,
    pub title: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    pub state: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GithubIssue {
    pub number: i64,
    pub title: String,
    pub labels: Vec<Label>,
    #[serde(rename = "updatedAt")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Debug, Default, Clone)]
pub struct DataStore {
    pub worktrees: Vec<Worktree>,
    pub prs: Vec<GithubPr>,
    pub issues: Vec<GithubIssue>,
    pub cmux_workspaces: Vec<String>,
    pub loading: bool,
}

impl DataStore {
    pub async fn refresh(&mut self, repo_root: &PathBuf) {
        self.loading = true;
        let (wt, prs, issues, ws) = tokio::join!(
            fetch_worktrees(repo_root),
            fetch_prs(repo_root),
            fetch_issues(repo_root),
            fetch_cmux_workspaces(),
        );
        self.worktrees = wt.unwrap_or_default();
        self.prs = prs.unwrap_or_default();
        self.issues = issues.unwrap_or_default();
        self.cmux_workspaces = ws.unwrap_or_default();
        self.loading = false;
    }
}

async fn run_command(cmd: &str, args: &[&str], cwd: Option<&PathBuf>) -> Result<String, String> {
    let mut command = tokio::process::Command::new(cmd);
    command.args(args);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let output = command.output().await.map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

async fn fetch_worktrees(repo_root: &PathBuf) -> Result<Vec<Worktree>, String> {
    let output = run_command("wt", &["list", "--format=json"], Some(repo_root)).await?;
    serde_json::from_str(&output).map_err(|e| e.to_string())
}

async fn fetch_prs(repo_root: &PathBuf) -> Result<Vec<GithubPr>, String> {
    let output = run_command(
        "gh",
        &["pr", "list", "--json", "number,title,headRefName,state,updatedAt", "--limit", "20"],
        Some(repo_root),
    ).await?;
    serde_json::from_str(&output).map_err(|e| e.to_string())
}

async fn fetch_issues(repo_root: &PathBuf) -> Result<Vec<GithubIssue>, String> {
    let output = run_command(
        "gh",
        &["issue", "list", "--json", "number,title,labels,updatedAt", "--limit", "20", "--state", "open"],
        Some(repo_root),
    ).await?;
    serde_json::from_str(&output).map_err(|e| e.to_string())
}

async fn fetch_cmux_workspaces() -> Result<Vec<String>, String> {
    let output = run_command(
        "/Applications/cmux.app/Contents/Resources/bin/cmux",
        &["list-workspaces"],
        None,
    ).await?;
    // Parse text format: "* workspace:14  scratch  [selected]"
    Ok(output
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim().trim_start_matches('*').trim();
            // Skip the workspace:N ref, get the name
            let parts: Vec<&str> = trimmed.splitn(2, "  ").collect();
            parts.get(1).map(|s| s.trim().trim_end_matches("[selected]").trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .collect())
}
```

**Step 2: Integrate DataStore into App**

Update `src/app.rs` to include data:

```rust
use crate::data::DataStore;
use std::path::PathBuf;

#[derive(Default)]
pub struct App {
    pub should_quit: bool,
    pub current_tab: Tab,
    pub data: DataStore,
    pub repo_root: PathBuf,
    pub list_index: usize,
}

impl App {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            ..Default::default()
        }
    }

    pub async fn refresh_data(&mut self) {
        self.data.refresh(&self.repo_root).await;
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab => {
                self.current_tab = self.current_tab.next();
                self.list_index = 0;
            }
            KeyCode::BackTab => {
                self.current_tab = self.current_tab.prev();
                self.list_index = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Char('r') => {} // refresh handled in main loop
            _ => {}
        }
    }

    fn select_next(&mut self) {
        let len = self.current_list_len();
        if len > 0 {
            self.list_index = (self.list_index + 1).min(len - 1);
        }
    }

    fn select_prev(&mut self) {
        self.list_index = self.list_index.saturating_sub(1);
    }

    fn current_list_len(&self) -> usize {
        match self.current_tab {
            Tab::Worktrees => self.data.worktrees.len(),
            Tab::Prs => self.data.prs.len(),
            Tab::Issues => self.data.issues.len(),
            Tab::Sessions => 0,
        }
    }
}
```

**Step 3: Update main.rs to detect repo root and trigger initial refresh**

```rust
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

    let mut refresh_interval = tokio::time::interval(Duration::from_secs(5));

    loop {
        terminal.draw(|f| ui::render(&app, f))?;

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

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
```

**Step 4: Build and test**

Run: `cargo run` (from a git repo like ~/dev/worktrunk)
Expected: Compiles. TUI shows tabs. Data loaded in background (not visible yet — next task renders it). Press r to refresh. q to quit.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat: add async data layer for worktrees, PRs, issues, cmux workspaces"
```

---

### Task 5: Render worktree list with status columns

**Files:**
- Modify: `src/ui.rs`
- Modify: `src/app.rs` (add `ListState`)

**Step 1: Update App to use ratatui ListState**

In `src/app.rs`, replace `list_index: usize` with:

```rust
use ratatui::widgets::ListState;

pub struct App {
    pub should_quit: bool,
    pub current_tab: Tab,
    pub data: DataStore,
    pub repo_root: PathBuf,
    pub list_state: ListState,
}
```

Update `select_next`/`select_prev` to use `list_state.select_next()` / `list_state.select_previous()`. Set initial selection in `refresh_data`:

```rust
pub async fn refresh_data(&mut self) {
    self.data.refresh(&self.repo_root).await;
    if self.list_state.selected().is_none() && self.current_list_len() > 0 {
        self.list_state.select(Some(0));
    }
}
```

**Step 2: Render worktree list and preview in ui.rs**

Update `render_content` to split into list + preview:

```rust
use ratatui::widgets::{List, ListItem, HighlightSpacing};
use ratatui::text::{Line, Span};

fn render_content(app: &mut App, frame: &mut Frame, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    match app.current_tab {
        Tab::Worktrees => render_worktree_list(app, frame, chunks[0]),
        Tab::Prs => render_pr_list(app, frame, chunks[0]),
        Tab::Issues => render_issue_list(app, frame, chunks[0]),
        Tab::Sessions => render_sessions(app, frame, chunks[0]),
    }

    render_preview(app, frame, chunks[1]);
}

fn render_worktree_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app
        .data
        .worktrees
        .iter()
        .map(|wt| {
            let indicator = if app.data.cmux_workspaces.iter().any(|ws| {
                wt.path.to_string_lossy().contains(ws) || ws.contains(&wt.branch)
            }) {
                "●"
            } else {
                "○"
            };

            let ahead = wt.main.as_ref().map(|m| format!("↑{}", m.ahead)).unwrap_or_default();
            let branch = &wt.branch;
            let modified = if wt.working_tree.as_ref().is_some_and(|w| w.modified) { "*" } else { "" };

            ListItem::new(Line::from(vec![
                Span::styled(format!(" {indicator} "), Style::default().fg(Color::Green)),
                Span::styled(format!("{branch}{modified:<20}"), Style::default().bold()),
                Span::styled(format!(" {ahead:<6}"), Style::default().fg(Color::Yellow)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::bordered().title(" Worktrees "))
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always);

    frame.render_stateful_widget(list, area, &mut app.list_state);
}
```

Add placeholder renderers for the other tabs and preview:

```rust
fn render_pr_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app.data.prs.iter().map(|pr| {
        ListItem::new(format!("  PR #{:<5} {}", pr.number, pr.title))
    }).collect();
    let list = List::new(items)
        .block(Block::bordered().title(" Pull Requests "))
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_issue_list(app: &mut App, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = app.data.issues.iter().map(|issue| {
        let labels = issue.labels.iter().map(|l| &l.name).cloned().collect::<Vec<_>>().join(",");
        ListItem::new(format!("  #{:<5} {} {}", issue.number, issue.title, labels))
    }).collect();
    let list = List::new(items)
        .block(Block::bordered().title(" Issues "))
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ");
    frame.render_stateful_widget(list, area, &mut app.list_state);
}

fn render_sessions(_app: &mut App, frame: &mut Frame, area: Rect) {
    let content = Paragraph::new("  Web sessions not yet connected.\n  (Waiting for claude.ai/code API)")
        .block(Block::bordered().title(" Sessions "))
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(content, area);
}

fn render_preview(app: &App, frame: &mut Frame, area: Rect) {
    let text = match app.current_tab {
        Tab::Worktrees => {
            if let Some(i) = app.list_state.selected() {
                if let Some(wt) = app.data.worktrees.get(i) {
                    let sha = wt.commit.as_ref().and_then(|c| c.short_sha.as_deref()).unwrap_or("?");
                    let msg = wt.commit.as_ref().and_then(|c| c.message.as_deref()).unwrap_or("");
                    format!("Branch: {}\nPath: {}\nCommit: {} {}", wt.branch, wt.path.display(), sha, msg)
                } else { String::new() }
            } else { String::new() }
        }
        Tab::Prs => {
            if let Some(i) = app.list_state.selected() {
                if let Some(pr) = app.data.prs.get(i) {
                    format!("PR #{}: {}\nBranch: {}\nState: {}", pr.number, pr.title, pr.head_ref_name, pr.state)
                } else { String::new() }
            } else { String::new() }
        }
        Tab::Issues => {
            if let Some(i) = app.list_state.selected() {
                if let Some(issue) = app.data.issues.get(i) {
                    format!("#{}: {}", issue.number, issue.title)
                } else { String::new() }
            } else { String::new() }
        }
        Tab::Sessions => "Not connected".to_string(),
    };

    let preview = Paragraph::new(text)
        .block(Block::bordered().title(" Preview "))
        .wrap(ratatui::widgets::Wrap { trim: true });
    frame.render_widget(preview, area);
}
```

**Note:** `render` signature changes — `app` must be `&mut App` now for `render_stateful_widget`. Update `ui::render` and the `terminal.draw` call accordingly.

**Step 3: Build and test**

Run from a repo with worktrees: `cd ~/dev/reticulate && cargo run --manifest-path ~/dev/scratch/Cargo.toml`
Expected: Worktrees tab shows list with branch names, ●/○ indicators, ahead counts. Arrow keys navigate. Preview pane shows details for selected worktree. PRs and Issues tabs show their data.

**Step 4: Commit**

```bash
git add src/
git commit -m "feat: render worktree/PR/issue lists with preview pane"
```

---

### Task 6: Add action dispatch — Enter to switch/create workspace

**Files:**
- Create: `src/actions.rs`
- Modify: `src/app.rs`
- Modify: `src/main.rs`

**Step 1: Create actions module**

```rust
use std::path::PathBuf;
use tokio::process::Command;

pub async fn switch_to_worktree(worktree_path: &PathBuf) -> Result<(), String> {
    // Focus the cmux workspace if it exists, or just report the path
    // For now, just print info — cmux workspace creation comes in Task 7
    Ok(())
}

pub async fn create_worktree(branch: &str, repo_root: &PathBuf) -> Result<PathBuf, String> {
    let output = Command::new("wt")
        .args(["switch", "--create", branch, "--no-cd"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).to_string());
    }

    // Get worktree path
    let list_output = Command::new("wt")
        .args(["list", "--format=json"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    let worktrees: Vec<serde_json::Value> =
        serde_json::from_slice(&list_output.stdout).map_err(|e| e.to_string())?;

    for wt in &worktrees {
        if let Some(b) = wt.get("branch").and_then(|v| v.as_str()) {
            if b.ends_with(branch) || b == branch {
                if let Some(p) = wt.get("path").and_then(|v| v.as_str()) {
                    return Ok(PathBuf::from(p));
                }
            }
        }
    }

    Err("Could not find worktree path after creation".to_string())
}

pub async fn remove_worktree(branch: &str, repo_root: &PathBuf) -> Result<(), String> {
    let output = Command::new("wt")
        .args(["remove", branch])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}

pub async fn open_pr_in_browser(pr_number: i64, repo_root: &PathBuf) -> Result<(), String> {
    let output = Command::new("gh")
        .args(["pr", "view", &pr_number.to_string(), "--web"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| e.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).to_string())
    }
}
```

**Step 2: Add action handling to App**

Add an `Action` enum and pending action queue:

```rust
pub enum PendingAction {
    None,
    SwitchWorktree(usize),
    CreateWorktree(String),
    RemoveWorktree(usize),
    OpenPr(i64),
    Refresh,
}
```

Wire Enter/d/p keybindings in `handle_key`:

```rust
KeyCode::Enter => self.pending_action = PendingAction::SwitchWorktree(self.list_state.selected().unwrap_or(0)),
KeyCode::Char('d') if self.current_tab == Tab::Worktrees => {
    if let Some(i) = self.list_state.selected() {
        self.pending_action = PendingAction::RemoveWorktree(i);
    }
}
KeyCode::Char('p') => { /* open PR */ }
```

**Step 3: Process pending actions in main loop**

In `run()`, after handling events, check for pending actions and execute them asynchronously.

**Step 4: Build and test**

Run from a repo. Navigate to a worktree. Press Enter — action fires. Press d — remove prompt (or direct remove for now). Press r — refresh.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat: add action dispatch — switch, create, remove worktree"
```

---

### Task 7: Add cmux workspace creation from template

**Files:**
- Create: `src/template.rs`
- Modify: `src/actions.rs`

**Step 1: Create template loader**

```rust
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceTemplate {
    pub panes: Vec<PaneTemplate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneTemplate {
    pub name: String,
    #[serde(default)]
    pub split: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub surfaces: Vec<SurfaceTemplate>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SurfaceTemplate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub command: String,
}

impl WorkspaceTemplate {
    pub fn load(repo_root: &PathBuf) -> Self {
        let path = repo_root.join(".cmux/workspace.yaml");
        if path.exists() {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            serde_yaml::from_str(&content).unwrap_or_else(|_| Self::default_template())
        } else {
            Self::default_template()
        }
    }

    fn default_template() -> Self {
        Self {
            panes: vec![PaneTemplate {
                name: "main".to_string(),
                split: None,
                parent: None,
                surfaces: vec![SurfaceTemplate {
                    name: None,
                    command: "{main_command}".to_string(),
                }],
            }],
        }
    }

    pub fn render(&self, vars: &std::collections::HashMap<String, String>) -> Self {
        let mut rendered = self.clone();
        for pane in &mut rendered.panes {
            for surface in &mut pane.surfaces {
                for (key, value) in vars {
                    surface.command = surface.command.replace(&format!("{{{key}}}"), value);
                }
            }
        }
        rendered
    }
}
```

**Step 2: Add workspace creation to actions.rs**

```rust
use crate::template::WorkspaceTemplate;
use std::collections::HashMap;

const CMUX_BIN: &str = "/Applications/cmux.app/Contents/Resources/bin/cmux";

async fn cmux_cmd(args: &[&str]) -> Result<String, String> {
    let output = Command::new(CMUX_BIN)
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub async fn create_cmux_workspace(
    template: &WorkspaceTemplate,
    worktree_path: &PathBuf,
    main_command: &str,
) -> Result<(), String> {
    let mut vars = HashMap::new();
    vars.insert("main_command".to_string(), main_command.to_string());
    let rendered = template.render(&vars);

    // Create workspace
    cmux_cmd(&["new-workspace"]).await?;

    let mut pane_refs: HashMap<String, String> = HashMap::new();

    for (i, pane) in rendered.panes.iter().enumerate() {
        let pane_ref = if i == 0 {
            // First pane exists already — just use it
            "pane:1".to_string()
        } else {
            let direction = pane.split.as_deref().unwrap_or("right");
            let mut args = vec!["new-split", direction];
            if let Some(parent) = &pane.parent {
                if let Some(parent_ref) = pane_refs.get(parent) {
                    args.extend(["--panel", parent_ref]);
                }
            }
            cmux_cmd(&args).await?;
            format!("pane:{}", i + 1)
        };
        pane_refs.insert(pane.name.clone(), pane_ref.clone());

        for (j, surface) in pane.surfaces.iter().enumerate() {
            if !(i == 0 && j == 0) {
                cmux_cmd(&["new-surface", "--type", "terminal", "--pane", &pane_ref]).await?;
            }
            let cmd = if surface.command.is_empty() {
                format!("cd {}", worktree_path.display())
            } else {
                format!("cd {} && {}", worktree_path.display(), surface.command)
            };
            cmux_cmd(&["send", &format!("{cmd}\n")]).await?;
        }
    }

    Ok(())
}
```

**Step 3: Wire into the switch action**

When Enter is pressed on a worktree that doesn't have a cmux workspace, call `create_cmux_workspace`. When it does have one, focus it with `cmux select-workspace`.

**Step 4: Build and test**

Create `.cmux/workspace.yaml` in a test repo. Run the TUI. Select a worktree. Press Enter. Verify cmux workspace is created with the right panes.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat: add cmux workspace creation from template"
```

---

### Task 8: Add action menu popup (Space key)

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui.rs`

**Step 1: Add popup state to App**

```rust
pub struct App {
    // ... existing fields
    pub show_action_menu: bool,
    pub action_menu_items: Vec<String>,
    pub action_menu_index: usize,
}
```

**Step 2: Generate context-sensitive action menu**

When Space is pressed, populate `action_menu_items` based on current tab and selected item:

- Worktree with cmux workspace: ["Switch", "Remove", "View diff", "Open PR", "Close workspace"]
- Worktree without workspace: ["Create workspace", "Remove", "View diff"]
- PR: ["Checkout & create workspace", "View in browser"]
- Issue: ["Create branch & workspace", "View in browser"]

**Step 3: Render popup overlay in ui.rs**

```rust
use ratatui::widgets::Clear;

fn render_action_menu(app: &mut App, frame: &mut Frame) {
    if !app.show_action_menu { return; }

    let area = popup_area(frame.area(), 40, 40);
    frame.render_widget(Clear, area);

    let items: Vec<ListItem> = app.action_menu_items.iter().enumerate().map(|(i, item)| {
        ListItem::new(format!(" {}: {}", i + 1, item))
    }).collect();

    let list = List::new(items)
        .block(Block::bordered().title(" Actions "))
        .highlight_style(Style::default().bg(Color::Blue).bold())
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    state.select(Some(app.action_menu_index));
    frame.render_stateful_widget(list, area, &mut state);
}

fn popup_area(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    use ratatui::layout::Flex;
    let [area] = Layout::vertical([Constraint::Percentage(percent_y)])
        .flex(Flex::Center)
        .areas(area);
    let [area] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(area);
    area
}
```

**Step 4: Handle keybindings when popup is open**

When `show_action_menu` is true, keybindings switch to: j/k navigate, Enter selects, Esc closes.

**Step 5: Build and test**

Run. Select a worktree. Press Space. Verify popup appears with correct actions. Navigate and select. Esc closes.

**Step 6: Commit**

```bash
git add src/
git commit -m "feat: add context-sensitive action menu popup"
```

---

### Task 9: Add text input for new branch creation

**Files:**
- Modify: `src/app.rs`
- Modify: `src/ui.rs`

**Step 1: Add input mode to App**

```rust
use tui_input::Input;

pub enum InputMode {
    Normal,
    BranchName,
}

pub struct App {
    // ... existing
    pub input_mode: InputMode,
    pub input: Input,
}
```

**Step 2: Handle input mode keybindings**

When `n` is pressed in Worktrees tab, switch to `InputMode::BranchName`. In input mode, feed key events to `tui_input::Input`. On Enter, create the worktree with the input value. On Esc, cancel.

**Step 3: Render input popup**

Show a centered popup with the text input field and cursor.

**Step 4: Build and test**

Press `n`. Type a branch name. Press Enter. Verify worktree is created.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat: add text input for new branch creation"
```

---

### Task 10: Add help overlay and polish

**Files:**
- Modify: `src/ui.rs`
- Modify: `src/app.rs`

**Step 1: Add help overlay**

When `?` is pressed, show a popup listing all keybindings.

**Step 2: Polish status bar**

Show context-sensitive keybindings in the status bar (changes based on tab and whether popup is open).

**Step 3: Add loading indicator**

Show a spinner or "loading..." text when data is being refreshed.

**Step 4: Build and test**

Verify help shows all keybindings. Status bar updates per context.

**Step 5: Commit**

```bash
git add src/
git commit -m "feat: add help overlay, loading indicator, status bar polish"
```

---

## Summary

| Task | What | Depends on |
|------|------|------------|
| 1 | Rust scaffold + async event loop | Nothing |
| 2 | Tab bar and app state | Task 1 |
| 3 | Module split (app/event/ui) | Task 2 |
| 4 | Data layer (wt/gh/cmux fetching) | Task 3 |
| 5 | Worktree/PR/issue list rendering | Task 4 |
| 6 | Action dispatch (switch/create/remove) | Task 5 |
| 7 | cmux workspace creation from template | Task 6 |
| 8 | Action menu popup | Task 6 |
| 9 | Text input for new branch | Task 8 |
| 10 | Help overlay and polish | Task 9 |

All tasks are sequential — each builds on the previous. After Task 5, the TUI is usable for viewing. After Task 7, it's fully functional for creating workspaces.
