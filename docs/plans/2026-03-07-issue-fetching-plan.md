# Issue Fetching Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace hardcoded issue fetching with a daemon-owned cache supporting pagination, linked-issue pinning, and search.

**Architecture:** The daemon owns an `IssueCache` per repo. The TUI reports viewport size and requests more issues on scroll. The `IssueTracker` trait gains page-based fetching, batch-by-ID fetching, and search. Linked issues are always pinned in cache. Search hits the GitHub search API server-side.

**Tech Stack:** Rust, async-trait, tokio, serde, gh CLI (via GhApiClient)

---

### Task 1: Protocol types — IssuePage and new Command variants

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs:6-56`
- Modify: `crates/flotilla-protocol/src/snapshot.rs:55-63`
- Modify: `crates/flotilla-protocol/src/provider_data.rs` (add IssuePage)
- Modify: `crates/flotilla-protocol/src/lib.rs` (re-exports)
- Test: `cargo test -p flotilla-protocol`

**Step 1: Add IssuePage to provider_data.rs**

After the `Issue` struct (line 80), add:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuePage {
    pub issues: Vec<Issue>,
    pub total_count: Option<u32>,
    pub has_more: bool,
}
```

Re-export `IssuePage` from `lib.rs` alongside the existing `Issue` re-export.

**Step 2: Add new Command variants**

In `commands.rs`, add to the `Command` enum (after `Refresh`):

```rust
SetIssueViewport { repo: PathBuf, visible_count: usize },
FetchMoreIssues { repo: PathBuf, desired_count: usize },
SearchIssues { repo: PathBuf, query: String },
ClearIssueSearch { repo: PathBuf },
```

**Step 3: Add issue metadata fields to Snapshot**

In `snapshot.rs`, add to the `Snapshot` struct:

```rust
pub issue_total: Option<u32>,
pub issue_has_more: bool,
pub issue_search_results: Option<Vec<Issue>>,
```

**Step 4: Build and test**

Run: `cargo build -p flotilla-protocol && cargo test -p flotilla-protocol`

Fix any downstream compilation errors in other crates (the new `Snapshot` fields need defaults where snapshots are constructed — `convert.rs`, tests).

**Step 5: Commit**

```
feat: add protocol types for issue pagination and search
```

---

### Task 2: IssueTracker trait — new methods with default impls

**Files:**
- Modify: `crates/flotilla-core/src/providers/issue_tracker/mod.rs:7-22`
- Test: `cargo build -p flotilla-core`

**Step 1: Add new methods to IssueTracker trait**

Add default implementations so existing providers compile without changes:

```rust
async fn list_issues_page(
    &self,
    repo_root: &Path,
    page: u32,
    per_page: usize,
) -> Result<IssuePage, String> {
    // Default: delegate to list_issues for page 1 only
    if page > 1 {
        return Ok(IssuePage { issues: vec![], total_count: None, has_more: false });
    }
    let issues = self.list_issues(repo_root, per_page).await?;
    let has_more = issues.len() >= per_page;
    Ok(IssuePage { issues, total_count: None, has_more })
}

async fn fetch_issues_by_id(
    &self,
    _repo_root: &Path,
    _ids: &[String],
) -> Result<Vec<Issue>, String> {
    Ok(vec![])
}

async fn search_issues(
    &self,
    _repo_root: &Path,
    _query: &str,
    _limit: usize,
) -> Result<Vec<Issue>, String> {
    Ok(vec![])
}
```

Add `use crate::providers::types::IssuePage;` import (IssuePage is re-exported via flotilla-protocol types).

**Step 2: Build**

Run: `cargo build -p flotilla-core`

**Step 3: Commit**

```
feat: add pagination, batch-fetch, and search to IssueTracker trait
```

---

### Task 3: GhApiClient — parse Link header for pagination

**Files:**
- Modify: `crates/flotilla-core/src/providers/github_api.rs:18-50`
- Test: `crates/flotilla-core/src/providers/github_api.rs` (tests module)

**Step 1: Write failing tests for Link header parsing**

In the `tests` module of `github_api.rs`:

```rust
#[test]
fn parse_link_header_has_next() {
    let raw = "HTTP/2.0 200 OK\r\nLink: <https://api.github.com/repos/foo/bar/issues?page=2>; rel=\"next\", <https://api.github.com/repos/foo/bar/issues?page=5>; rel=\"last\"\r\nEtag: \"abc\"\r\n\r\n[{\"number\":1}]";
    let result = parse_gh_api_response(raw);
    assert!(result.has_next_page);
    assert_eq!(result.total_count, None);
}

#[test]
fn parse_link_header_no_next() {
    let raw = "HTTP/2.0 200 OK\r\nLink: <https://api.github.com/repos/foo/bar/issues?page=3>; rel=\"prev\"\r\nEtag: \"abc\"\r\n\r\n[]";
    let result = parse_gh_api_response(raw);
    assert!(!result.has_next_page);
}

#[test]
fn parse_no_link_header() {
    let raw = "HTTP/2.0 200 OK\r\nEtag: \"abc\"\r\n\r\n[]";
    let result = parse_gh_api_response(raw);
    assert!(!result.has_next_page);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core -- github_api::tests::parse_link`

**Step 3: Add has_next_page field to GhApiResponse and parse it**

Add `pub has_next_page: bool` to `GhApiResponse`.

In `parse_gh_api_response`, add Link header parsing alongside the ETag parsing:

```rust
let mut has_next_page = false;

// In the header parsing loop, add:
} else if line.len() >= 6 && line[..5].eq_ignore_ascii_case("link:") {
    has_next_page = line.contains("rel=\"next\"");
}
```

Update the `GhApiResponse` construction to include `has_next_page`.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core -- github_api::tests`

**Step 5: Commit**

```
feat: parse Link header for pagination in GhApiClient
```

---

### Task 4: GitHub IssueTracker — implement new trait methods

**Files:**
- Modify: `crates/flotilla-core/src/providers/issue_tracker/github.rs`
- Test: `crates/flotilla-core/src/providers/issue_tracker/github.rs` (tests module)

**Step 1: Implement list_issues_page**

Add to the `impl IssueTracker for GitHubIssueTracker`:

```rust
async fn list_issues_page(
    &self,
    repo_root: &Path,
    page: u32,
    per_page: usize,
) -> Result<IssuePage, String> {
    let per_page = clamp_per_page(per_page);
    let endpoint = format!(
        "repos/{}/issues?state=open&per_page={}&page={}",
        self.repo_slug, per_page, page
    );
    let response = self.api.get_with_headers(&endpoint, repo_root).await?;
    let items: Vec<serde_json::Value> =
        serde_json::from_str(&response.body).map_err(|e| e.to_string())?;

    let issues: Vec<Issue> = items
        .into_iter()
        .filter(|v| !v.as_object().map(|o| o.contains_key("pull_request")).unwrap_or(false))
        .filter_map(|v| parse_issue(&self.provider_name, &v))
        .collect();

    Ok(IssuePage {
        issues,
        total_count: None,
        has_more: response.has_next_page,
    })
}
```

This requires:
- Extracting the existing issue parsing into a `parse_issue(provider_name, value) -> Option<Issue>` helper function (refactor from `list_issues`)
- Adding `get_with_headers` to `GhApiClient` that returns `GhApiResponse` instead of just the body string (or modifying `get` to return it — the existing `list_issues` can be updated to call `list_issues_page(1, limit)`)

**Step 2: Implement fetch_issues_by_id**

```rust
async fn fetch_issues_by_id(
    &self,
    repo_root: &Path,
    ids: &[String],
) -> Result<Vec<Issue>, String> {
    let futs: Vec<_> = ids.iter().map(|id| {
        let endpoint = format!("repos/{}/issues/{}", self.repo_slug, id);
        let api = Arc::clone(&self.api);
        let repo_root = repo_root.to_path_buf();
        let provider_name = self.provider_name.clone();
        async move {
            let body = api.get(&endpoint, &repo_root).await?;
            let v: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
            parse_issue(&provider_name, &v).ok_or_else(|| format!("failed to parse issue {}", id))
        }
    }).collect();

    let results = futures::future::join_all(futs).await;
    let mut issues = Vec::new();
    for result in results {
        match result {
            Ok(issue) => issues.push(issue),
            Err(e) => tracing::warn!("failed to fetch issue: {}", e),
        }
    }
    Ok(issues)
}
```

Add `futures` to `flotilla-core` Cargo.toml dependencies (or use `tokio::join!` with a bounded set).

**Step 3: Implement search_issues**

```rust
async fn search_issues(
    &self,
    repo_root: &Path,
    query: &str,
    limit: usize,
) -> Result<Vec<Issue>, String> {
    let per_page = clamp_per_page(limit);
    let encoded_query = urlencoding::encode(
        &format!("repo:{} is:issue is:open {}", self.repo_slug, query)
    );
    let endpoint = format!("search/issues?q={}&per_page={}", encoded_query, per_page);
    let body = self.api.get(&endpoint, repo_root).await?;
    let response: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;

    let items = response["items"].as_array().ok_or("no items array")?;
    Ok(items.iter().filter_map(|v| parse_issue(&self.provider_name, v)).collect())
}
```

Add `urlencoding` to `flotilla-core` dependencies.

**Step 4: Refactor list_issues to use list_issues_page**

```rust
async fn list_issues(&self, repo_root: &Path, limit: usize) -> Result<Vec<Issue>, String> {
    let page = self.list_issues_page(repo_root, 1, limit).await?;
    Ok(page.issues)
}
```

**Step 5: Add GhApiClient::get_with_headers method**

In `github_api.rs`, add a method that returns the full `GhApiResponse` instead of just the body:

```rust
pub async fn get_with_headers(&self, endpoint: &str, repo_root: &Path) -> Result<GhApiResponse, String> {
    // Same as get(), but returns the parsed GhApiResponse instead of just body
    // Still handles caching via ETag
}
```

Refactor `get` to delegate to `get_with_headers` and return `.body`.

**Step 6: Build and test**

Run: `cargo build -p flotilla-core && cargo test -p flotilla-core`

**Step 7: Commit**

```
feat: implement paginated, batch, and search issue fetching for GitHub
```

---

### Task 5: IssueCache struct

**Files:**
- Create: `crates/flotilla-core/src/issue_cache.rs`
- Modify: `crates/flotilla-core/src/lib.rs` (add `pub mod issue_cache;`)
- Test: unit tests in `issue_cache.rs`

**Step 1: Write tests for IssueCache**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {}", id),
            labels: vec![],
            association_keys: vec![],
        }
    }

    #[test]
    fn merge_page_appends_issues() {
        let mut cache = IssueCache::new();
        let page = IssuePage {
            issues: vec![issue("1"), issue("2")],
            total_count: Some(10),
            has_more: true,
        };
        cache.merge_page(page);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.total_count, Some(10));
        assert!(cache.has_more);
    }

    #[test]
    fn pin_issues_marks_as_pinned() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1"), issue("2")],
            total_count: None,
            has_more: false,
        });
        cache.pin(&["1".to_string()]);
        assert!(cache.pinned.contains("1"));
        assert!(!cache.pinned.contains("2"));
    }

    #[test]
    fn missing_ids_returns_unpinned_absent_ids() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1")],
            total_count: None,
            has_more: false,
        });
        let missing = cache.missing_ids(&["1".to_string(), "3".to_string(), "5".to_string()]);
        assert_eq!(missing, vec!["3", "5"]);
    }

    #[test]
    fn add_pinned_inserts_and_pins() {
        let mut cache = IssueCache::new();
        cache.add_pinned(vec![issue("99")]);
        assert!(cache.entries.contains_key("99"));
        assert!(cache.pinned.contains("99"));
    }

    #[test]
    fn to_index_map_returns_all_entries() {
        let mut cache = IssueCache::new();
        cache.merge_page(IssuePage {
            issues: vec![issue("1"), issue("2")],
            total_count: None,
            has_more: false,
        });
        cache.add_pinned(vec![issue("99")]);
        let map = cache.to_index_map();
        assert_eq!(map.len(), 3);
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core -- issue_cache`

**Step 3: Implement IssueCache**

```rust
use std::collections::HashSet;
use indexmap::IndexMap;
use flotilla_protocol::{Issue, IssuePage};

pub struct IssueCache {
    pub entries: IndexMap<String, Issue>,
    pub next_page: u32,
    pub has_more: bool,
    pub pinned: HashSet<String>,
    pub total_count: Option<u32>,
}

impl IssueCache {
    pub fn new() -> Self {
        Self {
            entries: IndexMap::new(),
            next_page: 1,
            has_more: true,
            pinned: HashSet::new(),
            total_count: None,
        }
    }

    pub fn merge_page(&mut self, page: IssuePage) {
        for issue in page.issues {
            self.entries.insert(issue.id.clone(), issue);
        }
        self.next_page += 1;
        self.has_more = page.has_more;
        if page.total_count.is_some() {
            self.total_count = page.total_count;
        }
    }

    pub fn pin(&mut self, ids: &[String]) {
        for id in ids {
            self.pinned.insert(id.clone());
        }
    }

    pub fn missing_ids(&self, ids: &[String]) -> Vec<String> {
        ids.iter()
            .filter(|id| !self.entries.contains_key(id.as_str()))
            .cloned()
            .collect()
    }

    pub fn add_pinned(&mut self, issues: Vec<Issue>) {
        for issue in issues {
            self.pinned.insert(issue.id.clone());
            self.entries.insert(issue.id.clone(), issue);
        }
    }

    pub fn to_index_map(&self) -> IndexMap<String, Issue> {
        self.entries.clone()
    }
}
```

**Step 4: Run tests**

Run: `cargo test -p flotilla-core -- issue_cache`

**Step 5: Commit**

```
feat: add IssueCache for daemon-owned issue pagination
```

---

### Task 6: Refresh changes — remove issue fetching from refresh loop

**Files:**
- Modify: `crates/flotilla-core/src/refresh.rs:143-152, 218-228`
- Test: `cargo test -p flotilla-core`

**Step 1: Remove issue fetching from refresh_providers**

In `refresh_providers` (refresh.rs):
- Remove the `issues_fut` block (lines 143-152)
- Remove the `issues` join arm from the `tokio::join!` call
- Remove the `pd.issues = ...` block (lines 218-228)
- Remove issue-related error handling from `provider_health`
- Keep `pd.issues` as an empty IndexMap — the daemon will populate it from the IssueCache before building snapshots

The `skip_issues` flag and its `Arc<AtomicBool>` can remain for now (the daemon may still use it to signal whether to fetch issues at all).

**Step 2: Build and test**

Run: `cargo build -p flotilla-core && cargo test -p flotilla-core`

Some tests may need updating if they depend on issues appearing in refresh snapshots.

**Step 3: Commit**

```
refactor: remove issue fetching from refresh loop

Issues are now managed by the daemon's IssueCache, not the
periodic refresh cycle.
```

---

### Task 7: Daemon integration — wire IssueCache into InProcessDaemon

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/executor.rs:20-25` (or handle new commands in daemon directly)
- Modify: `crates/flotilla-core/src/convert.rs` (populate new Snapshot fields)
- Test: `cargo build && cargo test`

This is the largest task. It wires everything together.

**Step 1: Add IssueCache to RepoState**

In `in_process.rs`, add to `RepoState`:

```rust
issue_cache: IssueCache,
```

Initialize it in `InProcessDaemon::new()` with `IssueCache::new()`.

**Step 2: Handle new commands in execute()**

The new issue commands don't go through the executor — they're daemon-level operations (like `AddRepo`/`RemoveRepo`). Handle them directly in `InProcessDaemon::execute()`:

```rust
Command::SetIssueViewport { repo, visible_count } => {
    self.ensure_issues_cached(&repo, visible_count * 2).await?;
    Ok(CommandResult::Ok)
}
Command::FetchMoreIssues { repo, desired_count } => {
    self.ensure_issues_cached(&repo, desired_count).await?;
    Ok(CommandResult::Ok)
}
Command::SearchIssues { repo, query } => {
    self.search_issues(&repo, &query).await?;
    Ok(CommandResult::Ok)
}
Command::ClearIssueSearch { repo } => {
    // Clear search results for this client
    // For now, single-client: just clear the search overlay
    Ok(CommandResult::Ok)
}
```

**Step 3: Implement ensure_issues_cached**

New method on `InProcessDaemon`:

```rust
async fn ensure_issues_cached(&self, repo: &Path, desired_count: usize) -> Result<(), String> {
    let mut repos = self.repos.write().await;
    let state = repos.get_mut(repo)
        .ok_or_else(|| format!("repo not tracked: {}", repo.display()))?;

    while state.issue_cache.entries.len() < desired_count && state.issue_cache.has_more {
        let page_num = state.issue_cache.next_page;
        if let Some(tracker) = state.model.registry.issue_trackers.values().next() {
            let page = tracker.list_issues_page(repo, page_num, 50).await?;
            state.issue_cache.merge_page(page);
        } else {
            break;
        }
    }
    Ok(())
}
```

**Step 4: Pin linked issues after correlation**

In `poll_snapshots`, after updating the model's data, check for linked issue IDs not in the cache and batch-fetch them:

```rust
// After updating providers data from refresh snapshot:
let linked_ids = collect_linked_issue_ids(&snapshot.providers);
let missing = state.issue_cache.missing_ids(&linked_ids);
if !missing.is_empty() {
    if let Some(tracker) = state.model.registry.issue_trackers.values().next() {
        if let Ok(fetched) = tracker.fetch_issues_by_id(path, &missing).await {
            state.issue_cache.add_pinned(fetched);
        }
    }
}

// Merge cache into providers before building snapshot
state.model.data.providers = {
    let mut pd = (*snapshot.providers).clone();
    pd.issues = state.issue_cache.to_index_map();
    Arc::new(pd)
};
```

Write a helper `collect_linked_issue_ids` that extracts issue IDs from `AssociationKey::IssueRef` on change_requests and checkouts.

**Step 5: Populate new Snapshot fields in convert.rs**

In `snapshot_to_proto`, add:

```rust
issue_total: None,      // populated by daemon
issue_has_more: false,   // populated by daemon
issue_search_results: None,
```

Then in `poll_snapshots` / `get_state`, override these from the cache:

```rust
proto_snapshot.issue_total = state.issue_cache.total_count;
proto_snapshot.issue_has_more = state.issue_cache.has_more;
```

**Step 6: Build and test**

Run: `cargo build && cargo test`

**Step 7: Commit**

```
feat: wire IssueCache into InProcessDaemon

The daemon now manages issue fetching via IssueCache. Issues are
fetched on demand (viewport-driven), linked issues are pinned,
and cache state is included in snapshots.
```

---

### Task 8: TUI — infinite scroll and search

**Files:**
- Modify: `crates/flotilla-tui/src/app/intent.rs` (no changes needed if using Command directly)
- Modify: `crates/flotilla-tui/src/app/mod.rs` (key handling, scroll detection)
- Modify: `crates/flotilla-tui/src/app/ui_state.rs` (add IssueSearch mode)
- Modify: `crates/flotilla-tui/src/ui.rs` (loading indicator, search UI)
- Modify: `crates/flotilla-tui/src/app/executor.rs` (send new commands)
- Test: `cargo build -p flotilla-tui`

**Step 1: Send SetIssueViewport on startup/resize**

In `App::new()` or on first snapshot, send `Command::SetIssueViewport` with the terminal height minus chrome (headers, tabs, etc.). Also send on terminal resize events.

**Step 2: Detect scroll past last issue**

In `handle_normal_key` for `j`/`Down`, after updating selection, check if the selected item is in the Issues section and near/at the bottom. If so, send `Command::FetchMoreIssues`:

```rust
// After navigation update:
if self.is_near_issues_bottom() {
    if let Some(snapshot) = &current_snapshot {
        if snapshot.issue_has_more {
            let desired = snapshot.providers.issues.len() + 50;
            self.send_command(Command::FetchMoreIssues {
                repo: snapshot.repo.clone(),
                desired_count: desired,
            });
        }
    }
}
```

**Step 3: Add search mode**

Add `IssueSearch { input: Input }` variant to `UiMode`.

Handle `/` key in normal mode to enter search:

```rust
KeyCode::Char('/') => {
    self.ui.mode = UiMode::IssueSearch { input: Input::default() };
}
```

In search mode, `Enter` sends `Command::SearchIssues`, `Esc` sends `Command::ClearIssueSearch` and returns to Normal.

**Step 4: Render loading indicator**

In `ui.rs`, when rendering the Issues section, if `snapshot.issue_has_more` is true, append a row with "Loading more issues..." or "↓ more" styled as dim text.

**Step 5: Render search results**

When `snapshot.issue_search_results` is `Some`, render search results instead of the normal issues section. Show a header like "Search: {query}" and the results.

**Step 6: Build and test**

Run: `cargo build -p flotilla-tui`

**Step 7: Commit**

```
feat: TUI infinite scroll and search for issues
```

---

### Task 9: Integration test and cleanup

**Files:**
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs` (if applicable)
- Run: full test suite

**Step 1: Run full build and tests**

```bash
cargo build --locked
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

**Step 2: Fix any issues**

Address compilation errors, test failures, clippy warnings.

**Step 3: Manual smoke test**

Run `cargo run` against a repo with many issues. Verify:
- Issues load on startup (viewport-sized batch)
- Scrolling past last issue loads more
- `/` opens search, results appear, `Esc` clears
- Linked issues from PRs appear even if they'd be beyond the first page

**Step 4: Commit any fixes**

```
fix: address review feedback and test failures
```
