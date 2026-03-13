# Test Infrastructure Deduplication — Design Spec

## Goal

Extract three duplicated test infrastructure patterns (A7, A8, A9 from issue #225) into shared modules, removing duplicated test helper code across 9 provider test modules.

## Architecture

All changes are `#[cfg(test)]` only — no production code affected. New shared helpers use `#[cfg(test)] pub(crate)` visibility, following the existing `checkout_test_support` and `testing` module patterns in the codebase.

## Components

### A9: VCS `git()` helper consolidation

**Problem:** `git()` (run-git-and-panic-on-failure) is defined identically in `vcs/git.rs` and `vcs/wt.rs`, while `checkout_test_support::git()` already exists in `vcs/mod.rs`. The local copies use `.unwrap()` while the shared version uses `.expect("failed to spawn git")` — the shared version is better per project conventions.

**Solution:** Delete the local `git()` functions from `git.rs` and `wt.rs`. Update all call sites to use `super::checkout_test_support::git()`. `wt.rs` already does this in some tests, proving the migration path works.

**Files:**
- Modify: `crates/flotilla-core/src/providers/vcs/git.rs` — remove local `git()`, update calls
- Modify: `crates/flotilla-core/src/providers/vcs/wt.rs` — remove local `git()`, update calls

### A8: GitHub test helpers

**Problem:** `repo_root_for_recording()` and `build_api_and_runner()` are character-identical between `code_review/github.rs` and `issue_tracker/github.rs` test modules.

**Solution:** Create a `#[cfg(test)] pub(crate) mod github_test_support` in `crates/flotilla-core/src/providers/mod.rs` (following the `checkout_test_support` precedent). Move both functions there. Both GitHub test modules import from the shared location.

**Files:**
- Modify: `crates/flotilla-core/src/providers/mod.rs` — add `github_test_support` module
- Modify: `crates/flotilla-core/src/providers/code_review/github.rs` — remove local helpers, import shared
- Modify: `crates/flotilla-core/src/providers/issue_tracker/github.rs` — remove local helpers, import shared

### A7: Provider fixture path helper

**Problem:** `fixture()` is repeated 9 times across provider test modules, differing only in the subdirectory name:
- `vcs/git.rs`, `vcs/wt.rs`, `vcs/git_worktree.rs`
- `code_review/github.rs`, `issue_tracker/github.rs`
- `coding_agent/claude.rs`, `coding_agent/codex.rs`
- `workspace/tmux.rs`, `workspace/zellij.rs`

(`coding_agent/cursor.rs` has no fixture tests.)

**Solution:** Add a `fixture_path` function to the existing `#[cfg(test)] pub(crate) mod testing` in `providers/mod.rs`:

```rust
pub fn fixture_path(provider_dir: &str, name: &str) -> String {
    format!("{}/src/providers/{}/fixtures/{}", env!("CARGO_MANIFEST_DIR"), provider_dir, name)
}
```

Each test module keeps a local one-liner wrapper for call-site ergonomics:

```rust
fn fixture(name: &str) -> String {
    crate::providers::testing::fixture_path("code_review", name)
}
```

**Files:**
- Modify: `crates/flotilla-core/src/providers/mod.rs` — add `fixture_path` to `testing` module
- Modify: 9 provider test modules — replace `fixture()` bodies with delegation

## Testing

All changes are in test infrastructure. Verification: `cargo test --locked` passes with no regressions.

## Non-goals

- No changes to production code
- No new test coverage (this is test infrastructure refactoring)
- No changes to fixture file locations
