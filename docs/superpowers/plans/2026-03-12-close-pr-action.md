# Close PR Action Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a "Close PR" action to the TUI action menu with a confirmation dialog.

**Architecture:** Follows the existing Intent → Command → Executor → Provider chain. The confirmation uses a two-step dispatch: the intent transitions to a `CloseConfirm` UI mode, and only on user confirmation is the `CloseChangeRequest` command dispatched.

**Tech Stack:** Rust, ratatui, async-trait, serde, gh CLI

**Spec:** `docs/superpowers/specs/2026-03-12-close-pr-action-design.md`

---

## Chunk 1: Protocol & Provider Layer

### Task 1: Add `Command::CloseChangeRequest` to protocol

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs`

- [ ] **Step 1: Add command variant**

After `OpenChangeRequest { id: String }` (line 31), add:

```rust
CloseChangeRequest {
    id: String,
},
```

- [ ] **Step 2: Add description**

In `Command::description()`, after the `OpenChangeRequest` arm (line 83), add:

```rust
Command::CloseChangeRequest { .. } => "Closing PR...",
```

- [ ] **Step 3: Update `command_roundtrip_covers_all_variants` test**

After the `Command::OpenChangeRequest` entry (line 168), add:

```rust
Command::CloseChangeRequest { id: "77".into() },
```

- [ ] **Step 4: Update `command_description_covers_all_variants` test**

After the `Command::OpenChangeRequest` entry (line 301), add:

```rust
Command::CloseChangeRequest { id: "1".into() },
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p flotilla-protocol --locked`
Expected: All pass including the updated roundtrip/description tests.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/commands.rs
git commit -m "feat(protocol): add CloseChangeRequest command (#227)"
```

### Task 2: Add `close_change_request` to CodeReview trait + GitHub impl

**Files:**
- Modify: `crates/flotilla-core/src/providers/code_review/mod.rs`
- Modify: `crates/flotilla-core/src/providers/code_review/github.rs`
- Modify: `crates/flotilla-core/src/executor.rs` (MockCodeReview)

- [ ] **Step 1: Add trait method**

In `crates/flotilla-core/src/providers/code_review/mod.rs`, after `open_in_browser` (line 30), add:

```rust
async fn close_change_request(&self, repo_root: &Path, id: &str) -> Result<(), String>;
```

- [ ] **Step 2: Implement in GitHub provider**

In `crates/flotilla-core/src/providers/code_review/github.rs`, after `open_in_browser` (line 200), add:

```rust
async fn close_change_request(&self, repo_root: &Path, id: &str) -> Result<(), String> {
    run!(self.runner, "gh", &["pr", "close", id], repo_root)?;
    Ok(())
}
```

- [ ] **Step 3: Update MockCodeReview in executor.rs**

In `crates/flotilla-core/src/executor.rs`, in the `MockCodeReview` impl (after `open_in_browser` around line 687), add:

```rust
async fn close_change_request(&self, _repo_root: &Path, _id: &str) -> Result<(), String> {
    Ok(())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: All pass (trait satisfied by all implementors).

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/code_review/mod.rs \
       crates/flotilla-core/src/providers/code_review/github.rs \
       crates/flotilla-core/src/executor.rs
git commit -m "feat(core): add close_change_request to CodeReview trait (#227)"
```

### Task 3: Add executor handler + tests

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`

- [ ] **Step 1: Add executor match arm**

In `executor.rs`, after the `Command::OpenChangeRequest` block (around line 177), add:

```rust
Command::CloseChangeRequest { id } => {
    debug!(%id, "closing change request");
    if let Some(cr) = registry.code_review.values().next() {
        cr.close_change_request(repo_root, &id).await.map_err(|e| {
            error!(%id, %e, "failed to close change request");
            e
        })?;
    }
    CommandResult::Ok
}
```

Wait — looking at the `OpenChangeRequest` pattern, it ignores errors with `let _ =`. Let's match that pattern for consistency:

```rust
Command::CloseChangeRequest { id } => {
    debug!(%id, "closing change request");
    if let Some(cr) = registry.code_review.values().next() {
        let _ = cr.close_change_request(repo_root, &id).await;
    }
    CommandResult::Ok
}
```

- [ ] **Step 2: Write test — no provider**

After the `open_change_request_with_provider` test (around line 1508), add:

```rust
// -----------------------------------------------------------------------
// Tests: CloseChangeRequest
// -----------------------------------------------------------------------

#[tokio::test]
async fn close_change_request_no_provider() {
    let registry = empty_registry();
    let runner = runner_ok();

    let result = run_execute(
        Command::CloseChangeRequest {
            id: "42".to_string(),
        },
        &registry,
        &empty_data(),
        &runner,
    )
    .await;

    assert_ok(result);
}
```

- [ ] **Step 3: Write test — with provider**

```rust
#[tokio::test]
async fn close_change_request_with_provider() {
    let mut registry = empty_registry();
    registry
        .code_review
        .insert("github".to_string(), Arc::new(MockCodeReview));
    let runner = runner_ok();

    let result = run_execute(
        Command::CloseChangeRequest {
            id: "42".to_string(),
        },
        &registry,
        &empty_data(),
        &runner,
    )
    .await;

    assert_ok(result);
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: All pass including both new tests.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "feat(core): handle CloseChangeRequest in executor (#227)"
```

## Chunk 2: TUI Layer

### Task 4: Add `CloseConfirm` UI mode

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`

- [ ] **Step 1: Add mode variant**

After `DeleteConfirm` (line 56), add:

```rust
CloseConfirm {
    id: String,
    title: String,
},
```

- [ ] **Step 2: Update `is_config_returns_true_only_for_config_variant` test**

After the `DeleteConfirm` entry (line 292), add:

```rust
(
    UiMode::CloseConfirm {
        id: "42".into(),
        title: "test".into(),
    },
    false,
),
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass. (The new variant will cause compile errors in `handle_key`/`handle_mouse` match arms — fix those in the next task.)

Actually, this will fail to compile because `handle_key` and `handle_mouse` in `key_handlers.rs` have exhaustive matches on `UiMode`. We need to add stubs first. Add a temporary arm to both:

In `handle_key` (key_handlers.rs, around the `match self.ui.mode` block, line 34), the `DeleteConfirm` arm is at line 48. After it, add:

```rust
UiMode::CloseConfirm { .. } => self.handle_close_confirm_key(key),
```

In `handle_mouse` (key_handlers.rs, around line 153-156), the block that returns early for `DeleteConfirm` etc. Add `CloseConfirm`:

```rust
UiMode::Help
| UiMode::DeleteConfirm { .. }
| UiMode::CloseConfirm { .. }
| UiMode::BranchInput { .. }
| UiMode::IssueSearch { .. } => {
    return;
}
```

And add a stub handler method:

```rust
fn handle_close_confirm_key(&mut self, key: KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
            if let UiMode::CloseConfirm { ref id, .. } = self.ui.mode {
                self.proto_commands.push(Command::CloseChangeRequest {
                    id: id.clone(),
                });
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

- [ ] **Step 3 (revised): Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/app/ui_state.rs crates/flotilla-tui/src/app/key_handlers.rs
git commit -m "feat(tui): add CloseConfirm UI mode and key handler (#227)"
```

### Task 5: Add `Intent::CloseChangeRequest`

**Files:**
- Modify: `crates/flotilla-tui/src/app/intent.rs`

- [ ] **Step 1: Add enum variant**

After `ArchiveSession` (line 15), add:

```rust
CloseChangeRequest,
```

- [ ] **Step 2: Add label**

In `label()` match, after `ArchiveSession` (line 34), add:

```rust
Intent::CloseChangeRequest => format!("Close {}", labels.code_review.noun),
```

- [ ] **Step 3: Add is_available**

In `is_available()` match, after `ArchiveSession` (line 57), add:

```rust
Intent::CloseChangeRequest => item.change_request_key.is_some(),
```

- [ ] **Step 4: Add requires_local_host**

`CloseChangeRequest` does NOT require local host (it's an API call). The existing `requires_local_host()` match (line 67) uses an inclusive list — no change needed since unlisted variants return `false`.

- [ ] **Step 5: Add shortcut_hint**

No shortcut for this intent. The existing catch-all `_ => None` (line 102) handles it.

- [ ] **Step 6: Add resolve**

In `resolve()` match, after `ArchiveSession` (line 207), add. This does fine-grained filtering — returns `None` for non-open PRs:

```rust
Intent::CloseChangeRequest => {
    let cr_key = item.change_request_key.as_ref()?;
    let providers = &app.model.active().providers;
    let cr = providers.change_requests.get(cr_key.as_str())?;
    if cr.status != flotilla_protocol::ChangeRequestStatus::Open {
        return None;
    }
    Some(Command::CloseChangeRequest { id: cr_key.clone() })
}
```

- [ ] **Step 7: Add to all_in_menu_order**

After `Intent::ArchiveSession` (line 222), add:

```rust
Intent::CloseChangeRequest,
```

- [ ] **Step 8: Update all_intent_variants in tests**

In `all_intent_variants()` (test helper, around line 541), add `Intent::CloseChangeRequest` to both the array and the match:

Array entry after `Intent::ArchiveSession`:
```rust
Intent::CloseChangeRequest,
```

Match arm after `Intent::ArchiveSession => v`:
```rust
Intent::CloseChangeRequest => v,
```

- [ ] **Step 9: Write is_available test**

After `archive_session_needs_session_key` test (line 407), add:

```rust
#[test]
fn close_pr_needs_change_request_key() {
    let pr = pr_item("123");
    assert!(Intent::CloseChangeRequest.is_available(&pr));

    let no_pr = bare_item();
    assert!(!Intent::CloseChangeRequest.is_available(&no_pr));
}
```

- [ ] **Step 10: Write label tests**

Update `label_with_default_labels` (add after ArchiveSession assert):
```rust
assert_eq!(Intent::CloseChangeRequest.label(&labels), "Close item");
```

Update `label_with_custom_labels` (add after LinkIssuesToChangeRequest assert):
```rust
assert_eq!(Intent::CloseChangeRequest.label(&labels), "Close PR");
```

- [ ] **Step 11: Write shortcut_hint test**

Update `shortcut_hint_none_for_other_intents` — add:
```rust
assert!(Intent::CloseChangeRequest.shortcut_hint(&labels).is_none());
```

- [ ] **Step 12: Write resolve test — open PR**

After existing resolve tests, add:

```rust
#[test]
fn resolve_close_change_request_open_pr() {
    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let rm = app.model.repos.get_mut(&repo).unwrap();
    let mut providers = ProviderData::default();
    providers.change_requests.insert(
        "55".to_string(),
        flotilla_protocol::ChangeRequest {
            title: "My PR".into(),
            branch: "feat/x".into(),
            status: flotilla_protocol::ChangeRequestStatus::Open,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: "github".into(),
            provider_display_name: "GitHub".into(),
        },
    );
    rm.providers = Arc::new(providers);

    let item = pr_item("55");
    let cmd = Intent::CloseChangeRequest.resolve(&item, &app);
    assert!(cmd.is_some());
    match cmd.unwrap() {
        Command::CloseChangeRequest { id } => assert_eq!(id, "55"),
        other => panic!("expected CloseChangeRequest, got {other:?}"),
    }
}
```

- [ ] **Step 13: Write resolve test — merged PR returns None**

```rust
#[test]
fn resolve_close_change_request_none_for_merged() {
    let mut app = stub_app();
    let repo = app.model.repo_order[0].clone();
    let rm = app.model.repos.get_mut(&repo).unwrap();
    let mut providers = ProviderData::default();
    providers.change_requests.insert(
        "56".to_string(),
        flotilla_protocol::ChangeRequest {
            title: "Done PR".into(),
            branch: "feat/done".into(),
            status: flotilla_protocol::ChangeRequestStatus::Merged,
            body: None,
            correlation_keys: vec![],
            association_keys: vec![],
            provider_name: "github".into(),
            provider_display_name: "GitHub".into(),
        },
    );
    rm.providers = Arc::new(providers);

    let item = pr_item("56");
    assert!(Intent::CloseChangeRequest.resolve(&item, &app).is_none());
}
```

- [ ] **Step 14: Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass.

- [ ] **Step 15: Commit**

```bash
git add crates/flotilla-tui/src/app/intent.rs
git commit -m "feat(tui): add CloseChangeRequest intent (#227)"
```

### Task 6: Wire confirmation mode in resolve_and_push + render dialog

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`

- [ ] **Step 1: Intercept in resolve_and_push**

In `resolve_and_push()` (key_handlers.rs, around line 306), update the match inside `if let Some(cmd)` to add `CloseChangeRequest` before the command is pushed. The intent should set `CloseConfirm` mode and **not** push the command (the confirm handler does that):

After `Intent::GenerateBranchName` arm (line 316), add:

```rust
Intent::CloseChangeRequest => {
    self.ui.mode = UiMode::CloseConfirm {
        id: match &cmd {
            Command::CloseChangeRequest { id } => id.clone(),
            _ => return,
        },
        title: item.description.clone(),
    };
    return; // Don't push command — confirm handler will
}
```

- [ ] **Step 2: Add render function**

In `crates/flotilla-tui/src/ui.rs`, after `render_delete_confirm` (line 1024), add:

```rust
fn render_close_confirm(model: &TuiModel, ui: &UiState, frame: &mut Frame) {
    let UiMode::CloseConfirm { ref id, ref title } = ui.mode else {
        return;
    };

    let area = ui_helpers::popup_area(frame.area(), 50, 30);
    frame.render_widget(Clear, area);

    let noun = &model.active_labels().code_review.noun;
    let lines = vec![
        Line::from(vec![
            Span::raw(format!("{} #", noun)),
            Span::styled(id, Style::default().bold()),
        ]),
        Line::from(Span::styled(
            title.as_str(),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "y/Enter: confirm    n/Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block_title = format!(" Close {} ", noun);
    let paragraph = Paragraph::new(lines)
        .block(Block::bordered().title(block_title))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}
```

- [ ] **Step 3: Call render function**

In `render()` (ui.rs, around line 107), after `render_delete_confirm(model, ui, frame);`, add:

```rust
render_close_confirm(model, ui, frame);
```

- [ ] **Step 4: Run full test suite**

Run: `cargo clippy --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: All pass, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/src/ui.rs
git commit -m "feat(tui): wire close PR confirmation dialog (#227)"
```

### Task 7: TUI confirmation flow tests

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs` (test module) or `crates/flotilla-tui/src/app/mod.rs` (test module)

- [ ] **Step 1: Write test — confirm dispatches command**

Find the appropriate test location in key_handlers.rs tests (or mod.rs tests). Add:

```rust
#[test]
fn close_confirm_y_dispatches_command() {
    let mut app = stub_app();
    app.ui.mode = UiMode::CloseConfirm {
        id: "42".into(),
        title: "Test PR".into(),
    };
    app.handle_key(key(KeyCode::Char('y')));
    assert!(matches!(app.ui.mode, UiMode::Normal));
    let cmd = app.proto_commands.take_next();
    assert!(matches!(cmd, Some(Command::CloseChangeRequest { id }) if id == "42"));
}

#[test]
fn close_confirm_enter_dispatches_command() {
    let mut app = stub_app();
    app.ui.mode = UiMode::CloseConfirm {
        id: "42".into(),
        title: "Test PR".into(),
    };
    app.handle_key(key(KeyCode::Enter));
    assert!(matches!(app.ui.mode, UiMode::Normal));
    let cmd = app.proto_commands.take_next();
    assert!(matches!(cmd, Some(Command::CloseChangeRequest { id }) if id == "42"));
}

#[test]
fn close_confirm_esc_cancels() {
    let mut app = stub_app();
    app.ui.mode = UiMode::CloseConfirm {
        id: "42".into(),
        title: "Test PR".into(),
    };
    app.handle_key(key(KeyCode::Esc));
    assert!(matches!(app.ui.mode, UiMode::Normal));
    assert!(app.proto_commands.take_next().is_none());
}

#[test]
fn close_confirm_n_cancels() {
    let mut app = stub_app();
    app.ui.mode = UiMode::CloseConfirm {
        id: "42".into(),
        title: "Test PR".into(),
    };
    app.handle_key(key(KeyCode::Char('n')));
    assert!(matches!(app.ui.mode, UiMode::Normal));
    assert!(app.proto_commands.take_next().is_none());
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p flotilla-tui --locked`
Expected: All pass.

- [ ] **Step 3: Run full verification**

Run: `cargo fmt && cargo clippy --all-targets --locked -- -D warnings && cargo test --workspace --locked`
Expected: All clean.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/
git commit -m "test(tui): add close PR confirmation flow tests (#227)"
```
