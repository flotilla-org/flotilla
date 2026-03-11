# Show Modified Git Content on Delete Worktree Confirmation

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show the list of uncommitted/modified files in the delete worktree confirmation dialog so users can make an informed decision about data loss.

**Architecture:** Widen the existing `git status --porcelain` data pipe from `bool` to `Vec<String>`. The output is already fetched in `fetch_checkout_status()` — we just need to preserve the file list instead of reducing it to a boolean. The UI renders the files under the existing warning, capped at 10 with an overflow indicator.

**Tech Stack:** Rust, ratatui, crossterm, insta (snapshot tests)

---

## Chunk 1: Protocol, data, and rendering

### Task 1: Add `uncommitted_files` field to `CheckoutStatus`

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs:116-124`

- [ ] **Step 1: Add the field**

In `CheckoutStatus`, add `uncommitted_files` and derive `has_uncommitted` from it:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CheckoutStatus {
    pub branch: String,
    pub change_request_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
    #[serde(default)]
    pub uncommitted_files: Vec<String>,  // NEW: raw `git status --porcelain` lines
    pub base_detection_warning: Option<String>,
}
```

Keep `has_uncommitted` as-is for the "safe to delete" check. The new field carries the detail. The `#[serde(default)]` ensures old serialised messages (from a daemon on a prior version) deserialise cleanly with an empty vec.

- [ ] **Step 2: Update the roundtrip test data**

In `command_result_roundtrip_covers_all_variants` (line ~220), add the field to the `CheckoutStatus` instance:

```rust
CommandResult::CheckoutStatus(CheckoutStatus {
    branch: "old".into(),
    change_request_status: Some("merged".into()),
    merge_commit_sha: Some("abc123".into()),
    unpushed_commits: vec!["def456".into()],
    has_uncommitted: true,
    uncommitted_files: vec!["M  src/main.rs".into(), "?? TODO.txt".into()],
    base_detection_warning: Some("warning text".into()),
}),
```

- [ ] **Step 3: Update the `checkout_status_roundtrip_preserves_fields` test data**

In the test at line ~259, add the field:

```rust
let info = CheckoutStatus {
    branch: "cleanup".to_string(),
    change_request_status: Some("closed".into()),
    merge_commit_sha: Some("deadbeef".into()),
    unpushed_commits: vec!["aaa".into(), "bbb".into()],
    has_uncommitted: true,
    uncommitted_files: vec!["M  src/lib.rs".into()],
    base_detection_warning: Some("ambiguous base".into()),
};
```

- [ ] **Step 4: Update the `checkout_status_default` test assertion**

At line ~249, add:

```rust
assert!(info.uncommitted_files.is_empty());
```

- [ ] **Step 5: Run protocol tests**

Run: `cargo test -p flotilla-protocol`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/commands.rs
git commit -m "feat(protocol): add uncommitted_files to CheckoutStatus (#226)"
```

---

### Task 2: Populate `uncommitted_files` in `fetch_checkout_status()`

**Files:**
- Modify: `crates/flotilla-core/src/data.rs:611-737`

- [ ] **Step 1: Parse git status output into file list**

Replace the current boolean reduction (line 720):

```rust
// Before:
info.has_uncommitted = !uncommitted.trim().is_empty();

// After:
info.uncommitted_files = uncommitted
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| l.to_string())
    .collect();
info.has_uncommitted = !info.uncommitted_files.is_empty();
```

The `git status --porcelain` output has lines like `M  src/main.rs`, `?? TODO.txt`, `AM new.rs`. We store the raw lines — the UI will display them as-is.

- [ ] **Step 2: Add a unit test with mock porcelain output**

In `crates/flotilla-core/src/executor.rs` tests, add a test that provides actual `git status --porcelain` output from the mock runner to verify the file list is populated. The existing `fetch_checkout_status_returns_checkout_status` test at line ~1291 uses all-error mocks. Add a companion test where the third mock (the `git status --porcelain` call) returns real output:

```rust
#[tokio::test]
async fn fetch_checkout_status_populates_uncommitted_files() {
    let registry = empty_registry();
    // Mock responses: upstream -> Err, origin/HEAD -> Err, git status -> Ok, gh pr -> Err
    let runner = MockRunner::new(vec![
        Err("err".to_string()),
        Err("err".to_string()),
        Ok(" M src/main.rs\n?? TODO.txt\n".to_string()),
        Err("err".to_string()),
    ]);

    let result = run_execute(
        Command::FetchCheckoutStatus {
            branch: "feat".to_string(),
            checkout_path: Some(PathBuf::from("/repo/wt")),
            change_request_id: None,
        },
        &registry,
        &empty_data(),
        &runner,
    )
    .await;

    match result {
        CommandResult::CheckoutStatus(info) => {
            assert!(info.has_uncommitted);
            assert_eq!(info.uncommitted_files, vec![
                " M src/main.rs".to_string(),
                "?? TODO.txt".to_string(),
            ]);
        }
        other => panic!("expected CheckoutStatus, got {other:?}"),
    }
}
```

Note: the mock runner responses must match the order of `tokio::join!` calls in `fetch_checkout_status()`: (1) upstream rev-parse, (2) origin/HEAD rev-parse, (3) git status --porcelain, (4) gh pr view. Verify the ordering if this test fails unexpectedly — the mock runner hands out responses in FIFO order, but `tokio::join!` may poll futures in any order. If the mock is order-sensitive, the test may need adjustment.

- [ ] **Step 3: Run core tests**

Run: `cargo test -p flotilla-core`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/data.rs
git add crates/flotilla-core/src/data.rs crates/flotilla-core/src/executor.rs
git commit -m "feat(core): populate uncommitted_files from git status output (#226)"
```

---

### Task 3: Update all `CheckoutStatus` construction sites in TUI

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs:520-528`
- Modify: `crates/flotilla-tui/tests/snapshots.rs:257-271`

These construct `CheckoutStatus` literals and need the new field.

- [ ] **Step 1: Update key_handlers.rs test helper**

At line ~520, add the field to the `CheckoutStatus` literal:

```rust
CheckoutStatus {
    branch: branch.to_string(),
    change_request_status: None,
    merge_commit_sha: None,
    unpushed_commits: vec![],
    has_uncommitted: false,
    uncommitted_files: vec![],
    base_detection_warning: None,
}
```

- [ ] **Step 2: Update snapshot test `delete_confirm_safe_to_delete`**

At line ~259, add the field:

```rust
flotilla_protocol::CheckoutStatus {
    branch: "feat-cleanup".into(),
    change_request_status: Some("MERGED".into()),
    merge_commit_sha: Some("abc1234".into()),
    unpushed_commits: vec![],
    has_uncommitted: false,
    uncommitted_files: vec![],
    base_detection_warning: None,
}
```

- [ ] **Step 3: Verify TUI compiles and tests pass**

Run: `cargo test -p flotilla-tui`
Expected: all pass (no rendering change yet, snapshot unchanged)

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/app/key_handlers.rs crates/flotilla-tui/tests/snapshots.rs
git commit -m "chore: add uncommitted_files field to CheckoutStatus construction sites (#226)"
```

---

### Task 4: Render file list in delete confirmation dialog

**Files:**
- Modify: `crates/flotilla-tui/src/ui.rs:774-778`

- [ ] **Step 1: Add file list rendering after the warning**

Replace lines 774-778:

```rust
// Before:
if info.has_uncommitted {
    lines.push(Line::from(Span::styled(
        "  ⚠ Has uncommitted changes",
        Style::default().fg(Color::Red).bold(),
    )));
}

// After:
if info.has_uncommitted {
    lines.push(Line::from(Span::styled(
        format!("  ⚠ {} uncommitted file(s):", info.uncommitted_files.len()),
        Style::default().fg(Color::Red).bold(),
    )));
    let max_display = 10;
    for file_line in info.uncommitted_files.iter().take(max_display) {
        lines.push(Line::from(Span::styled(
            format!("    {}", file_line),
            Style::default().fg(Color::DarkGray),
        )));
    }
    if info.uncommitted_files.len() > max_display {
        lines.push(Line::from(Span::styled(
            format!(
                "    ...and {} more",
                info.uncommitted_files.len() - max_display
            ),
            Style::default().fg(Color::DarkGray),
        )));
    }
}
```

- [ ] **Step 2: Run all tests, check for snapshot diffs**

Run: `cargo test -p flotilla-tui`
Expected: existing `delete_confirm_safe_to_delete` snapshot still passes (it has `has_uncommitted: false`, so no file list rendered)

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-tui/src/ui.rs
git commit -m "feat(ui): show uncommitted files in delete confirmation dialog (#226)"
```

---

### Task 5: Add snapshot test for dialog with uncommitted files

**Files:**
- Modify: `crates/flotilla-tui/tests/snapshots.rs`

- [ ] **Step 1: Add test with a few uncommitted files**

Add after the existing `delete_confirm_safe_to_delete` test:

```rust
#[test]
fn delete_confirm_with_uncommitted_files() {
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::DeleteConfirm {
        info: Some(flotilla_protocol::CheckoutStatus {
            branch: "feat-wip".into(),
            change_request_status: Some("OPEN".into()),
            merge_commit_sha: None,
            unpushed_commits: vec!["abc1234 work in progress".into()],
            has_uncommitted: true,
            uncommitted_files: vec![
                " M src/main.rs".into(),
                " M src/lib.rs".into(),
                "?? TODO.txt".into(),
            ],
            base_detection_warning: None,
        }),
        loading: false,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 2: Run the test to generate the snapshot**

Run: `cargo test -p flotilla-tui delete_confirm_with_uncommitted_files -- --nocapture`
Expected: first run creates a new snapshot file. Review it manually to confirm the file list appears correctly.

- [ ] **Step 3: Accept the snapshot**

Run: `cargo insta review` (or `cargo insta accept`)

- [ ] **Step 4: Add test with overflow (more than 10 files)**

```rust
#[test]
fn delete_confirm_with_many_uncommitted_files() {
    let files: Vec<String> = (0..15)
        .map(|i| format!(" M src/file_{}.rs", i))
        .collect();
    let mut harness = TestHarness::single_repo("my-project").with_mode(UiMode::DeleteConfirm {
        info: Some(flotilla_protocol::CheckoutStatus {
            branch: "feat-big-wip".into(),
            change_request_status: None,
            merge_commit_sha: None,
            unpushed_commits: vec![],
            has_uncommitted: true,
            uncommitted_files: files,
            base_detection_warning: None,
        }),
        loading: false,
    });
    let output = harness.render_to_string();
    insta::assert_snapshot!(output);
}
```

- [ ] **Step 5: Run and accept the overflow snapshot**

Run: `cargo test -p flotilla-tui delete_confirm_with_many_uncommitted_files -- --nocapture`
Then: `cargo insta review`

Verify the snapshot shows 10 files then `...and 5 more`.

- [ ] **Step 6: Run full test suite**

Run: `cargo test --workspace`
Then: `cargo clippy --all-targets --locked -- -D warnings`
Then: `cargo fmt --check`
Expected: all pass, no warnings

- [ ] **Step 7: Commit**

```bash
git add crates/flotilla-tui/tests/snapshots.rs crates/flotilla-tui/tests/snapshots/
git commit -m "test: snapshot tests for uncommitted file list in delete dialog (#226)"
```
