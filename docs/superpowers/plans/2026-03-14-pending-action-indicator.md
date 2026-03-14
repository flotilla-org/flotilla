# Pending Action Indicator Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show per-row visual feedback (spinner + shimmer) when actions are in flight on work items, and error markers on failure.

**Architecture:** Add `PendingAction` tracking to `RepoUiState`, carry work item identity through the command queue so `dispatch()` can record it, and render pending rows with the generalized `Shimmer` struct.

**Tech Stack:** Rust, ratatui, tokio

**Spec:** `docs/superpowers/specs/2026-03-14-pending-action-indicator-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|---------------|
| `crates/flotilla-tui/src/shimmer.rs` | Modify | Refactor `shimmer_spans` into `Shimmer` struct with offset-aware `spans()` |
| `crates/flotilla-tui/src/app/ui_state.rs` | Modify | Add `PendingStatus`, `PendingAction`, `PendingActionContext`; add `pending_actions` to `RepoUiState` |
| `crates/flotilla-tui/src/app/mod.rs` | Modify | Extend `CommandQueue` to carry `PendingActionContext`; update `handle_daemon_event` for pending lifecycle; extend `needs_animation` |
| `crates/flotilla-tui/src/app/executor.rs` | Modify | Accept `PendingActionContext` in `dispatch()`; insert pending action on success |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Modify | Attach `PendingActionContext` in `resolve_and_push()` and confirm handlers |
| `crates/flotilla-tui/src/ui.rs` | Modify | Pass pending state to `build_item_row()`; apply shimmer/error rendering |
| `crates/flotilla-tui/src/run.rs` | Modify | Update queue drain to pass context through to `dispatch()` |

---

## Chunk 1: Shimmer Refactor and Data Model

### Task 1: Refactor shimmer.rs into Shimmer struct

**Files:**
- Modify: `crates/flotilla-tui/src/shimmer.rs`

- [ ] **Step 1: Write tests for the Shimmer struct**

Add tests to `shimmer.rs` verifying: (a) `Shimmer::new_at` with a known duration produces deterministic span output; (b) `spans()` with offset shifts the band position; (c) the convenience `shimmer_spans()` wrapper produces the same output as `Shimmer::new(len).spans(text, 0)`.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shimmer_spans_wrapper_matches_struct() {
        let text = "hello world";
        let expected = shimmer_spans(text);
        let shimmer = Shimmer::new(text.chars().count());
        let actual = shimmer.spans(text, 0);
        assert_eq!(expected.len(), actual.len());
        for (e, a) in expected.iter().zip(actual.iter()) {
            assert_eq!(e.style, a.style);
            assert_eq!(e.content, a.content);
        }
    }

    #[test]
    fn new_at_deterministic() {
        let elapsed = Duration::from_millis(500);
        let s1 = Shimmer::new_at(20, elapsed);
        let s2 = Shimmer::new_at(20, elapsed);
        let spans1 = s1.spans("test", 0);
        let spans2 = s2.spans("test", 0);
        for (a, b) in spans1.iter().zip(spans2.iter()) {
            assert_eq!(a.style, b.style);
        }
    }

    #[test]
    fn offset_shifts_band_position() {
        let elapsed = Duration::from_millis(500);
        let shimmer = Shimmer::new_at(40, elapsed);
        let at_zero = shimmer.spans("ab", 0);
        let at_twenty = shimmer.spans("ab", 20);
        // Different offsets should produce different styles
        // (unless both happen to land outside the band)
        let styles_differ = at_zero.iter().zip(at_twenty.iter()).any(|(a, b)| a.style != b.style);
        assert!(styles_differ, "offset should shift the shimmer band");
    }

    #[test]
    fn empty_text_returns_empty_spans() {
        let shimmer = Shimmer::new(10);
        assert!(shimmer.spans("", 0).is_empty());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui shimmer::tests -- --nocapture`
Expected: compilation error — `Shimmer` struct does not exist yet.

- [ ] **Step 3: Implement the Shimmer struct**

Refactor `shimmer.rs`: extract the sweep computation into a `Shimmer` struct with `new()`, `new_at()`, and `spans()`. The per-character loop in `spans()` uses `(offset + i)` instead of `i` for the distance calculation. Keep `shimmer_spans()` as a thin wrapper. Keep `blend`, `elapsed_since_start`, `has_true_color` as private helpers.

```rust
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

fn elapsed_since_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

fn has_true_color() -> bool {
    static TRUE_COLOR: OnceLock<bool> = OnceLock::new();
    *TRUE_COLOR.get_or_init(|| std::env::var("COLORTERM").map(|v| v == "truecolor" || v == "24bit").unwrap_or(false))
}

fn blend(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let r = (a.0 as f32 * t + b.0 as f32 * (1.0 - t)) as u8;
    let g = (a.1 as f32 * t + b.1 as f32 * (1.0 - t)) as u8;
    let b_val = (a.2 as f32 * t + b.2 as f32 * (1.0 - t)) as u8;
    (r, g, b_val)
}

/// Shimmer animation: a bright band sweeps across text on a 2-second cycle.
///
/// For multi-segment use (e.g. table rows), create one `Shimmer` with the total
/// width and call `spans()` per segment with its column offset. For single-text
/// use, call `shimmer_spans()` which wraps this with offset 0.
pub(crate) struct Shimmer {
    pos: f32,
    band_half_width: f32,
    true_color: bool,
    padding: usize,
}

impl Shimmer {
    pub fn new(total_width: usize) -> Self {
        Self::new_at(total_width, elapsed_since_start())
    }

    pub fn new_at(total_width: usize, elapsed: Duration) -> Self {
        let padding = 10usize;
        let period = total_width + padding * 2;
        let sweep_seconds = 2.0f32;
        let pos = (elapsed.as_secs_f32() % sweep_seconds) / sweep_seconds * period as f32;
        Self {
            pos,
            band_half_width: 5.0,
            true_color: has_true_color(),
            padding,
        }
    }

    /// Render a segment of the shimmer at `offset` characters from the row start.
    pub fn spans(&self, text: &str, offset: usize) -> Vec<Span<'static>> {
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return Vec::new();
        }

        let base: (u8, u8, u8) = (140, 130, 40);
        let highlight: (u8, u8, u8) = (255, 240, 120);

        let mut spans = Vec::with_capacity(chars.len());
        for (i, ch) in chars.iter().enumerate() {
            let dist = (((offset + i) as f32 + self.padding as f32) - self.pos).abs();
            let t = if dist <= self.band_half_width {
                0.5 * (1.0 + (std::f32::consts::PI * dist / self.band_half_width).cos())
            } else {
                0.0
            };

            let style = if self.true_color {
                let (r, g, b) = blend(highlight, base, t);
                Style::default().fg(Color::Rgb(r, g, b))
            } else if t < 0.2 {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM)
            } else if t < 0.6 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            };

            spans.push(Span::styled(ch.to_string(), style));
        }
        spans
    }
}

/// Convenience wrapper — single-segment shimmer (status bar, etc.).
pub(crate) fn shimmer_spans(text: &str) -> Vec<Span<'static>> {
    Shimmer::new(text.chars().count()).spans(text, 0)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui shimmer::tests -- --nocapture`
Expected: all 4 tests pass.

- [ ] **Step 5: Run full test suite to verify no regressions**

Run: `cargo test --locked`
Expected: all tests pass. Existing `shimmer_spans` callers are unchanged.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/shimmer.rs
git commit -m "refactor: generalize shimmer into offset-aware Shimmer struct"
```

---

### Task 2: Add PendingAction types and storage to ui_state.rs

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`

- [ ] **Step 1: Write tests for pending action types and cleanup**

Add tests to the existing `mod tests` in `ui_state.rs`:

```rust
// ── PendingAction tests ──────────────────────────────────────────

#[test]
fn pending_actions_default_is_empty() {
    let state = RepoUiState::default();
    assert!(state.pending_actions.is_empty());
}

#[test]
fn pending_actions_cleaned_on_table_view_update() {
    use flotilla_protocol::{HostPath, HostName};

    let mut state = RepoUiState::default();

    let identity_a = WorkItemIdentity::Checkout(HostPath {
        host: HostName::Local,
        path: "/tmp/a".into(),
    });
    let identity_b = WorkItemIdentity::Checkout(HostPath {
        host: HostName::Local,
        path: "/tmp/b".into(),
    });

    state.pending_actions.insert(
        identity_a.clone(),
        PendingAction {
            command_id: 1,
            status: PendingStatus::InFlight,
            description: "test".into(),
        },
    );
    state.pending_actions.insert(
        identity_b.clone(),
        PendingAction {
            command_id: 2,
            status: PendingStatus::InFlight,
            description: "test".into(),
        },
    );

    // Build a table view that only contains identity_a
    let mut table_view = GroupedWorkItems::default();
    let item_a = flotilla_protocol::WorkItem {
        kind: flotilla_protocol::WorkItemKind::Checkout,
        identity: identity_a.clone(),
        host: HostName::Local,
        branch: None,
        description: String::new(),
        checkout: None,
        change_request_key: None,
        session_key: None,
        issue_keys: Vec::new(),
        workspace_refs: Vec::new(),
        is_main_checkout: false,
        debug_group: Vec::new(),
        source: None,
        terminal_keys: Vec::new(),
    };
    table_view.table_entries.push(GroupEntry::Item(item_a));
    table_view.selectable_indices.push(0);

    state.update_table_view(table_view);

    assert!(state.pending_actions.contains_key(&identity_a));
    assert!(!state.pending_actions.contains_key(&identity_b));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui ui_state::tests -- --nocapture`
Expected: compilation error — `PendingAction`, `PendingStatus`, `pending_actions` don't exist.

- [ ] **Step 3: Add types and storage**

In `crates/flotilla-tui/src/app/ui_state.rs`, add the types and field:

```rust
// After existing imports, add:
// (WorkItemIdentity is already imported)

#[derive(Clone, Debug)]
pub enum PendingStatus {
    InFlight,
    Failed(String),
}

#[derive(Clone, Debug)]
pub struct PendingAction {
    pub command_id: u64,
    pub status: PendingStatus,
    pub description: String,
}

#[derive(Clone, Debug)]
pub struct PendingActionContext {
    pub identity: WorkItemIdentity,
    pub description: String,
}
```

Add to `RepoUiState`:

```rust
pub pending_actions: HashMap<WorkItemIdentity, PendingAction>,
```

In `update_table_view()`, add cleanup after the `multi_selected.retain` line:

```rust
self.pending_actions.retain(|id, _| current_identities.contains(id));
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui ui_state::tests -- --nocapture`
Expected: all tests pass including the two new ones.

- [ ] **Step 5: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs
git commit -m "feat: add PendingAction types and storage in RepoUiState"
```

---

## Chunk 2: Command Queue and Dispatch Integration

### Task 3: Extend CommandQueue to carry PendingActionContext

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/executor.rs`
- Modify: `crates/flotilla-tui/src/run.rs`

- [ ] **Step 1: Write tests for the extended CommandQueue**

In the existing `mod tests` in `crates/flotilla-tui/src/app/mod.rs`, add:

```rust
#[test]
fn command_queue_push_with_context() {
    use crate::app::ui_state::PendingActionContext;

    let mut q = CommandQueue::default();
    let ctx = PendingActionContext {
        identity: WorkItemIdentity::Session("s1".into()),
        description: "Archive session".into(),
    };
    q.push_with_context(
        Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } },
        Some(ctx),
    );
    let (cmd, ctx) = q.take_next().expect("should have one entry");
    assert!(matches!(cmd.action, CommandAction::Refresh { .. }));
    assert!(ctx.is_some());
    assert_eq!(ctx.unwrap().description, "Archive session");
}

#[test]
fn command_queue_push_without_context() {
    let mut q = CommandQueue::default();
    q.push(Command { host: None, context_repo: None, action: CommandAction::Refresh { repo: None } });
    let (_, ctx) = q.take_next().expect("should have one entry");
    assert!(ctx.is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui "command_queue_push_with" -- --nocapture`
Expected: compilation error — `push_with_context` doesn't exist, `take_next` returns `Option<Command>` not a tuple.

- [ ] **Step 3: Extend CommandQueue**

In `crates/flotilla-tui/src/app/mod.rs`, change `CommandQueue`:

```rust
use super::ui_state::PendingActionContext;

#[derive(Default)]
pub struct CommandQueue {
    queue: VecDeque<(Command, Option<PendingActionContext>)>,
}

impl CommandQueue {
    pub fn push(&mut self, cmd: Command) {
        self.queue.push_back((cmd, None));
    }
    pub fn push_with_context(&mut self, cmd: Command, ctx: Option<PendingActionContext>) {
        self.queue.push_back((cmd, ctx));
    }
    pub fn take_next(&mut self) -> Option<(Command, Option<PendingActionContext>)> {
        self.queue.pop_front()
    }
}
```

- [ ] **Step 4: Update executor::dispatch to accept PendingActionContext**

In `crates/flotilla-tui/src/app/executor.rs`, change the signature and add pending action insertion:

```rust
use super::ui_state::{PendingAction, PendingActionContext, PendingStatus, UiMode};

pub async fn dispatch(cmd: Command, app: &mut App, pending_ctx: Option<PendingActionContext>) {
    app.model.status_message = None;

    let background_issue_command = matches!(
        cmd.action,
        CommandAction::SetIssueViewport { .. }
            | CommandAction::FetchMoreIssues { .. }
            | CommandAction::SearchIssues { .. }
            | CommandAction::ClearIssueSearch { .. }
    );

    if background_issue_command {
        let daemon = app.daemon.clone();
        tokio::spawn(async move {
            let _ = daemon.execute(cmd).await;
        });
        return;
    }

    match app.daemon.execute(cmd).await {
        Ok(command_id) => {
            if let Some(ctx) = pending_ctx {
                let repo_path = app.model.active_repo_root().clone();
                if let Some(rui) = app.ui.repo_ui.get_mut(&repo_path) {
                    rui.pending_actions.insert(ctx.identity, PendingAction {
                        command_id,
                        status: PendingStatus::InFlight,
                        description: ctx.description,
                    });
                }
            }
        }
        Err(e) => {
            reset_loading_mode(app);
            app.model.status_message = Some(e);
        }
    }
}
```

- [ ] **Step 5: Update run.rs queue drain**

In `crates/flotilla-tui/src/run.rs`, update the drain loop (around line 221):

```rust
// ── Process queued commands ──
while let Some((cmd, pending_ctx)) = app.proto_commands.take_next() {
    app::executor::dispatch(cmd, &mut app, pending_ctx).await;
}
```

- [ ] **Step 6: Fix existing test call sites**

Update existing tests that call `CommandQueue::take_next()`. The return type changes from `Option<Command>` to `Option<(Command, Option<PendingActionContext>)>`. Three patterns need updating:

**Pattern 1: `.unwrap()` into a variable** — destructure the tuple:
```rust
// Before: let cmd = app.proto_commands.take_next().unwrap();
// After:
let (cmd, _) = app.proto_commands.take_next().unwrap();
```

**Pattern 2: `matches!(take_next(), Some(Command { ... }))` in assertions** — wrap in tuple:
```rust
// Before: assert!(matches!(app.proto_commands.take_next(), Some(Command { action: ... })));
// After:
assert!(matches!(app.proto_commands.take_next(), Some((Command { action: ... }, _))));
```

**Pattern 3: `.is_none()` checks** — no change needed (outer `Option` unchanged).

Apply these across both `crates/flotilla-tui/src/app/mod.rs` tests (the `command_queue_push_and_take_fifo` test) and `crates/flotilla-tui/src/app/key_handlers.rs` tests (all `take_next()` usage in resolve_and_push tests and others).

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --locked`
Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs crates/flotilla-tui/src/app/executor.rs crates/flotilla-tui/src/run.rs crates/flotilla-tui/src/app/key_handlers.rs
git commit -m "feat: carry PendingActionContext through command queue to dispatch"
```

---

### Task 4: Handle CommandFinished for pending action lifecycle

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`

- [ ] **Step 1: Write tests for pending action lifecycle on CommandFinished**

In the existing `mod tests` in `crates/flotilla-tui/src/app/mod.rs`, add:

```rust
#[test]
fn command_finished_ok_clears_pending_action() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(
        identity.clone(),
        PendingAction { command_id: 42, status: PendingStatus::InFlight, description: "test".into() },
    );
    app.in_flight.insert(42, InFlightCommand { repo: repo.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        result: CommandResult::Ok,
    });

    assert!(!app.ui.repo_ui[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn command_finished_error_transitions_to_failed() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(
        identity.clone(),
        PendingAction { command_id: 42, status: PendingStatus::InFlight, description: "test".into() },
    );
    app.in_flight.insert(42, InFlightCommand { repo: repo.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        result: CommandResult::Error { message: "boom".into() },
    });

    let pending = &app.ui.repo_ui[&repo].pending_actions[&identity];
    assert!(matches!(pending.status, PendingStatus::Failed(ref msg) if msg == "boom"));
}

#[test]
fn command_finished_cancelled_clears_pending_action() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let identity = WorkItemIdentity::Session("s1".into());

    app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(
        identity.clone(),
        PendingAction { command_id: 42, status: PendingStatus::InFlight, description: "test".into() },
    );
    app.in_flight.insert(42, InFlightCommand { repo: repo.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        result: CommandResult::Cancelled,
    });

    assert!(!app.ui.repo_ui[&repo].pending_actions.contains_key(&identity));
}

#[test]
fn orphaned_command_finished_harmlessly_ignored() {
    use crate::app::ui_state::{PendingAction, PendingStatus};

    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let identity = WorkItemIdentity::Session("s1".into());

    // Insert pending action with command_id 99 (different from finished event)
    app.ui.repo_ui.get_mut(&repo).unwrap().pending_actions.insert(
        identity.clone(),
        PendingAction { command_id: 99, status: PendingStatus::InFlight, description: "test".into() },
    );
    app.in_flight.insert(42, InFlightCommand { repo: repo.clone(), description: "test".into() });

    app.handle_daemon_event(DaemonEvent::CommandFinished {
        command_id: 42,
        result: CommandResult::Ok,
    });

    // The pending action with command_id 99 should still be there
    assert!(app.ui.repo_ui[&repo].pending_actions.contains_key(&identity));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui "command_finished_ok_clears\|command_finished_error\|command_finished_cancelled\|orphaned_command" -- --nocapture`
Expected: the first test fails (pending action is not removed — no scanning logic yet).

- [ ] **Step 3: Add pending action lifecycle to handle_daemon_event**

In `crates/flotilla-tui/src/app/mod.rs`, replace the `CommandFinished` arm of `handle_daemon_event()` with:

```rust
DaemonEvent::CommandFinished { command_id, result, .. } => {
    if let Some(_cmd) = self.in_flight.remove(&command_id) {
        tracing::info!(%command_id, "command finished");
        let error_message = match &result {
            CommandResult::Error { message } => Some(message.clone()),
            _ => None,
        };
        executor::handle_result(result, self);

        // Find which repo+identity has this command_id
        let found: Option<(PathBuf, WorkItemIdentity)> = self
            .ui
            .repo_ui
            .iter()
            .find_map(|(path, rui)| {
                rui.pending_actions
                    .iter()
                    .find(|(_, a)| a.command_id == command_id)
                    .map(|(id, _)| (path.clone(), id.clone()))
            });

        if let Some((repo_path, identity)) = found {
            let rui = self.ui.repo_ui.get_mut(&repo_path).expect("repo exists");
            if let Some(message) = error_message {
                if let Some(entry) = rui.pending_actions.get_mut(&identity) {
                    entry.status = PendingStatus::Failed(message);
                }
            } else {
                rui.pending_actions.remove(&identity);
            }
        }
    }
}
```

- [ ] **Step 4: Add the required imports**

Add to the imports at the top of `mod.rs`:

```rust
use super::ui_state::PendingStatus;
```

`WorkItemIdentity` should already be available via `flotilla_protocol`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui "command_finished_ok_clears\|command_finished_error\|command_finished_cancelled\|orphaned_command" -- --nocapture`
Expected: all 4 tests pass.

- [ ] **Step 6: Extend needs_animation**

In `crates/flotilla-tui/src/app/mod.rs`, update `needs_animation()`:

```rust
pub fn needs_animation(&self) -> bool {
    if !self.in_flight.is_empty() {
        return true;
    }
    if self.ui.repo_ui.values().any(|rui| {
        rui.pending_actions.values().any(|a| matches!(a.status, PendingStatus::InFlight))
    }) {
        return true;
    }
    matches!(self.ui.mode, UiMode::BranchInput { kind: BranchInputKind::Generating, .. } | UiMode::DeleteConfirm { loading: true, .. })
}
```

- [ ] **Step 7: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-tui/src/app/mod.rs
git commit -m "feat: handle pending action lifecycle on CommandFinished events"
```

---

### Task 5: Attach PendingActionContext in resolve_and_push and confirm handlers

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/app/ui_state.rs` (add `identity` to `CloseConfirm`)
- Modify: `crates/flotilla-tui/src/ui.rs` (update `render_close_confirm` destructure)

- [ ] **Step 1: Write tests for pending context attachment**

Add tests in the existing `mod tests` in `key_handlers.rs`:

```rust
#[test]
fn resolve_and_push_attaches_pending_context() {
    let mut app = stub_app();
    let item = make_work_item("a");
    app.resolve_and_push(Intent::CreateWorkspace, &item);
    let (_, ctx) = app.proto_commands.take_next().expect("should have command");
    let ctx = ctx.expect("should have pending context");
    assert_eq!(ctx.identity, item.identity);
}

#[test]
fn close_confirm_attaches_pending_context() {
    let mut app = stub_app();
    let item = make_work_item("a");
    // Set up CloseConfirm mode with the item's identity
    app.ui.mode = UiMode::CloseConfirm {
        id: "PR-1".into(),
        title: "test".into(),
        identity: item.identity.clone(),
    };
    // Simulate pressing 'y' to confirm
    app.handle_key(KeyEvent::from(KeyCode::Char('y')));
    let (_, ctx) = app.proto_commands.take_next().expect("should have command");
    let ctx = ctx.expect("should have pending context");
    assert_eq!(ctx.identity, item.identity);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui "resolve_and_push_attaches\|close_confirm_attaches" -- --nocapture`
Expected: first test fails — `push_with_context` not called from `resolve_and_push`.

- [ ] **Step 3: Update resolve_and_push to use push_with_context**

In `crates/flotilla-tui/src/app/key_handlers.rs`, modify `resolve_and_push()`:

```rust
fn resolve_and_push(&mut self, intent: Intent, item: &WorkItem) {
    if !intent.is_allowed_for_host(item, &self.model.my_host) {
        tracing::warn!(?intent, host = %item.host, "blocked intent on remote item");
        self.model.status_message = Some("Cannot perform this action on a remote item".to_string());
        return;
    }

    if let Some(cmd) = intent.resolve(item, self) {
        match intent {
            Intent::RemoveCheckout => {
                self.ui.mode = UiMode::DeleteConfirm { info: None, loading: true, terminal_keys: item.terminal_keys.clone() };
            }
            Intent::GenerateBranchName => {
                self.enter_branch_input(BranchInputKind::Generating);
            }
            Intent::CloseChangeRequest => {
                // CloseChangeRequest doesn't push now — the confirm handler
                // creates its own PendingActionContext when the user confirms.
                self.ui.mode = UiMode::CloseConfirm {
                    id: match &cmd {
                        Command { action: CommandAction::CloseChangeRequest { id }, .. } => id.clone(),
                        _ => return,
                    },
                    title: item.description.clone(),
                    identity: item.identity.clone(),
                };
                return;
            }
            _ => {}
        }
        let pending_ctx = PendingActionContext {
            identity: item.identity.clone(),
            description: intent.label(self.model.active_labels()),
        };
        self.proto_commands.push_with_context(cmd, Some(pending_ctx));
    }
}
```

- [ ] **Step 4: Add identity field to CloseConfirm variant**

In `crates/flotilla-tui/src/app/ui_state.rs`, update the `CloseConfirm` variant:

```rust
CloseConfirm {
    id: String,
    title: String,
    identity: WorkItemIdentity,
},
```

Update all sites that construct or destructure `CloseConfirm`:

- `crates/flotilla-tui/src/app/ui_state.rs` — the `is_config` test case vector (line ~277): add `identity` field
- `crates/flotilla-tui/src/app/mod.rs` — test cases for CloseConfirm (around lines 1116, 1126, 1136, 1145): add `identity` field using `WorkItemIdentity::Session("test".into())` or similar
- `crates/flotilla-tui/src/ui.rs` — `render_close_confirm` function (~line 951) destructures `CloseConfirm { ref id, ref title }` without `..`: add `..` or the new field
- `crates/flotilla-tui/src/app/key_handlers.rs` — match arms using `..` are already fine

- [ ] **Step 5: Update handle_close_confirm_key to attach context**

In `crates/flotilla-tui/src/app/key_handlers.rs`, update the confirm handler:

```rust
fn handle_close_confirm_key(&mut self, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
            if let UiMode::CloseConfirm { ref id, ref identity, .. } = self.ui.mode {
                let ctx = PendingActionContext {
                    identity: identity.clone(),
                    description: format!("Close {}", id),
                };
                self.proto_commands.push_with_context(
                    self.repo_command(CommandAction::CloseChangeRequest { id: id.clone() }),
                    Some(ctx),
                );
            }
            self.ui.mode = UiMode::Normal;
        }
        KeyCode::Esc | KeyCode::Char('n') => {
            self.ui.mode = UiMode::Normal;
        }
        _ => {}
    }
}
```

- [ ] **Step 6: Add required imports to key_handlers.rs**

```rust
use super::ui_state::PendingActionContext;
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --locked`
Expected: all tests pass. Check for any compilation errors from the `CloseConfirm` variant change.

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/ui.rs
git commit -m "feat: attach PendingActionContext in resolve_and_push and confirm handlers"
```

---

## Chunk 3: Rendering

### Task 6: Render pending action indicators on work item rows

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs`
- Modify: `crates/flotilla-tui/src/ui_helpers.rs` (spinner helper)

- [ ] **Step 1: Add braille spinner helper to ui_helpers.rs**

In `crates/flotilla-tui/src/ui_helpers.rs`, add the spinner constant and function:

```rust
const BRAILLE_SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

pub fn spinner_char() -> char {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
    BRAILLE_SPINNER[(ms / 100) as usize % BRAILLE_SPINNER.len()]
}
```

Add a test in the existing `mod tests`:

```rust
#[test]
fn spinner_char_returns_valid_braille() {
    let ch = spinner_char();
    assert!(BRAILLE_SPINNER.contains(&ch));
}
```

- [ ] **Step 2: Modify build_item_row to accept pending state**

In `crates/flotilla-tui/src/ui.rs`, change the `build_item_row` signature:

```rust
fn build_item_row<'a>(
    item: &WorkItem,
    providers: &ProviderData,
    col_widths: &[u16],
    repo_root: &Path,
    prev_source: Option<&str>,
    pending: Option<&PendingAction>,
) -> Row<'a> {
```

Add the imports. Note: `Modifier` is not currently imported in `ui.rs` — add it to the existing `style::` import:

```rust
use ratatui::style::{Color, Modifier, Style};  // add Modifier
use crate::app::ui_state::{PendingAction, PendingStatus};
use crate::shimmer::Shimmer;
```

- [ ] **Step 3: Implement shimmer rendering for InFlight rows**

At the end of `build_item_row`, before constructing the `Row`, add pending logic. When `pending` is `Some` with `InFlight` status:

```rust
if let Some(pending) = pending {
    match &pending.status {
        PendingStatus::InFlight => {
            let total_width: usize = col_widths.iter().map(|w| *w as usize).sum();
            let shimmer = Shimmer::new(total_width);
            let spinner = ui_helpers::spinner_char();

            let mut offset: usize = 0;
            let cells = vec![
                (format!(" {spinner}"), col_widths.get(0).copied().unwrap_or(3) as usize),
                (source_display.clone(), col_widths.get(1).copied().unwrap_or(8) as usize),
                (path_display.clone(), col_widths.get(2).copied().unwrap_or(14) as usize),
                (description.clone(), col_widths.get(3).copied().unwrap_or(15) as usize),
                (branch_display.clone(), col_widths.get(4).copied().unwrap_or(25) as usize),
                (wt_indicator.to_string(), col_widths.get(5).copied().unwrap_or(3) as usize),
                (ws_indicator.clone(), col_widths.get(6).copied().unwrap_or(3) as usize),
                (pr_display.clone(), col_widths.get(7).copied().unwrap_or(8) as usize),
                (session_display.clone(), col_widths.get(8).copied().unwrap_or(8) as usize),
                (issues_display.clone(), col_widths.get(9).copied().unwrap_or(8) as usize),
                (git_display.clone(), col_widths.get(10).copied().unwrap_or(5) as usize),
            ];

            let shimmer_cells: Vec<Cell> = cells
                .into_iter()
                .map(|(text, width)| {
                    let spans = shimmer.spans(&text, offset);
                    offset += width;
                    Cell::from(Line::from(spans))
                })
                .collect();

            return Row::new(shimmer_cells);
        }
        PendingStatus::Failed(_) => {
            let error_style = Style::default().fg(Color::Red).add_modifier(Modifier::DIM);
            return Row::new(vec![
                Cell::from(Span::styled(" ✗", Style::default().fg(Color::Red))),
                Cell::from(Span::styled(source_display, error_style)),
                Cell::from(Span::styled(path_display, error_style)),
                Cell::from(Span::styled(description, error_style)),
                Cell::from(Span::styled(branch_display, error_style)),
                Cell::from(Span::styled(wt_indicator.to_string(), error_style)),
                Cell::from(Span::styled(ws_indicator, error_style)),
                Cell::from(Span::styled(pr_display, error_style)),
                Cell::from(Span::styled(session_display, error_style)),
                Cell::from(Span::styled(issues_display, error_style)),
                Cell::from(Span::styled(git_display, error_style)),
            ]);
        }
    }
}

// ... existing Row::new(vec![...]) for normal rendering
```

- [ ] **Step 4: Update the caller in render_unified_table**

In `crates/flotilla-tui/src/ui.rs`, in the `render_unified_table` function, update the `GroupEntry::Item` arm (around line 544):

```rust
GroupEntry::Item(item) => {
    let pending = rui.pending_actions.get(&item.identity);
    let mut row = build_item_row(item, &rm.providers, &col_widths, model.active_repo_root(), prev_source.as_deref(), pending);
    prev_source = item.source.clone();
    if is_multi_selected {
        row = row.style(Style::default().bg(Color::Indexed(236)));
    }
    row
}
```

- [ ] **Step 5: Run the full test suite**

Run: `cargo test --locked`
Expected: all tests pass.

- [ ] **Step 6: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: no warnings.

- [ ] **Step 7: Format**

Run: `cargo +nightly fmt`

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-tui/src/ui.rs crates/flotilla-tui/src/ui_helpers.rs
git commit -m "feat: render shimmer and error indicators on pending action rows"
```

---

### Task 7: Final verification

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: no warnings.

- [ ] **Step 3: Format**

Run: `cargo +nightly fmt`

- [ ] **Step 4: Manual smoke test**

Run: `cargo run` and verify:
1. Trigger an action from the action menu (e.g., create workspace)
2. The affected row should show a braille spinner with shimmer effect
3. On success, the shimmer disappears when the command finishes
4. Trigger an action that might fail to verify the red error indicator appears
