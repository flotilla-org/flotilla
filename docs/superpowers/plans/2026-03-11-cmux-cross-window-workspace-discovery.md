# cmux Cross-Window Workspace Discovery Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the cmux workspace manager discover workspaces across all cmux windows so Flotilla keeps correlating moved workspaces and stops attempting duplicate workspace creation.

**Architecture:** Keep the fix inside `crates/flotilla-core/src/providers/workspace/cmux.rs`. Add small helpers to parse `list-windows` output and per-window `list-workspaces` responses, then have `list_workspaces()` aggregate results across windows with partial-failure handling. Leave public workspace identity and downstream refresh/correlation flows unchanged.

**Tech Stack:** Rust, `serde_json`, `async_trait`, existing `MockRunner`-based unit tests, `cargo test`

**Spec:** `docs/superpowers/specs/2026-03-11-cmux-cross-window-workspace-discovery-design.md`

**Status:** Implemented in PR #248 (`codex/cmux-cross-window-workspace-discovery`).

---

## Chunk 1: Parser And Aggregation Changes

### Task 1: Add failing tests for multi-window workspace discovery

**Files:**
- Modify: `crates/flotilla-core/src/providers/workspace/cmux.rs`
- Test: `crates/flotilla-core/src/providers/workspace/cmux.rs`

- [ ] **Step 1: Write the failing test for multi-window aggregation**

Add a unit test in `crates/flotilla-core/src/providers/workspace/cmux.rs` that scripts `MockRunner` responses in this order:

1. `cmux --json list-windows`
2. `cmux --json list-workspaces --window window:1`
3. `cmux --json list-workspaces --window window:2`

Use fixture-like inline JSON so the test asserts:

- both workspaces are returned
- workspace refs are preserved
- directories become `CorrelationKey::CheckoutPath`

Suggested assertion shape:

```rust
#[tokio::test]
async fn list_workspaces_aggregates_all_windows() {
    let manager = CmuxWorkspaceManager::new(Arc::new(MockRunner::new(vec![
        Ok(r#"{"windows":[{"ref":"window:1"},{"ref":"window:2"}]}"#.to_string()),
        Ok(r#"{"workspaces":[{"ref":"workspace:10","title":"Main","directories":["/tmp/repo-a"]}]}"#.to_string()),
        Ok(r#"{"workspaces":[{"ref":"workspace:11","title":"Feature","directories":["/tmp/repo-b"]}]}"#.to_string()),
    ])));

    let workspaces = manager.list_workspaces().await.expect("list workspaces");
    assert_eq!(workspaces.len(), 2);
}
```

- [ ] **Step 2: Run the targeted test and verify it fails**

Run:

```bash
cargo test --locked -p flotilla-core providers::workspace::cmux::tests::list_workspaces_aggregates_all_windows
```

Expected: FAIL because `list_workspaces()` still performs only one `list-workspaces` call.

- [ ] **Step 3: Write the failing test for partial window failure**

Add a second test that returns:

1. successful `list-windows`
2. successful `list-workspaces --window window:1`
3. failing `list-workspaces --window window:2`

Assert that:

- `list_workspaces()` succeeds
- only the successful window's workspace is returned

- [ ] **Step 4: Run the targeted test and verify it fails**

Run:

```bash
cargo test --locked -p flotilla-core providers::workspace::cmux::tests::list_workspaces_skips_failed_window
```

Expected: FAIL until partial-failure handling is implemented.

- [ ] **Step 5: Write the failing test for top-level window-list failure**

Add a test where the first `list-windows` call returns `Err("cmux unavailable".into())` and assert that `list_workspaces()` returns an error.

- [ ] **Step 6: Run the targeted test and verify it fails**

Run:

```bash
cargo test --locked -p flotilla-core providers::workspace::cmux::tests::list_workspaces_fails_when_window_listing_fails
```

Expected: FAIL until `list_workspaces()` uses `list-windows`.

### Task 2: Extract parsing helpers and implement global workspace aggregation

**Files:**
- Modify: `crates/flotilla-core/src/providers/workspace/cmux.rs`
- Test: `crates/flotilla-core/src/providers/workspace/cmux.rs`

- [ ] **Step 1: Add a helper to parse window refs**

In `crates/flotilla-core/src/providers/workspace/cmux.rs`, add a private helper with a narrow contract:

```rust
fn parse_window_refs(output: &str) -> Result<Vec<String>, String>
```

Implementation requirements:

- parse JSON with `serde_json::Value`
- read `windows` as an array
- extract each `ref` string
- return an error if the `windows` array is missing

- [ ] **Step 2: Add a helper to parse workspaces from JSON**

Refactor the current JSON parsing from `list_workspaces()` into a reusable helper:

```rust
fn parse_workspaces(output: &str) -> Result<Vec<(String, Workspace)>, String>
```

Keep existing behavior for:

- `Workspace.name`
- `Workspace.directories`
- `CorrelationKey::CheckoutPath`

- [ ] **Step 3: Update `list_workspaces()` to enumerate all windows**

Replace the single `cmux --json list-workspaces` call with:

1. `self.cmux_cmd(&["--json", "list-windows"])`
2. `parse_window_refs(...)`
3. one `self.cmux_cmd(&["--json", "list-workspaces", "--window", window_ref])` per window
4. `parse_workspaces(...)` on each successful response

Use a local `Vec<(String, Workspace)>` accumulator and a `HashSet<String>` or `HashMap<String, Workspace>` to dedupe by workspace ref.

- [ ] **Step 4: Handle per-window failures without failing the whole method**

For each window:

- if the command fails, log with `tracing::warn!` and continue
- if parsing fails, log with `tracing::warn!` and continue

Only fail the whole method when `list-windows` itself fails or its top-level JSON is invalid.

- [ ] **Step 5: Run the focused cmux tests**

Run:

```bash
cargo test --locked -p flotilla-core providers::workspace::cmux::tests
```

Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/workspace/cmux.rs
git commit -m "fix: discover cmux workspaces across windows"
```

## Chunk 2: Defensive Coverage And Workspace Verification

### Task 3: Add duplicate-ref coverage and preserve existing behavior

**Files:**
- Modify: `crates/flotilla-core/src/providers/workspace/cmux.rs`
- Test: `crates/flotilla-core/src/providers/workspace/cmux.rs`

- [ ] **Step 1: Write the failing duplicate-ref test**

Add a test where two windows both report the same `workspace:10` ref. Assert that the returned list contains only one entry for that ref.

- [ ] **Step 2: Run the targeted test and verify it fails**

Run:

```bash
cargo test --locked -p flotilla-core providers::workspace::cmux::tests::list_workspaces_dedupes_duplicate_workspace_refs
```

Expected: FAIL until dedupe logic is present.

- [ ] **Step 3: Implement the minimal dedupe logic**

Use the accumulator from Task 2 so duplicate refs are ignored after the first successful parse. Do not change the public return type.

- [ ] **Step 4: Re-run the cmux tests**

Run:

```bash
cargo test --locked -p flotilla-core providers::workspace::cmux::tests
```

Expected: PASS

- [ ] **Step 5: Sanity-check `select_workspace()` remains unchanged**

Read `crates/flotilla-core/src/providers/workspace/cmux.rs` and confirm `select_workspace()` still delegates to:

```rust
self.cmux_cmd(&["select-workspace", "--workspace", ws_ref]).await?;
```

Do not expand scope unless a failing test demonstrates a real requirement.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/providers/workspace/cmux.rs
git commit -m "test: cover cmux cross-window workspace edge cases"
```

## Chunk 3: Final Verification

### Task 4: Run repository verification and review the diff

**Files:**
- Modify: none
- Test: workspace

- [ ] **Step 1: Run the sandbox-safe workspace tests**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: PASS

- [ ] **Step 2: Review the diff**

Run:

```bash
git diff --stat HEAD~2..HEAD
```

Expected: only the intended cmux provider file and related docs/plan changes appear.

- [ ] **Step 3: Commit the plan document if desired**

If this plan file should be preserved in git with the implementation work, commit it alongside the finished code changes:

```bash
git add docs/superpowers/plans/2026-03-11-cmux-cross-window-workspace-discovery.md
git commit -m "docs: add cmux cross-window workspace discovery plan"
```
