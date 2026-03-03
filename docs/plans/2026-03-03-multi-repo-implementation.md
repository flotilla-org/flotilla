# Multi-Repo Support Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add tabbed multi-repo support with config persistence, parallel refresh, change indicators, and a file picker.

**Architecture:** HashMap<PathBuf, RepoState> keyed by repo path, with a Vec<PathBuf> for tab ordering. New `config.rs` module handles persistence. Title bar becomes a tab bar. All repos refresh in parallel; inactive tabs show a change indicator.

**Tech Stack:** Rust, ratatui, tokio, serde/toml for config, std::fs for file picker directory listing.

---

### Task 1: Create `config.rs` — repo persistence

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs:1` (add `mod config;`)

**Context:** This module manages `~/.config/cmux-controller/repos/` — one TOML file per repo. Pure logic, no UI dependencies.

**Step 1: Create `src/config.rs` with the full module**

```rust
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct RepoConfig {
    path: String,
}

fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".config/cmux-controller/repos")
}

/// Convert "/Users/robert/dev/scratch" → "users-robert-dev-scratch"
pub fn path_to_slug(path: &Path) -> String {
    path.to_string_lossy()
        .to_lowercase()
        .replace('/', "-")
        .trim_start_matches('-')
        .to_string()
}

/// Load all persisted repo paths from config dir, sorted alphabetically by slug.
pub fn load_repos() -> Vec<PathBuf> {
    let dir = config_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut repos: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|e| {
            let content = std::fs::read_to_string(e.path()).ok()?;
            let config: RepoConfig = toml::from_str(&content).ok()?;
            let path = PathBuf::from(&config.path);
            if path.is_dir() {
                Some((e.file_name().to_string_lossy().to_string(), path))
            } else {
                None // skip repos whose directories no longer exist
            }
        })
        .collect();
    repos.sort_by(|a, b| a.0.cmp(&b.0));
    repos.into_iter().map(|(_, path)| path).collect()
}

/// Persist a repo path to config. No-op if already persisted.
pub fn save_repo(path: &Path) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let slug = path_to_slug(path);
    let file = dir.join(format!("{slug}.toml"));
    if file.exists() {
        return;
    }
    let config = RepoConfig {
        path: path.to_string_lossy().to_string(),
    };
    if let Ok(content) = toml::to_string(&config) {
        let _ = std::fs::write(file, content);
    }
}

/// Remove a repo's config file.
pub fn remove_repo(path: &Path) {
    let dir = config_dir();
    let slug = path_to_slug(path);
    let file = dir.join(format!("{slug}.toml"));
    let _ = std::fs::remove_file(file);
}
```

**Step 2: Add `dirs` and `toml` dependencies to `Cargo.toml`**

Add to `[dependencies]`:
```toml
dirs = "6"
toml = "0.8"
```

**Step 3: Add `mod config;` to `src/main.rs`**

Add after line 6 (`mod ui;`):
```rust
mod config;
```

**Step 4: Build and verify**

Run: `cargo build`
Expected: clean build, no warnings about unused code (functions will be used in Task 5)

**Step 5: Commit**

```
git add src/config.rs Cargo.toml Cargo.lock src/main.rs
git commit -m "feat: add config module for repo persistence"
```

---

### Task 2: Restructure `app.rs` — RepoState and accessors

**Files:**
- Modify: `src/app.rs`

**Context:** Extract per-repo fields from `App` into `RepoState`. Add `HashMap<PathBuf, RepoState>` + `Vec<PathBuf>` for ordering. Add accessor methods. This task does NOT update the method bodies yet — that's Task 3.

**Step 1: Add imports and `RepoState` struct**

At `src/app.rs:8`, add `HashMap` import — change:
```rust
use std::collections::BTreeSet;
```
to:
```rust
use std::collections::{BTreeSet, HashMap};
```

After `InputMode` enum (after line 17), add:
```rust
pub struct RepoState {
    pub repo_root: PathBuf,
    pub data: DataStore,
    pub table_state: TableState,
    pub selected_selectable_idx: Option<usize>,
    pub has_unseen_changes: bool,
    pub multi_selected: BTreeSet<usize>,
}

impl RepoState {
    pub fn new(repo_root: PathBuf) -> Self {
        Self {
            repo_root,
            data: DataStore::default(),
            table_state: TableState::default(),
            selected_selectable_idx: None,
            has_unseen_changes: false,
            multi_selected: BTreeSet::new(),
        }
    }

    /// Snapshot for change detection: (worktrees, prs, sessions, branches, issues)
    pub fn data_snapshot(&self) -> (usize, usize, usize, usize, usize) {
        (
            self.data.worktrees.len(),
            self.data.prs.len(),
            self.data.sessions.len(),
            self.data.remote_branches.len(),
            self.data.issues.len(),
        )
    }
}
```

**Step 2: Replace single-repo fields in `App` struct**

Replace lines 178-208 (the `App` struct) with:

```rust
#[derive(Default)]
pub struct App {
    pub should_quit: bool,
    pub repos: HashMap<PathBuf, RepoState>,
    pub repo_order: Vec<PathBuf>,
    pub active_repo: usize,
    pub pending_action: PendingAction,
    pub show_action_menu: bool,
    pub action_menu_items: Vec<Action>,
    pub action_menu_index: usize,
    pub input_mode: InputMode,
    pub input: Input,
    pub show_help: bool,
    pub table_area: Rect,
    // Delete confirmation
    pub show_delete_confirm: bool,
    pub delete_confirm_info: Option<DeleteConfirmInfo>,
    pub delete_confirm_loading: bool,
    // Popup area for mouse hit-testing (set by UI render)
    pub menu_area: Rect,
    // Tab bar areas for mouse hit-testing (set by UI render)
    pub tab_areas: Vec<Rect>,
    pub add_tab_area: Rect,
    // Double-click detection
    last_click_time: Option<Instant>,
    last_click_selectable_idx: Option<usize>,
    // Branch generation loading
    pub generating_branch: bool,
    // Transient status/error message (cleared on next action)
    pub status_message: Option<String>,
}
```

**Step 3: Add accessor methods and update constructor**

Replace `App::new` (lines 210-216) with:

```rust
impl App {
    pub fn new(repos: Vec<PathBuf>) -> Self {
        let mut map = HashMap::new();
        let mut order = Vec::new();
        for path in repos {
            if !map.contains_key(&path) {
                map.insert(path.clone(), RepoState::new(path.clone()));
                order.push(path);
            }
        }
        Self {
            repos: map,
            repo_order: order,
            ..Default::default()
        }
    }

    /// Reference to the active repo state.
    pub fn active(&self) -> &RepoState {
        &self.repos[&self.repo_order[self.active_repo]]
    }

    /// Mutable reference to the active repo state.
    pub fn active_mut(&mut self) -> &mut RepoState {
        let key = &self.repo_order[self.active_repo];
        self.repos.get_mut(key).unwrap()
    }

    /// Path of the active repo.
    pub fn active_repo_root(&self) -> &PathBuf {
        &self.repo_order[self.active_repo]
    }

    /// Repo display name (directory basename).
    pub fn repo_name(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string())
    }

    pub fn add_repo(&mut self, path: PathBuf) {
        if !self.repos.contains_key(&path) {
            self.repos.insert(path.clone(), RepoState::new(path.clone()));
            self.repo_order.push(path);
        }
    }

    pub fn switch_tab(&mut self, idx: usize) {
        if idx < self.repo_order.len() {
            self.active_repo = idx;
            let key = &self.repo_order[idx];
            self.repos.get_mut(key).unwrap().has_unseen_changes = false;
        }
    }

    pub fn next_tab(&mut self) {
        if !self.repo_order.is_empty() {
            self.switch_tab((self.active_repo + 1) % self.repo_order.len());
        }
    }

    pub fn prev_tab(&mut self) {
        if !self.repo_order.is_empty() {
            self.switch_tab(
                self.active_repo
                    .checked_sub(1)
                    .unwrap_or(self.repo_order.len() - 1),
            );
        }
    }
```

Add `use std::path::Path;` to the imports at the top if not already present.

**Step 4: Build — expect errors**

Run: `cargo build 2>&1 | head -40`

Expected: many errors from methods still referencing `self.data`, `self.repo_root`, `self.table_state`, `self.selected_selectable_idx`, `self.multi_selected`. This confirms the struct change landed and Task 3 is needed.

**Step 5: Commit (WIP)**

```
git add src/app.rs
git commit -m "wip: restructure App with RepoState (breaks build)"
```

---

### Task 3: Migrate `app.rs` methods to use accessors

**Files:**
- Modify: `src/app.rs`

**Context:** Every method that previously accessed `self.data`, `self.table_state`, `self.selected_selectable_idx`, `self.repo_root`, or `self.multi_selected` must now go through `self.active()` / `self.active_mut()`. This is a mechanical find-and-replace with some borrow-checker care.

**Step 1: Update `refresh_data`**

Replace the current `refresh_data` method with:

```rust
    pub async fn refresh_data(&mut self) -> Vec<String> {
        let rs = self.active_mut();
        let errors = rs.data.refresh(&rs.repo_root).await;
        // Restore selection or pick first
        if rs.data.selectable_indices.is_empty() {
            rs.selected_selectable_idx = None;
            rs.table_state.select(None);
        } else if rs.selected_selectable_idx.is_none() {
            rs.selected_selectable_idx = Some(0);
            rs.table_state.select(Some(rs.data.selectable_indices[0]));
        } else if let Some(si) = rs.selected_selectable_idx {
            let clamped = si.min(rs.data.selectable_indices.len() - 1);
            rs.selected_selectable_idx = Some(clamped);
            rs.table_state.select(Some(rs.data.selectable_indices[clamped]));
        }
        errors
    }
```

**Step 2: Update `selected_work_item`**

```rust
    pub fn selected_work_item(&self) -> Option<&WorkItem> {
        let rs = self.active();
        let table_idx = rs.table_state.selected()?;
        match rs.data.table_entries.get(table_idx)? {
            TableEntry::Item(item) => Some(item),
            TableEntry::Header(_) => None,
        }
    }
```

**Step 3: Update `handle_key` — add `[`/`]`/`a` bindings, fix `self.multi_selected` references**

In the `match key.code` block, add these arms before the `_ => {}` catch-all:

```rust
            KeyCode::Char('[') => self.prev_tab(),
            KeyCode::Char(']') => self.next_tab(),
            KeyCode::Char('a') => {
                self.input_mode = InputMode::AddRepo;
                self.input.reset();
                // Pre-fill with parent of active repo
                if let Some(parent) = self.active_repo_root().parent() {
                    let parent_str = format!("{}/", parent.display());
                    self.input = Input::from(parent_str.as_str());
                }
                self.dir_entries = Vec::new();
                self.dir_selected = 0;
                self.refresh_dir_listing();
            }
```

Change references from `self.multi_selected` to `self.active().multi_selected` / `self.active_mut().multi_selected`. Specifically:

- `KeyCode::Esc` arm: `self.multi_selected` → `self.active().multi_selected` (for `.is_empty()` check) and `self.active_mut().multi_selected.clear()`
- `KeyCode::Char(' ')` check in status bar hint: same pattern

**Step 4: Update `handle_mouse` — fix data/table_state/multi_selected references**

Replace all `self.data.selectable_indices` with `self.active().data.selectable_indices`.
Replace all `self.table_state` with `self.active_mut().table_state`.
Replace all `self.selected_selectable_idx` (set) with `self.active_mut().selected_selectable_idx`.

For `row_at_mouse`, `toggle_multi_select`, `select_next`, `select_prev` — same pattern.

**Step 5: Update `Action::dispatch` — fix `app.data` references**

In `Action::dispatch` (lines 86-150), every `app.data.prs`, `app.data.issues`, `app.data.sessions`, `app.data.worktrees` must become `app.active().data.prs` etc. Also change `app.selected_selectable_idx` to `app.active().selected_selectable_idx`.

**Step 6: Update `action_enter_multi_select` — fix data/multi_selected references**

Replace `self.data.selectable_indices` → `self.active().data.selectable_indices`
Replace `self.data.table_entries` → `self.active().data.table_entries`
Replace `self.multi_selected` → `self.active().multi_selected` / `self.active_mut().multi_selected`
Replace `self.selected_selectable_idx` → `self.active().selected_selectable_idx`

**Step 7: Add `InputMode::AddRepo` variant**

In the `InputMode` enum, add:
```rust
    AddRepo,
```

Add file picker state fields to `App`:
```rust
    // File picker state
    pub dir_entries: Vec<DirEntry>,
    pub dir_selected: usize,
```

Add `DirEntry` struct (before `App`):
```rust
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_git_repo: bool,
    pub is_added: bool,
}
```

Add file picker input handler and directory listing refresh (implementation in Task 8).

**Step 8: Build and verify**

Run: `cargo build`
Expected: may still have errors from `ui.rs` and `main.rs` — those are Tasks 4-6.

**Step 9: Commit**

```
git add src/app.rs
git commit -m "feat: migrate app.rs methods to RepoState accessors"
```

---

### Task 4: Update `ui.rs` — tab bar and content references

**Files:**
- Modify: `src/ui.rs`

**Context:** Replace `render_title_bar` with tab bar rendering. Update all references from `app.data`/`app.table_state`/`app.repo_root`/`app.multi_selected` to use `app.active()`/`app.active_mut()`.

**Step 1: Replace `render_title_bar` with `render_tab_bar`**

Replace lines 34-46 with:

```rust
fn render_tab_bar(app: &mut App, frame: &mut Frame, area: Rect) {
    let mut spans: Vec<Span> = vec![
        Span::styled(" cmux ", Style::default().bold().fg(Color::Cyan)),
    ];

    app.tab_areas.clear();
    let mut x_offset: u16 = 6; // length of " cmux "

    for (i, path) in app.repo_order.iter().enumerate() {
        let rs = &app.repos[path];
        let name = App::repo_name(path);
        let is_active = i == app.active_repo;
        let loading = if rs.data.loading { " ⟳" } else { "" };
        let changed = if rs.has_unseen_changes { "*" } else { "" };

        let sep = Span::styled(" | ", Style::default().fg(Color::DarkGray));
        spans.push(sep);
        x_offset += 3;

        let label = format!("{name}{changed}{loading}");
        let label_len = label.len() as u16;
        let style = if is_active {
            Style::default().bold().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        spans.push(Span::styled(label, style));

        // Record tab area for mouse hit-testing
        app.tab_areas.push(Rect::new(area.x + x_offset, area.y, label_len, 1));
        x_offset += label_len;
    }

    // [+] button
    let add_sep = Span::styled(" | ", Style::default().fg(Color::DarkGray));
    spans.push(add_sep);
    x_offset += 3;
    let add_label = Span::styled("[+]", Style::default().fg(Color::Green));
    spans.push(add_label);
    app.add_tab_area = Rect::new(area.x + x_offset, area.y, 3, 1);

    let line = Line::from(spans);
    let title = Paragraph::new(line);
    frame.render_widget(title, area);
}
```

Update the call in `render` (line 25):
```rust
    render_tab_bar(app, frame, chunks[0]);
```

**Step 2: Update `render_unified_table` — use active() accessor**

Replace all `app.data` with `app.active().data`. Replace `app.table_state` with direct access via `app.active_mut()`.

The key change: since `render_unified_table` takes `&mut App` (for `table_state`), we need to extract data first, then render:

```rust
fn render_unified_table(app: &mut App, frame: &mut Frame, area: Rect) {
    app.table_area = area;

    // ... header and widths unchanged ...

    let active = app.active();
    let rows: Vec<Row> = active
        .data
        .table_entries
        .iter()
        .enumerate()
        .map(|(table_idx, entry)| {
            let is_multi_selected = active
                .data
                .selectable_indices
                .iter()
                .position(|&idx| idx == table_idx)
                .map(|si| active.multi_selected.contains(&si))
                .unwrap_or(false);

            match entry {
                TableEntry::Header(header) => build_header_row(header),
                TableEntry::Item(item) => {
                    let mut row = build_item_row(item, &active.data, &col_widths);
                    if is_multi_selected {
                        row = row.style(Style::default().bg(Color::Indexed(236)));
                    }
                    row
                }
            }
        })
        .collect();

    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered())
        .row_highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ")
        .highlight_spacing(HighlightSpacing::Always);

    let key = &app.repo_order[app.active_repo];
    let rs = app.repos.get_mut(key).unwrap();
    frame.render_stateful_widget(table, area, &mut rs.table_state);
}
```

**Step 3: Update `render_preview` — use active() accessor**

Replace `app.selected_work_item()` call (unchanged — it already delegates).
Replace `app.data.worktrees` → `app.active().data.worktrees` etc.
Replace `app.data.prs` → `app.active().data.prs` etc.
Replace `app.data.cmux_workspaces` → `app.active().data.cmux_workspaces` etc.

**Step 4: Update `render_status_bar` — fix multi_selected reference**

Replace `app.multi_selected` → `app.active().multi_selected`.

**Step 5: Add `render_file_picker` call to `render`**

After `render_help(app, frame);` in the `render` function, add:
```rust
    render_file_picker(app, frame);
```

Add the function:
```rust
fn render_file_picker(app: &App, frame: &mut Frame) {
    if app.input_mode != crate::app::InputMode::AddRepo {
        return;
    }

    let area = popup_area(frame.area(), 60, 60);
    frame.render_widget(Clear, area);

    let block = Block::bordered().title(" Add Repository ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(inner);

    // Input line
    let input_text = app.input.value();
    let display = format!("> {}", input_text);
    let paragraph = Paragraph::new(display).style(Style::default().fg(Color::Cyan));
    frame.render_widget(paragraph, chunks[0]);

    // Cursor
    let cursor_x = chunks[0].x + 2 + app.input.visual_cursor() as u16;
    frame.set_cursor_position((cursor_x, chunks[0].y));

    // Directory listing
    let items: Vec<ListItem> = app
        .dir_entries
        .iter()
        .map(|entry| {
            let tag = if entry.is_added {
                " (added)"
            } else if entry.is_git_repo {
                " (git repo)"
            } else if entry.is_dir {
                "/"
            } else {
                ""
            };
            let style = if entry.is_git_repo && !entry.is_added {
                Style::default().fg(Color::Green)
            } else if entry.is_added {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(format!("  {}{}", entry.name, tag)).style(style)
        })
        .collect();

    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).bold())
        .highlight_symbol("▸ ");

    let mut state = ListState::default();
    if !app.dir_entries.is_empty() {
        state.select(Some(app.dir_selected));
    }
    frame.render_stateful_widget(list, chunks[1], &mut state);
}
```

**Step 6: Update help text**

In `render_help`, add tab navigation lines after the "General" section:
```rust
        Line::from(""),
        Line::from(Span::styled("Repos", Style::default().bold())),
        Line::from("  [ / ]            Switch repo tab"),
        Line::from("  a                Add repository"),
```

**Step 7: Build and verify**

Run: `cargo build`
Expected: may still error from `main.rs` — that's Task 5.

**Step 8: Commit**

```
git add src/ui.rs
git commit -m "feat: tab bar rendering and multi-repo UI"
```

---

### Task 5: Update `main.rs` — startup, refresh, and action handlers

**Files:**
- Modify: `src/main.rs`

**Context:** CLI takes `Vec<PathBuf>`, loads persisted repos, constructs App with all repos, refreshes all in parallel, and updates action handlers to use `app.active_repo_root()`.

**Step 1: Update CLI struct**

Replace lines 18-22:
```rust
struct Cli {
    /// Git repo roots (repeatable; auto-detected from cwd if omitted)
    #[arg(long)]
    repo_root: Vec<PathBuf>,
}
```

**Step 2: Update `main()` — multi-repo collection**

Replace lines 29-41 with:

```rust
    // Collect repos: CLI args first, then persisted, then auto-detect
    let mut repo_roots: Vec<PathBuf> = Vec::new();
    for root in &cli.repo_root {
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        repo_roots.push(canonical);
    }

    // Auto-detect from cwd if no CLI args
    if repo_roots.is_empty() {
        let output = std::process::Command::new("git")
            .args(["rev-parse", "--show-toplevel"])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
                repo_roots.push(path);
            }
        }
    }

    // Load persisted repos
    let persisted = config::load_repos();
    for path in persisted {
        if !repo_roots.contains(&path) {
            repo_roots.push(path);
        }
    }

    // Persist any new CLI repos
    for path in &repo_roots {
        config::save_repo(path);
    }

    if repo_roots.is_empty() {
        eprintln!("Error: no git repositories found (use --repo-root to specify)");
        std::process::exit(1);
    }
```

Update the `run` call (line 45):
```rust
    let result = run(&mut terminal, repo_roots).await;
```

**Step 3: Update `run()` signature and App construction**

Change `run` signature:
```rust
async fn run(terminal: &mut ratatui::DefaultTerminal, repo_roots: Vec<PathBuf>) -> Result<()> {
    let mut app = app::App::new(repo_roots);
```

**Step 4: Replace `refresh_data` calls with `refresh_all`**

Add a `refresh_all` helper function (inside `run`, or as a free function):

Replace the initial data load block:
```rust
    // Initial data load — all repos in parallel
    refresh_all(&mut app).await;
```

Replace the `'r'` key handler:
```rust
                        refresh_all(&mut app).await;
```

Replace the tick handler:
```rust
                    if last_refresh.elapsed() >= refresh_interval {
                        refresh_all(&mut app).await;
                        last_refresh = std::time::Instant::now();
                    }
```

Add the helper function after `run`:
```rust
async fn refresh_all(app: &mut app::App) {
    // Snapshot all repos for change detection
    let snapshots: Vec<_> = app.repo_order.iter()
        .map(|path| app.repos[path].data_snapshot())
        .collect();

    // Refresh all repos in parallel
    let mut data_stores: Vec<_> = app.repo_order.iter()
        .map(|path| {
            let mut ds = std::mem::take(&mut app.repos.get_mut(path).unwrap().data);
            let root = path.clone();
            async move {
                let errors = ds.refresh(&root).await;
                (root, ds, errors)
            }
        })
        .collect();

    let results = futures::future::join_all(data_stores).await;

    let mut all_errors: Vec<String> = Vec::new();
    for (i, (path, data, errors)) in results.into_iter().enumerate() {
        let rs = app.repos.get_mut(&path).unwrap();
        rs.data = data;

        // Change detection
        let new_snapshot = rs.data_snapshot();
        if snapshots[i] != new_snapshot && i != app.active_repo {
            rs.has_unseen_changes = true;
        }

        // Restore selection
        if rs.data.selectable_indices.is_empty() {
            rs.selected_selectable_idx = None;
            rs.table_state.select(None);
        } else if rs.selected_selectable_idx.is_none() {
            rs.selected_selectable_idx = Some(0);
            rs.table_state.select(Some(rs.data.selectable_indices[0]));
        } else if let Some(si) = rs.selected_selectable_idx {
            let clamped = si.min(rs.data.selectable_indices.len() - 1);
            rs.selected_selectable_idx = Some(clamped);
            rs.table_state.select(Some(rs.data.selectable_indices[clamped]));
        }

        // Collect errors with repo name prefix
        if !errors.is_empty() {
            let name = app::App::repo_name(&path);
            for e in errors {
                all_errors.push(format!("{name}: {e}"));
            }
        }
    }

    if !all_errors.is_empty() {
        app.status_message = Some(all_errors.join("; "));
    }
}
```

**Step 5: Update action handlers — replace `app.repo_root` and `app.data`**

Throughout the `match pending` block, replace:
- `app.repo_root` → `app.active_repo_root().clone()` (where `.clone()` is needed)
- `app.data.worktrees` → `app.active().data.worktrees`
- `app.data.prs` → `app.active().data.prs`
- `app.data.sessions` → `app.active().data.sessions`
- `app.data.issues` → `app.active().data.issues`
- `app.data.selectable_indices` → `app.active().data.selectable_indices`
- `app.data.table_entries` → `app.active().data.table_entries`
- `template::WorkspaceTemplate::load(&app.repo_root)` → `template::WorkspaceTemplate::load(app.active_repo_root())`

For `refresh_data().await` calls in action handlers, replace with `refresh_all(&mut app).await`.

**Step 6: Add tab click handling in mouse event**

In the mouse handler section, before delegating to `app.handle_mouse(m)`, add:

```rust
                event::Event::Mouse(m) => {
                    // Check for tab clicks
                    if m.kind == crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) {
                        let x = m.column;
                        let y = m.row;
                        // Check tab areas
                        for (i, tab_area) in app.tab_areas.iter().enumerate() {
                            if x >= tab_area.x && x < tab_area.x + tab_area.width
                                && y >= tab_area.y && y < tab_area.y + tab_area.height
                            {
                                app.switch_tab(i);
                                continue; // skip normal mouse handling
                            }
                        }
                        // Check [+] button
                        let a = app.add_tab_area;
                        if x >= a.x && x < a.x + a.width && y >= a.y && y < a.y + a.height {
                            // TODO: open file picker
                        }
                    }
                    app.handle_mouse(m);
                }
```

**Step 7: Add `PendingAction::AddRepo` variant and handler**

In `src/app.rs`, add to `PendingAction`:
```rust
    AddRepo(PathBuf),
```

In `main.rs`, add the handler in the `match pending` block:
```rust
            app::PendingAction::AddRepo(path) => {
                config::save_repo(&path);
                app.add_repo(path);
                app.switch_tab(app.repo_order.len() - 1);
                refresh_all(&mut app).await;
            }
```

**Step 8: Build and verify**

Run: `cargo build`
Expected: clean build

**Step 9: Commit**

```
git add src/main.rs src/app.rs
git commit -m "feat: multi-repo startup, parallel refresh, and action routing"
```

---

### Task 6: File picker input handling in `app.rs`

**Files:**
- Modify: `src/app.rs`

**Context:** Add the `AddRepo` input mode handler and directory listing logic. When `InputMode::AddRepo` is active, the user types a path; directory contents update live; j/k select entries; Tab completes; Enter on a git repo adds it or descends into a plain directory.

**Step 1: Add `handle_add_repo_key` method**

Add after `handle_input_key`:

```rust
    fn handle_add_repo_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
                self.input.reset();
                self.dir_entries.clear();
            }
            KeyCode::Down | KeyCode::Char('j') if key.modifiers.is_empty() || key.code == KeyCode::Down => {
                if !self.dir_entries.is_empty() {
                    self.dir_selected = (self.dir_selected + 1).min(self.dir_entries.len() - 1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') if key.modifiers.is_empty() || key.code == KeyCode::Up => {
                self.dir_selected = self.dir_selected.saturating_sub(1);
            }
            KeyCode::Tab => {
                // Complete selected entry into input
                if let Some(entry) = self.dir_entries.get(self.dir_selected) {
                    let current = self.input.value().to_string();
                    // Find the directory prefix
                    let base = if current.ends_with('/') {
                        current.clone()
                    } else {
                        // Go up to last /
                        current.rsplit_once('/')
                            .map(|(prefix, _)| format!("{prefix}/"))
                            .unwrap_or_default()
                    };
                    let new_path = format!("{}{}/", base, entry.name);
                    self.input = Input::from(new_path.as_str());
                    self.dir_selected = 0;
                    self.refresh_dir_listing();
                }
            }
            KeyCode::Enter => {
                if let Some(entry) = self.dir_entries.get(self.dir_selected).cloned() {
                    if entry.is_git_repo && !entry.is_added {
                        // Add this repo
                        let current = self.input.value().to_string();
                        let base = if current.ends_with('/') {
                            current
                        } else {
                            current.rsplit_once('/')
                                .map(|(prefix, _)| format!("{prefix}/"))
                                .unwrap_or_default()
                        };
                        let path = PathBuf::from(format!("{}{}", base, entry.name));
                        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                        self.pending_action = PendingAction::AddRepo(canonical);
                        self.input_mode = InputMode::Normal;
                        self.input.reset();
                        self.dir_entries.clear();
                    } else if entry.is_dir {
                        // Descend into directory
                        let current = self.input.value().to_string();
                        let base = if current.ends_with('/') {
                            current
                        } else {
                            current.rsplit_once('/')
                                .map(|(prefix, _)| format!("{prefix}/"))
                                .unwrap_or_default()
                        };
                        let new_path = format!("{}{}/", base, entry.name);
                        self.input = Input::from(new_path.as_str());
                        self.dir_selected = 0;
                        self.refresh_dir_listing();
                    }
                }
            }
            _ => {
                self.input.handle_event(&crossterm::event::Event::Key(key));
                self.dir_selected = 0;
                self.refresh_dir_listing();
            }
        }
    }

    pub fn refresh_dir_listing(&mut self) {
        let path_str = self.input.value().to_string();
        let dir = if path_str.ends_with('/') {
            PathBuf::from(&path_str)
        } else {
            PathBuf::from(&path_str).parent().map(|p| p.to_path_buf()).unwrap_or_default()
        };

        let filter = if !path_str.ends_with('/') {
            PathBuf::from(&path_str)
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        } else {
            String::new()
        };

        let mut entries = Vec::new();
        if let Ok(read_dir) = std::fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') {
                    continue; // skip hidden
                }
                if !filter.is_empty() && !name.to_lowercase().starts_with(&filter) {
                    continue;
                }
                let path = entry.path();
                let is_dir = path.is_dir();
                if !is_dir {
                    continue; // only show directories
                }
                let is_git_repo = path.join(".git").exists();
                let canonical = std::fs::canonicalize(&path).unwrap_or(path);
                let is_added = self.repos.contains_key(&canonical);
                entries.push(DirEntry {
                    name,
                    is_dir,
                    is_git_repo,
                    is_added,
                });
            }
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        self.dir_entries = entries;
    }
```

**Step 2: Wire up in `handle_key`**

In `handle_key`, add before the `BranchName` input mode check:
```rust
        if self.input_mode == InputMode::AddRepo {
            self.handle_add_repo_key(key);
            return;
        }
```

Also add `InputMode::AddRepo` to the mouse ignore list in `handle_mouse`:
```rust
        if self.show_help || self.show_delete_confirm || self.generating_branch
            || self.input_mode == InputMode::BranchName
            || self.input_mode == InputMode::AddRepo
        {
```

**Step 3: Build and verify**

Run: `cargo build`
Expected: clean build

**Step 4: Run the app to test**

Run: `cargo run`
Expected: App launches, shows tab bar with repo name(s), `[` and `]` switch tabs, `a` opens file picker popup.

**Step 5: Commit**

```
git add src/app.rs
git commit -m "feat: file picker input handling for adding repos"
```

---

### Task 7: Final integration and polish

**Files:**
- Modify: `src/app.rs`, `src/main.rs`, `src/ui.rs` (minor fixes)

**Step 1: Verify all pending `refresh_data` calls are replaced**

Search for any remaining `app.refresh_data()` in `main.rs` — they should all be `refresh_all(&mut app).await`.

Run: `grep -n "refresh_data" src/main.rs`
Expected: no matches (all replaced in Task 5).

**Step 2: Verify no remaining `app.data` / `app.repo_root` / `app.table_state` outside accessors**

Run: `grep -n "app\.data\." src/main.rs src/ui.rs`
Expected: all references go through `app.active()`.

Run: `grep -n "app\.repo_root" src/main.rs src/ui.rs`
Expected: no matches.

**Step 3: Build clean**

Run: `cargo build 2>&1`
Expected: clean build, no warnings

**Step 4: Functional test**

Run: `cargo run`
Verify:
1. Tab bar shows repo name(s)
2. `]` / `[` switch tabs (if multiple repos)
3. `a` opens file picker, can navigate directories, Tab completes, Enter on git repo adds it
4. New repo tab appears, data loads
5. Inactive tab shows `*` when its data changes on refresh
6. All existing functionality works (worktree actions, PR view, sessions, etc.)

**Step 5: Final commit**

```
git add -A
git commit -m "feat: multi-repo support with tab bar, config persistence, and file picker"
```
