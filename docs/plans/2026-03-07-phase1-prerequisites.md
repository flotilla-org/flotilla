# Phase 1 Prerequisites: GhApi Trait (#74) + Injectable Config (#77)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make GitHub API calls and config I/O mockable so Phase 3 tests can run without `gh` CLI or touching the real filesystem.

**Architecture:** Extract a `GhApi` trait from the concrete `GhApiClient` struct so consumers accept `Arc<dyn GhApi>`. Add an `Option<&Path>` base-path parameter to all config functions so tests can use a `tempdir` instead of `~/.config/flotilla/`.

**Tech Stack:** Rust, async-trait, tempfile (dev-dependency, already present)

---

### Task 1: Extract `GhApi` trait

**Files:**
- Modify: `crates/flotilla-core/src/providers/github_api.rs`

**Step 1: Add the `GhApi` trait above `GhApiClient`**

Add `async_trait` import and trait definition. Place it between `parse_gh_api_response` and the `CacheEntry` struct (before line 52):

```rust
use async_trait::async_trait;
use std::path::Path;  // already imported — just confirming

#[async_trait]
pub trait GhApi: Send + Sync {
    async fn get(&self, endpoint: &str, repo_root: &Path) -> Result<String, String>;
}
```

**Step 2: Implement `GhApi` for `GhApiClient`**

Move the body of `impl GhApiClient { pub async fn get(...) }` into a trait impl block. The inherent impl block for `GhApiClient` should only keep `new()`:

```rust
impl GhApiClient {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl GhApi for GhApiClient {
    async fn get(&self, endpoint: &str, repo_root: &Path) -> Result<String, String> {
        // ... existing get() body unchanged ...
    }
}
```

**Step 3: Run tests to verify no breakage**

Run: `cargo test -p flotilla-core`
Expected: All existing tests pass (the struct's API is unchanged)

**Step 4: Commit**

```
feat: extract GhApi trait from GhApiClient (#74)
```

---

### Task 2: Update consumers to use `dyn GhApi`

**Files:**
- Modify: `crates/flotilla-core/src/providers/code_review/github.rs`
- Modify: `crates/flotilla-core/src/providers/issue_tracker/github.rs`
- Modify: `crates/flotilla-core/src/providers/discovery.rs`

**Step 1: Update `GitHubCodeReview`**

In `crates/flotilla-core/src/providers/code_review/github.rs`:

Change the import (line 1):
```rust
use crate::providers::github_api::{clamp_per_page, GhApi};
```

Change the struct field (line 10):
```rust
    api: Arc<dyn GhApi>,
```

Change the constructor (line 26):
```rust
    pub fn new(provider_name: String, repo_slug: String, api: Arc<dyn GhApi>) -> Self {
```

**Step 2: Update `GitHubIssueTracker`**

In `crates/flotilla-core/src/providers/issue_tracker/github.rs`:

Change the import (line 1):
```rust
use crate::providers::github_api::{clamp_per_page, GhApi};
```

Change the struct field (line 11):
```rust
    api: Arc<dyn GhApi>,
```

Change the constructor (line 15):
```rust
    pub fn new(provider_name: String, repo_slug: String, api: Arc<dyn GhApi>) -> Self {
```

**Step 3: Update `discovery.rs`**

In `crates/flotilla-core/src/providers/discovery.rs`:

Change the import (line 10):
```rust
use crate::providers::github_api::GhApiClient;
```
No change needed here — `GhApiClient` is still used to construct. The `Arc<GhApiClient>` auto-coerces to `Arc<dyn GhApi>` when passed to constructors that now accept `Arc<dyn GhApi>`. But to be explicit, cast at construction (line 169):

```rust
                let api: Arc<dyn crate::providers::github_api::GhApi> = Arc::new(GhApiClient::new());
```

Or simpler — just keep `let api = Arc::new(GhApiClient::new());` and let the coercion happen at the `new()` call sites. Try without the cast first.

**Step 4: Build and test**

Run: `cargo test --locked`
Expected: All tests pass. If there's a coercion error at discovery.rs line 169, add the explicit cast from Step 3.

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: Clean

**Step 5: Commit**

```
refactor: use dyn GhApi in GitHub providers (#74)
```

---

### Task 3: Make config base path injectable

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`

**Step 1: Change internal path helpers to accept optional base**

Replace the two private functions `config_dir()` (line 83) and `tab_order_file()` (line 148) plus add a new `config_base()` helper:

```rust
fn config_base(base: Option<&Path>) -> PathBuf {
    base.map(|b| b.to_path_buf())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("~"))
                .join(".config/flotilla")
        })
}

fn config_dir(base: Option<&Path>) -> PathBuf {
    config_base(base).join("repos")
}

fn tab_order_file(base: Option<&Path>) -> PathBuf {
    config_base(base).join("tab-order.json")
}
```

**Step 2: Add `base` parameter to all public functions**

Update each public function signature to accept `base: Option<&Path>` and thread it through:

`load_repos` (line 99):
```rust
pub fn load_repos(base: Option<&Path>) -> Vec<PathBuf> {
    let dir = config_dir(base);
    // ... rest unchanged
}
```

`save_repo` (line 123):
```rust
pub fn save_repo(base: Option<&Path>, path: &Path) {
    let dir = config_dir(base);
    // ... rest unchanged
}
```

`remove_repo` (line 141):
```rust
pub fn remove_repo(base: Option<&Path>, path: &Path) {
    let dir = config_dir(base);
    // ... rest unchanged
}
```

`load_tab_order` (line 155):
```rust
pub fn load_tab_order(base: Option<&Path>) -> Option<Vec<PathBuf>> {
    let content = std::fs::read_to_string(tab_order_file(base)).ok()?;
    // ... rest unchanged
}
```

`save_tab_order` (line 162):
```rust
pub fn save_tab_order(base: Option<&Path>, order: &[PathBuf]) {
    let dir = config_base(base);
    let _ = std::fs::create_dir_all(&dir);
    let paths: Vec<&str> = order.iter().filter_map(|p| p.to_str()).collect();
    if let Ok(content) = serde_json::to_string_pretty(&paths) {
        let _ = std::fs::write(tab_order_file(base), content);
    }
}
```

`load_config` (line 174):
```rust
pub fn load_config(base: Option<&Path>) -> FlotillaConfig {
    let path = config_base(base).join("config.toml");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| {
            toml::from_str(&content)
                .map_err(|e| tracing::warn!("failed to parse {}: {e}", path.display()))
                .ok()
        })
        .unwrap_or_default()
}
```

`resolve_checkouts_config` (line 189):
```rust
pub fn resolve_checkouts_config(base: Option<&Path>, repo_root: &Path) -> CheckoutsConfig {
    let global = load_config(base);
    let slug = path_to_slug(repo_root);
    let repo_file = config_dir(base).join(format!("{slug}.toml"));
    // ... rest unchanged
}
```

**Step 3: Verify it compiles in isolation**

Run: `cargo check -p flotilla-core 2>&1 | head -30`
Expected: Errors at call sites in `in_process.rs` and `discovery.rs` (they haven't been updated yet). The config module itself should compile.

**Step 4: Commit**

```
refactor: make config base path injectable (#77)
```

---

### Task 4: Update call sites to pass `None`

**Files:**
- Modify: `src/main.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/providers/discovery.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`

**Step 1: Update `src/main.rs`**

In `resolve_repo_roots` function:
- Line 285: `config::load_repos()` → `config::load_repos(None)`
- Line 286: `config::load_tab_order()` → `config::load_tab_order(None)`
- Line 324: `config::save_repo(path)` → `config::save_repo(None, path)`
- Line 326: `config::save_tab_order(&repo_roots)` → `config::save_tab_order(None, &repo_roots)`

In the mouse handler:
- Line 206: `config::save_tab_order(&app.model.repo_order)` → `config::save_tab_order(None, &app.model.repo_order)`

**Step 2: Update `crates/flotilla-core/src/in_process.rs`**

- Line 280: `config::save_repo(&path)` → `config::save_repo(None, &path)`
- Line 282: `config::save_tab_order(&order)` → `config::save_tab_order(None, &order)`
- Line 305: `config::remove_repo(&path)` → `config::remove_repo(None, &path)`
- Line 307: `config::save_tab_order(&order)` → `config::save_tab_order(None, &order)`

**Step 3: Update `crates/flotilla-core/src/providers/discovery.rs`**

- Line 121: `config::resolve_checkouts_config(repo_root)` → `config::resolve_checkouts_config(None, repo_root)`

**Step 4: Update `crates/flotilla-tui/src/app/mod.rs`**

- Line 479: `flotilla_core::config::save_tab_order(&self.model.repo_order)` → `flotilla_core::config::save_tab_order(None, &self.model.repo_order)`
- Line 484: same change

**Step 5: Build and test**

Run: `cargo test --locked`
Expected: All tests pass

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: Clean

**Step 6: Commit**

```
refactor: pass None base path at all config call sites (#77)
```

---

### Task 5: Add config tests using tempdir

**Files:**
- Modify: `crates/flotilla-core/src/config.rs` (add `#[cfg(test)] mod tests`)

**Step 1: Write tests for save/load repo roundtrip**

Add at end of `config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn path_to_slug_strips_leading_slash() {
        let slug = path_to_slug(Path::new("/Users/alice/dev/myrepo"));
        assert_eq!(slug, "users-alice-dev-myrepo");
    }

    #[test]
    fn save_and_load_repos_roundtrip() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let repo = PathBuf::from(base.join("fake-repo"));
        std::fs::create_dir_all(&repo).unwrap();

        save_repo(Some(base), &repo);
        let repos = load_repos(Some(base));
        assert_eq!(repos, vec![repo]);
    }

    #[test]
    fn save_repo_is_idempotent() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let repo = PathBuf::from(base.join("repo"));
        std::fs::create_dir_all(&repo).unwrap();

        save_repo(Some(base), &repo);
        save_repo(Some(base), &repo);
        let repos = load_repos(Some(base));
        assert_eq!(repos.len(), 1);
    }

    #[test]
    fn remove_repo_deletes_config() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let repo = PathBuf::from(base.join("repo"));
        std::fs::create_dir_all(&repo).unwrap();

        save_repo(Some(base), &repo);
        assert_eq!(load_repos(Some(base)).len(), 1);

        remove_repo(Some(base), &repo);
        assert_eq!(load_repos(Some(base)).len(), 0);
    }

    #[test]
    fn save_and_load_tab_order_roundtrip() {
        let dir = tempdir().unwrap();
        let base = dir.path();

        let order = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        save_tab_order(Some(base), &order);
        let loaded = load_tab_order(Some(base)).unwrap();
        assert_eq!(loaded, order);
    }

    #[test]
    fn load_tab_order_returns_none_when_missing() {
        let dir = tempdir().unwrap();
        assert!(load_tab_order(Some(dir.path())).is_none());
    }

    #[test]
    fn load_repos_returns_empty_when_dir_missing() {
        let dir = tempdir().unwrap();
        let repos = load_repos(Some(dir.path()));
        assert!(repos.is_empty());
    }

    #[test]
    fn load_config_returns_defaults_when_missing() {
        let dir = tempdir().unwrap();
        let cfg = load_config(Some(dir.path()));
        assert_eq!(cfg.vcs.git.checkouts.provider, "auto");
    }

    #[test]
    fn resolve_checkouts_config_uses_global_defaults() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();

        let co = resolve_checkouts_config(Some(base), &repo);
        assert_eq!(co.provider, "auto");
        assert!(co.path.contains("{{ repo_path }}"));
    }
}
```

**Step 2: Run the tests**

Run: `cargo test -p flotilla-core config::tests -- --nocapture`
Expected: All 8 tests pass

**Step 3: Commit**

```
test: add config roundtrip tests with injectable base path (#77)
```

---

### Task 6: Final verification

**Step 1: Full test suite**

Run: `cargo test --locked`
Expected: All tests pass

**Step 2: Clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: Clean

**Step 3: Format**

Run: `cargo fmt`

**Step 4: Remove `#[allow(dead_code)]` from `remove_repo`**

In `config.rs`, the `remove_repo` function had `#[allow(dead_code)]` — it's now used in tests, so the attribute can be removed. (Check if it was only there because it had no callers.)

Actually, `remove_repo` is called in `in_process.rs:305` so the `#[allow(dead_code)]` is likely stale. Remove it if clippy doesn't complain without it.

**Step 5: Commit any cleanup**

```
chore: final cleanup for phase 1 prerequisites
```
