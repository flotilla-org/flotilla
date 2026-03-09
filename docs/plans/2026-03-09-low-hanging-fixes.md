# Low-Hanging Fixes Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fix four small issues: #137 (slug spaces), #144 (inject_issues optimization), #130 (hardcoded "github"), #148 (daemon startup panic).

**Architecture:** Four independent fixes across config, in_process, executor, and main.rs. No cross-cutting concerns.

**Tech Stack:** Rust, tokio, color_eyre

---

### Task 1: #137 — Sanitize spaces in `path_to_slug`

**Files:**
- Modify: `crates/flotilla-core/src/config.rs:92-98` (implementation)
- Modify: `crates/flotilla-core/src/config.rs:253-257` (existing test)

**Step 1: Add a failing test for spaces in paths**

In `config.rs` test module, add after the existing `path_to_slug_strips_leading_slash` test:

```rust
#[test]
fn path_to_slug_sanitizes_spaces() {
    let slug = path_to_slug(Path::new("/Users/Bob Smith/my repo"));
    assert_eq!(slug, "users-bob-smith-my-repo");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core path_to_slug_sanitizes_spaces`
Expected: FAIL — spaces are preserved in slug

**Step 3: Fix `path_to_slug` to replace spaces**

Change the function at line 92:

```rust
pub fn path_to_slug(path: &Path) -> String {
    path.to_string_lossy()
        .to_lowercase()
        .replace('/', "-")
        .replace(' ', "-")
        .trim_start_matches('-')
        .to_string()
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core path_to_slug`
Expected: both slug tests PASS

**Step 5: Commit**

```bash
git add crates/flotilla-core/src/config.rs
git commit -m "fix: sanitize spaces in path_to_slug (#137)"
```

---

### Task 2: #144 — Short-circuit `inject_issues` when cache empty

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs:49-61` (inject_issues function)

**Step 1: Write failing test**

Add a test module at the bottom of `in_process.rs` (or in the existing test module if one exists):

```rust
#[cfg(test)]
mod inject_issues_tests {
    use super::*;
    use flotilla_protocol::provider_data::ProviderData;

    #[test]
    fn inject_issues_empty_cache_clears_base_issues() {
        let mut base = ProviderData::default();
        base.issues.insert("stale".into(), Issue {
            id: "stale".into(),
            title: "Stale".into(),
            labels: vec![],
            association_keys: vec![],
        });
        let cache = IssueCache::new();
        let result = inject_issues(&base, &cache, &None);
        assert!(result.issues.is_empty());
    }
}
```

**Step 2: Run test to verify it passes (baseline)**

Run: `cargo test -p flotilla-core inject_issues_empty_cache`
Expected: PASS (current code already clones empty map from cache)

**Step 3: Apply the optimization**

Replace the `inject_issues` function body (lines 49-61):

```rust
fn inject_issues(
    base_providers: &ProviderData,
    cache: &IssueCache,
    search_results: &Option<Vec<Issue>>,
) -> ProviderData {
    let mut providers = base_providers.clone();
    if let Some(ref results) = search_results {
        providers.issues = results.iter().map(|i| (i.id.clone(), i.clone())).collect();
    } else if !cache.is_empty() {
        providers.issues = (*cache.to_index_map()).clone();
    } else {
        providers.issues.clear();
    }
    providers
}
```

Key change: when no search results and cache is empty, just clear the existing issues instead of cloning an empty IndexMap from the Arc.

**Step 4: Run tests**

Run: `cargo test -p flotilla-core inject_issues`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs
git commit -m "fix: avoid redundant Arc clone in inject_issues when cache empty (#144)"
```

---

### Task 3: #130 — Replace hardcoded "github" fallback in `generate_branch_name`

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs:238`

**Step 1: Fix the hardcoded string**

Change line 238 from:

```rust
.unwrap_or_else(|| "github".to_string());
```

to:

```rust
.unwrap_or_else(|| "issues".to_string());
```

**Step 2: Run all tests**

Run: `cargo test -p flotilla-core`
Expected: PASS (no existing tests reference "github" as tracker name in this path)

**Step 3: Commit**

```bash
git add crates/flotilla-core/src/executor.rs
git commit -m "fix: use generic fallback instead of hardcoded 'github' in branch name generation (#130)"
```

---

### Task 4: #148 — Graceful recovery from daemon startup timeout

**Files:**
- Modify: `src/main.rs:124-140`

**Step 1: Save config_dir before the spawn**

Before the `daemon_task` spawn (before line 124), capture the config dir for use in error messages:

```rust
let daemon_log_path = cli.config_dir().join("daemon.log");
```

**Step 2: Change spawn to return Result instead of panicking**

Replace lines 124-137:

```rust
let daemon_task = tokio::spawn(async move {
    let daemon: Result<Arc<dyn DaemonHandle>, String> = if embedded {
        let d = InProcessDaemon::new(repo_roots, config_clone).await;
        Ok(d as Arc<dyn DaemonHandle>)
    } else {
        let socket_path = cli.socket_path();
        connect_or_spawn(&socket_path, &cli)
            .await
            .map(|d| d as Arc<dyn DaemonHandle>)
    };
    if let Ok(ref d) = daemon {
        let _ = d; // logged below
    }
    info!("daemon init completed in {:.0?}", startup.elapsed());
    daemon
});
```

**Step 3: Handle errors gracefully after splash**

Replace line 140 (`let daemon = daemon_task.await.expect(...)`) with:

```rust
let daemon = match daemon_task.await {
    Ok(Ok(daemon)) => {
        info!("daemon ready in {:.0?}", startup.elapsed());
        daemon
    }
    Ok(Err(e)) => {
        ratatui::restore();
        eprintln!("Error: {e}");
        eprintln!("  Check daemon log at {}", daemon_log_path.display());
        std::process::exit(1);
    }
    Err(e) => {
        ratatui::restore();
        eprintln!("Error: daemon initialization panicked: {e}");
        std::process::exit(1);
    }
};
```

**Step 4: Verify it compiles and runs**

Run: `cargo build`
Expected: compiles cleanly

**Step 5: Commit**

```bash
git add src/main.rs
git commit -m "fix: graceful recovery from daemon startup timeout instead of panic (#148)"
```

---

### Task 5: Final verification

**Step 1: Run full test suite and lints**

```bash
cargo fmt
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Expected: all pass, no warnings
