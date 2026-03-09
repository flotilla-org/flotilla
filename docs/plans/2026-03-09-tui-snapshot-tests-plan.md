# TUI Rendering Snapshot Tests Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add snapshot tests for `crates/flotilla-tui/src/ui.rs` using ratatui's `TestBackend` and `insta` for snapshot management.

**Architecture:** Integration tests in `crates/flotilla-tui/tests/` with a `TestHarness` struct that wraps terminal setup, fixture construction, and buffer capture. Each test builds a `TuiModel` + `UiState`, renders into a `TestBackend`, and asserts with `insta::assert_snapshot!`.

**Tech Stack:** ratatui `TestBackend`, `insta` crate, `flotilla-core::data::group_work_items`

**Note:** `crates/flotilla-tui/src/app/test_support.rs` (from #162) has work item builders (`checkout_item`, `pr_item`, etc.) but is `#[cfg(test)] pub(crate)` — not accessible from integration tests. Our `test_fixtures.rs` duplicates the work item builders we need and adds `ProviderData` builders and the `TestHarness` that `test_support` doesn't have.

---

### Task 1: Add `insta` dev-dependency

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml:28-30`

**Step 1: Add insta to dev-dependencies**

```toml
[dev-dependencies]
async-trait = { workspace = true }
tempfile = "3"
insta = "1"
```

**Step 2: Verify it compiles**

Run: `cargo check -p flotilla-tui --tests`
Expected: compiles without errors

**Step 3: Commit**

```bash
git add crates/flotilla-tui/Cargo.toml Cargo.lock
git commit -m "chore: add insta dev-dependency for TUI snapshot tests (#96)"
```

---

### Task 2: Create test harness and first snapshot tests

**Files:**
- Create: `crates/flotilla-tui/tests/test_fixtures.rs`
- Create: `crates/flotilla-tui/tests/snapshots.rs`

**Step 1: Write test_fixtures.rs**

```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use flotilla_core::data::{group_work_items, SectionLabels};
use flotilla_protocol::{
    ChangeRequest, ChangeRequestStatus, Checkout, CheckoutRef, CloudAgentSession, CorrelationKey,
    Issue, ProviderData, RepoInfo, RepoLabels, SessionStatus, WorkItem, WorkItemIdentity,
    WorkItemKind,
};
use flotilla_tui::app::{InFlightCommand, ProviderStatus, TuiModel, UiMode, UiState};
use flotilla_tui::ui;

const TERM_WIDTH: u16 = 120;
const TERM_HEIGHT: u16 = 30;

pub struct TestHarness {
    pub model: TuiModel,
    pub ui: UiState,
    pub in_flight: HashMap<u64, InFlightCommand>,
}

impl TestHarness {
    pub fn empty() -> Self {
        let model = TuiModel::from_repo_info(vec![]);
        let ui = UiState::new(&[]);
        Self {
            model,
            ui,
            in_flight: HashMap::new(),
        }
    }

    pub fn single_repo(name: &str) -> Self {
        let path = PathBuf::from(format!("/test/{name}"));
        let info = RepoInfo {
            path: path.clone(),
            name: name.to_string(),
            labels: test_labels(),
            provider_names: HashMap::new(),
            provider_health: HashMap::new(),
            loading: false,
        };
        let model = TuiModel::from_repo_info(vec![info]);
        let ui = UiState::new(&[path]);
        Self {
            model,
            ui,
            in_flight: HashMap::new(),
        }
    }

    pub fn multi_repo(names: &[&str]) -> Self {
        let mut infos = Vec::new();
        let mut paths = Vec::new();
        for name in names {
            let path = PathBuf::from(format!("/test/{name}"));
            infos.push(RepoInfo {
                path: path.clone(),
                name: name.to_string(),
                labels: test_labels(),
                provider_names: HashMap::new(),
                provider_health: HashMap::new(),
                loading: false,
            });
            paths.push(path);
        }
        let model = TuiModel::from_repo_info(infos);
        let ui = UiState::new(&paths);
        Self {
            model,
            ui,
            in_flight: HashMap::new(),
        }
    }

    pub fn with_mode(mut self, mode: UiMode) -> Self {
        self.ui.mode = mode;
        self
    }

    pub fn with_status_message(mut self, msg: &str) -> Self {
        self.model.status_message = Some(msg.to_string());
        self
    }

    pub fn with_provider_status(
        mut self,
        repo_name: &str,
        category: &str,
        provider: &str,
        status: ProviderStatus,
    ) -> Self {
        let path = PathBuf::from(format!("/test/{repo_name}"));
        self.model
            .provider_statuses
            .insert((path, category.to_string(), provider.to_string()), status);
        self
    }

    /// Populate the active repo with work items and provider data.
    /// Runs group_work_items to build the table view, matching real app behavior.
    pub fn with_provider_data(mut self, providers: ProviderData, items: Vec<WorkItem>) -> Self {
        let path = self.model.repo_order[0].clone();
        let labels = &self.model.repos[&path].labels;
        let section_labels = SectionLabels {
            checkouts: labels.checkouts.section.clone(),
            code_review: labels.code_review.section.clone(),
            issues: labels.issues.section.clone(),
            sessions: labels.sessions.section.clone(),
        };
        let grouped = group_work_items(&items, &providers, &section_labels);

        if let Some(repo) = self.model.repos.get_mut(&path) {
            repo.providers = Arc::new(providers);
        }
        if let Some(repo_ui) = self.ui.repo_ui.get_mut(&path) {
            repo_ui.update_table_view(grouped);
        }
        self
    }

    pub fn render_to_string(&mut self) -> String {
        let backend = TestBackend::new(TERM_WIDTH, TERM_HEIGHT);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| ui::render(&self.model, &mut self.ui, &self.in_flight, f))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer_to_string(&buffer)
    }
}

fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    let mut result = String::new();
    for y in area.y..area.y + area.height {
        let mut line = String::new();
        for x in area.x..area.x + area.width {
            let cell = &buffer[(x, y)];
            line.push_str(cell.symbol());
        }
        result.push_str(line.trim_end());
        result.push('\n');
    }
    result
}

fn test_labels() -> RepoLabels {
    RepoLabels {
        checkouts: flotilla_protocol::CategoryLabels {
            section: "Worktrees".into(),
            noun: "worktree".into(),
            abbr: "WT".into(),
        },
        code_review: flotilla_protocol::CategoryLabels {
            section: "Pull Requests".into(),
            noun: "PR".into(),
            abbr: "PR".into(),
        },
        issues: flotilla_protocol::CategoryLabels {
            section: "Issues".into(),
            noun: "issue".into(),
            abbr: "IS".into(),
        },
        sessions: flotilla_protocol::CategoryLabels {
            section: "Sessions".into(),
            noun: "session".into(),
            abbr: "SS".into(),
        },
    }
}

// ── ProviderData builders (not in test_support.rs) ────────────────────

pub fn make_checkout(branch: &str, path: &str, is_trunk: bool) -> (PathBuf, Checkout) {
    (
        PathBuf::from(path),
        Checkout {
            branch: branch.into(),
            is_trunk,
            trunk_ahead_behind: None,
            remote_ahead_behind: None,
            working_tree: None,
            last_commit: None,
            correlation_keys: vec![CorrelationKey::Branch(branch.into())],
            association_keys: vec![],
        },
    )
}

pub fn make_change_request(id: &str, title: &str, branch: &str) -> (String, ChangeRequest) {
    (
        id.to_string(),
        ChangeRequest {
            title: title.into(),
            branch: branch.into(),
            status: ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![CorrelationKey::Branch(branch.into())],
            association_keys: vec![],
        },
    )
}

pub fn make_issue(id: &str, title: &str) -> (String, Issue) {
    (
        id.to_string(),
        Issue {
            title: title.into(),
            labels: vec![],
            association_keys: vec![],
        },
    )
}

pub fn make_session(id: &str, title: &str, status: SessionStatus) -> (String, CloudAgentSession) {
    (
        id.to_string(),
        CloudAgentSession {
            title: title.into(),
            status,
            model: None,
            updated_at: None,
            correlation_keys: vec![],
        },
    )
}

// ── WorkItem builders (mirrors test_support.rs, needed for integration tests) ─

pub fn make_work_item_checkout(branch: &str, path: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Checkout,
        identity: WorkItemIdentity::Checkout(PathBuf::from(path)),
        branch: Some(branch.into()),
        description: branch.into(),
        checkout: Some(CheckoutRef {
            key: PathBuf::from(path),
            is_main_checkout: false,
        }),
        change_request_key: None,
        session_key: None,
        issue_keys: vec![],
        workspace_refs: vec![],
        is_main_checkout: false,
        debug_group: vec![],
    }
}

pub fn make_work_item_cr(id: &str, title: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::ChangeRequest,
        identity: WorkItemIdentity::ChangeRequest(id.into()),
        branch: None,
        description: title.into(),
        checkout: None,
        change_request_key: Some(id.into()),
        session_key: None,
        issue_keys: vec![],
        workspace_refs: vec![],
        is_main_checkout: false,
        debug_group: vec![],
    }
}

pub fn make_work_item_issue(id: &str, title: &str) -> WorkItem {
    WorkItem {
        kind: WorkItemKind::Issue,
        identity: WorkItemIdentity::Issue(id.into()),
        branch: None,
        description: title.into(),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: vec![id.into()],
        workspace_refs: vec![],
        is_main_checkout: false,
        debug_group: vec![],
    }
}
```

**Step 2: Write snapshots.rs with first three tests**

```rust
mod test_fixtures;

use flotilla_protocol::{ProviderData, SessionStatus};
use flotilla_tui::app::{ProviderStatus, UiMode};
use test_fixtures::*;

#[test]
fn empty_state() {
    let mut harness = TestHarness::empty();
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn single_repo_empty_table() {
    let mut harness = TestHarness::single_repo("my-project");
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn single_repo_with_items() {
    let mut providers = ProviderData::default();
    let (path, checkout) = make_checkout("feat-login", "/test/my-project/feat-login", false);
    providers.checkouts.insert(path, checkout);
    let (id, cr) = make_change_request("42", "Add login page", "feat-login");
    providers.change_requests.insert(id, cr);
    let (id, issue) = make_issue("10", "Users need authentication");
    providers.issues.insert(id, issue);
    let (id, session) = make_session("s1", "Implement auth flow", SessionStatus::Idle);
    providers.sessions.insert(id, session);

    let items = vec![
        make_work_item_checkout("feat-login", "/test/my-project/feat-login"),
        make_work_item_cr("42", "Add login page"),
        make_work_item_issue("10", "Users need authentication"),
    ];

    let mut harness =
        TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

**Step 3: Run tests, review and accept snapshots**

Run: `cargo test -p flotilla-tui --test snapshots`
Then: `cargo insta review` or manually rename `.snap.new` → `.snap`
Verify each snapshot looks reasonable.

**Step 4: Commit**

```bash
git add crates/flotilla-tui/Cargo.toml Cargo.lock \
    crates/flotilla-tui/tests/test_fixtures.rs \
    crates/flotilla-tui/tests/snapshots.rs \
    crates/flotilla-tui/tests/snapshots/
git commit -m "test: TUI snapshot test harness with initial tests (#96)"
```

---

### Task 3: Tab bar, status bar, and help screen snapshots

**Files:**
- Modify: `crates/flotilla-tui/tests/snapshots.rs`

**Step 1: Add three tests**

```rust
#[test]
fn tab_bar_multiple_repos() {
    let mut harness = TestHarness::multi_repo(&["alpha", "beta", "gamma"]);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn status_bar_with_error() {
    let mut harness = TestHarness::single_repo("my-project")
        .with_status_message("GitHub API rate limit exceeded");
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn help_screen() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::Help);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

**Step 2: Run, review, accept**

Run: `cargo test -p flotilla-tui --test snapshots`
Then: `cargo insta review`
Verify: tab bar shows all repo names; error shows in status bar; help overlay has keybindings.

**Step 3: Commit**

```bash
git add crates/flotilla-tui/tests/snapshots.rs crates/flotilla-tui/tests/snapshots/
git commit -m "test: tab bar, status bar, and help screen snapshots (#96)"
```

---

### Task 4: Action menu, config screen, and preview panel snapshots

**Files:**
- Modify: `crates/flotilla-tui/tests/snapshots.rs`

**Step 1: Add three tests**

Note: `Intent` variants are: `SwitchToWorkspace`, `CreateWorkspace`, `RemoveCheckout`, `CreateCheckoutAndWorkspace`, `GenerateBranchName`, `OpenChangeRequest`, `OpenIssue`, `LinkIssuesToChangeRequest`, `TeleportSession`, `ArchiveSession`. Pick a representative subset.

```rust
use flotilla_tui::app::Intent;

#[test]
fn action_menu() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::ActionMenu {
        items: vec![
            Intent::CreateWorkspace,
            Intent::OpenChangeRequest,
            Intent::RemoveCheckout,
        ],
        index: 0,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn config_screen() {
    let mut harness = TestHarness::single_repo("my-project")
        .with_mode(UiMode::Config)
        .with_provider_status("my-project", "code_review", "GitHub", ProviderStatus::Ok)
        .with_provider_status("my-project", "issues", "GitHub", ProviderStatus::Error);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}

#[test]
fn selected_item_preview() {
    let mut providers = ProviderData::default();
    let (path, checkout) =
        make_checkout("feat-dashboard", "/test/my-project/feat-dashboard", false);
    providers.checkouts.insert(path, checkout);
    let (id, cr) = make_change_request("99", "Build analytics dashboard", "feat-dashboard");
    providers.change_requests.insert(id, cr);

    let items = vec![
        make_work_item_checkout("feat-dashboard", "/test/my-project/feat-dashboard"),
        make_work_item_cr("99", "Build analytics dashboard"),
    ];

    let mut harness =
        TestHarness::single_repo("my-project").with_provider_data(providers, items);
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

**Step 2: Run, review, accept**

Run: `cargo test -p flotilla-tui --test snapshots`
Then: `cargo insta review`
Verify: action menu popup renders; config shows provider statuses; preview shows selected item details.

**Step 3: Commit**

```bash
git add crates/flotilla-tui/tests/snapshots.rs crates/flotilla-tui/tests/snapshots/
git commit -m "test: action menu, config screen, and preview snapshots (#96)"
```

---

### Task 5: Final verification

**Step 1: Run all snapshot tests**

Run: `cargo test -p flotilla-tui --test snapshots`
Expected: all 9 tests pass

**Step 2: Run full test suite + lint**

Run: `cargo fmt && cargo clippy --all-targets --locked -- -D warnings && cargo test --locked`
Expected: all pass, no warnings

**Step 3: Commit any fixups**

Only if needed from clippy/fmt.
