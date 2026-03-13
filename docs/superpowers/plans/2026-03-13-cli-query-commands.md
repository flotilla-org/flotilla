# CLI Query Commands Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add four CLI query commands (`status`, `repo <slug>`, `repo <slug> providers`, `repo <slug> work`) with `--json` support, backed by new `DaemonHandle` methods and protocol types.

**Architecture:** New protocol response types in `flotilla-protocol/src/query.rs`. Four new `DaemonHandle` trait methods implemented by `InProcessDaemon` (builds from internal state) and `SocketDaemon` (RPC over socket). Discovery data retained on `RepoState` for provider queries. Slug resolution inside the daemon. CLI dispatch in `main.rs` using `connect_or_spawn` (except `status` which uses `connect`).

**Tech Stack:** Rust, clap (CLI), serde (JSON), async-trait, tokio broadcast channels, comfy-table (human formatting)

**Spec:** `docs/superpowers/specs/2026-03-13-cli-query-commands-design.md`

---

## Chunk 1: Foundation (protocol types, discovery changes, slug resolution)

### Task 1: Protocol query types

**Files:**
- Create: `crates/flotilla-protocol/src/query.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs` (add `pub mod query;` and re-exports)

- [ ] **Step 1: Create query.rs with all response types**

```rust
// crates/flotilla-protocol/src/query.rs
use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::snapshot::{ProviderError, WorkItem};

/// Provider health across categories. Outer key: category (e.g. "vcs",
/// "code_review"). Inner key: provider name. Value: healthy.
pub type ProviderHealthMap = HashMap<String, HashMap<String, bool>>;

// --- status ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResponse {
    pub repos: Vec<RepoSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoSummary {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_item_count: usize,
    pub error_count: usize,
}

// --- repo detail ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoDetailResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub provider_health: ProviderHealthMap,
    pub work_items: Vec<WorkItem>,
    pub errors: Vec<ProviderError>,
}

// --- repo providers ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoProvidersResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub host_discovery: Vec<DiscoveryEntry>,
    pub repo_discovery: Vec<DiscoveryEntry>,
    pub providers: Vec<ProviderInfo>,
    pub unmet_requirements: Vec<UnmetRequirementInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryEntry {
    pub kind: String,
    pub detail: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub category: String,
    pub name: String,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnmetRequirementInfo {
    pub factory: String,
    pub requirement: String,
}

// --- repo work ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoWorkResponse {
    pub path: PathBuf,
    pub slug: Option<String>,
    pub work_items: Vec<WorkItem>,
}
```

- [ ] **Step 2: Add module declaration and re-exports to lib.rs**

In `crates/flotilla-protocol/src/lib.rs`, add `pub mod query;` alongside the existing module declarations. Add re-exports:

```rust
pub use query::{
    DiscoveryEntry, ProviderHealthMap, ProviderInfo, RepoDetailResponse, RepoProvidersResponse,
    RepoSummary, RepoWorkResponse, StatusResponse, UnmetRequirementInfo,
};
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p flotilla-protocol`
Expected: compiles cleanly

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-protocol/src/query.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat: add protocol query response types (#282)"
```

---

### Task 2: EnvironmentBag accessor

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`

- [ ] **Step 1: Write test for assertions accessor**

Add to the existing `#[cfg(test)] mod tests` block at the bottom of `crates/flotilla-core/src/providers/discovery/mod.rs`:

```rust
#[test]
fn environment_bag_assertions_accessor() {
    let bag = EnvironmentBag::new()
        .with(EnvironmentAssertion::BinaryAvailable {
            name: "git".into(),
            path: PathBuf::from("/usr/bin/git"),
            version: Some("2.40".into()),
        })
        .with(EnvironmentAssertion::AuthFileExists {
            provider: "github".into(),
            path: PathBuf::from("/home/user/.config/gh/hosts.yml"),
        });
    assert_eq!(bag.assertions().len(), 2);
    assert!(matches!(bag.assertions()[0], EnvironmentAssertion::BinaryAvailable { ref name, .. } if name == "git"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core assertions_accessor`
Expected: FAIL — `assertions` method not found

- [ ] **Step 3: Add the accessor method**

Add to the `impl EnvironmentBag` block (after the `new()` method):

```rust
/// Public read access to the raw assertions, for conversion to protocol types.
pub fn assertions(&self) -> &[EnvironmentAssertion] {
    &self.assertions
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core assertions_accessor`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/mod.rs
git commit -m "feat: add EnvironmentBag::assertions() accessor (#282)"
```

---

### Task 3: Tag unmet requirements with factory name

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`
- Modify: `crates/flotilla-core/src/in_process.rs` (update destructuring of `DiscoveryResult`)

- [ ] **Step 1: Write test for tagged unmet requirements**

Add to the tests block in `crates/flotilla-core/src/providers/discovery/mod.rs`:

```rust
#[test]
fn unmet_requirements_tagged_with_factory_name() {
    // Verify the tuple structure compiles and is accessible
    let tagged: Vec<(String, UnmetRequirement)> = vec![
        ("github-cr".into(), UnmetRequirement::MissingBinary("gh".into())),
        ("claude".into(), UnmetRequirement::MissingAuth("claude".into())),
    ];
    assert_eq!(tagged[0].0, "github-cr");
    assert!(matches!(tagged[0].1, UnmetRequirement::MissingBinary(ref s) if s == "gh"));
}
```

- [ ] **Step 2: Run test to verify it passes (type structure test)**

Run: `cargo test -p flotilla-core unmet_requirements_tagged`
Expected: PASS (this just validates the tuple type compiles)

- [ ] **Step 3: Change DiscoveryResult.unmet type and probe helpers**

In `crates/flotilla-core/src/providers/discovery/mod.rs`:

Change `DiscoveryResult`:
```rust
pub struct DiscoveryResult {
    pub registry: ProviderRegistry,
    pub host_repo_bag: EnvironmentBag,
    pub repo_bag: EnvironmentBag,
    pub repo_slug: Option<String>,
    pub unmet: Vec<(String, UnmetRequirement)>,
}
```

Change `probe_all` signature — `unmet: &mut Vec<(String, UnmetRequirement)>` and tag with factory name:
```rust
Err(reqs) => {
    let name = factory.descriptor().name.clone();
    unmet.extend(reqs.into_iter().map(|r| (name.clone(), r)));
}
```

Same change for `probe_first`.

Change the `DiscoveryResult` construction at the end of `discover_providers`:
```rust
DiscoveryResult { registry, host_repo_bag: combined, repo_bag, repo_slug, unmet }
```

This means `repo_bag` must be kept alive — currently `discover_providers` creates `repo_bag` then consumes it into `combined` via `host_bag.merge(&repo_bag)`. Since `merge` takes `&self` and `&other` (both by reference), the repo_bag survives. Verify this by reading the `merge` method — it clones both sides.

- [ ] **Step 4: Update in_process.rs to match new DiscoveryResult shape**

In `crates/flotilla-core/src/in_process.rs`, find the two places that destructure `DiscoveryResult`:

1. In `new_with_options` (around line 336):
```rust
let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } =
```
Use `host_repo_bag` where `bag` was used (for `repo_identity()`).

2. In `add_repo` (around line 1178):
```rust
let DiscoveryResult { registry, repo_slug, host_repo_bag, repo_bag, unmet } =
```
Same adjustment.

- [ ] **Step 5: Update existing discovery tests for renamed field**

In `crates/flotilla-core/src/providers/discovery/mod.rs`, find the `discover_providers_with_git_repo` test (or any test that accesses `result.bag`). Change `result.bag` to `result.host_repo_bag` — e.g.:

```rust
// Before: result.bag.find_binary("git")
// After:  result.host_repo_bag.find_binary("git")
```

- [ ] **Step 6: Add new fields to RepoState**

In `crates/flotilla-core/src/in_process.rs`, add to `struct RepoState` (around line 170):

```rust
slug: Option<String>,
repo_bag: EnvironmentBag,
unmet: Vec<(String, UnmetRequirement)>,
```

Update both `RepoState` construction sites (in `new_with_options` and `add_repo`) to populate these fields. Note: `repo_slug` is `Option<String>` and is moved into `RepoModel::new(path.clone(), registry, repo_slug)`. Clone it before the move so `RepoState` can also store it:

```rust
let slug = repo_slug.clone();
let mut model = RepoModel::new(path.clone(), registry, repo_slug);
// ...
repos.insert(path.clone(), RepoState {
    model,
    slug,
    repo_bag,
    unmet,
    // ... existing fields ...
});
```

Add the necessary imports for `EnvironmentBag` and `UnmetRequirement` (they should already be available via the existing `discovery` import).

- [ ] **Step 7: Verify it compiles and existing tests pass**

Run: `cargo build && cargo test -p flotilla-core`
Expected: compiles and all existing tests pass

- [ ] **Step 8: Commit**

```bash
git add crates/flotilla-core/src/providers/discovery/mod.rs crates/flotilla-core/src/in_process.rs
git commit -m "feat: retain discovery data on RepoState (#282)

Tag unmet requirements with factory name. Return repo bag
separately from combined bag in DiscoveryResult. Store slug,
repo_bag, and unmet on RepoState for CLI query access."
```

---

### Task 4: Repo slug resolution

**Files:**
- Create: `crates/flotilla-core/src/resolve.rs`
- Modify: `crates/flotilla-core/src/lib.rs` (add `pub mod resolve;`)

- [ ] **Step 1: Write tests for resolve_repo**

Create `crates/flotilla-core/src/resolve.rs` with tests first:

```rust
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum ResolveError {
    NotFound(String),
    Ambiguous { query: String, candidates: Vec<PathBuf> },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(q) => write!(f, "no repo matching '{q}'"),
            Self::Ambiguous { query, candidates } => {
                write!(f, "'{query}' matches multiple repos:")?;
                for c in candidates {
                    write!(f, "\n  {}", c.display())?;
                }
                Ok(())
            }
        }
    }
}

pub fn resolve_repo<'a>(
    query: &str,
    repos: impl Iterator<Item = (&'a Path, Option<&'a str>)>,
) -> Result<PathBuf, ResolveError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repos() -> Vec<(PathBuf, Option<String>)> {
        vec![
            (PathBuf::from("/home/user/dev/flotilla"), Some("rjwittams/flotilla".into())),
            (PathBuf::from("/home/user/dev/other-project"), Some("org/other-project".into())),
            (PathBuf::from("/home/user/dev/flotilla-fork"), Some("someone/flotilla".into())),
        ]
    }

    fn iter(repos: &[(PathBuf, Option<String>)]) -> impl Iterator<Item = (&Path, Option<&str>)> {
        repos.iter().map(|(p, s)| (p.as_path(), s.as_deref()))
    }

    #[test]
    fn exact_path_match() {
        let r = repos();
        let result = resolve_repo("/home/user/dev/flotilla", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/flotilla")));
    }

    #[test]
    fn exact_name_match() {
        let r = repos();
        let result = resolve_repo("other-project", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/other-project")));
    }

    #[test]
    fn exact_slug_match() {
        let r = repos();
        let result = resolve_repo("rjwittams/flotilla", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/flotilla")));
    }

    #[test]
    fn unique_substring_match() {
        let r = repos();
        let result = resolve_repo("other", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/other-project")));
    }

    #[test]
    fn ambiguous_substring() {
        let r = repos();
        // "flot" is a substring of both "flotilla" and "flotilla-fork"
        // but matches no exact name or slug, so it's ambiguous
        let result = resolve_repo("flot", iter(&r));
        assert!(matches!(result, Err(ResolveError::Ambiguous { .. })));
        if let Err(ResolveError::Ambiguous { candidates, .. }) = result {
            assert_eq!(candidates.len(), 2);
        }
    }

    #[test]
    fn not_found() {
        let r = repos();
        let result = resolve_repo("nonexistent", iter(&r));
        assert!(matches!(result, Err(ResolveError::NotFound(_))));
    }

    #[test]
    fn exact_match_takes_priority_over_substring() {
        // "flotilla-fork" has "flotilla" as a substring, but exact name
        // "flotilla" should match the first repo, not be ambiguous
        let r = vec![
            (PathBuf::from("/a/flotilla"), Some("rjwittams/flotilla".into())),
            (PathBuf::from("/a/flotilla-fork"), Some("someone/flotilla".into())),
        ];
        let result = resolve_repo("flotilla", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/a/flotilla")));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core resolve`
Expected: FAIL — `todo!()` panics

- [ ] **Step 3: Implement resolve_repo**

Replace the `todo!()` body:

```rust
pub fn resolve_repo<'a>(
    query: &str,
    repos: impl Iterator<Item = (&'a Path, Option<&'a str>)>,
) -> Result<PathBuf, ResolveError> {
    let entries: Vec<_> = repos.collect();

    // 1. Exact path match
    for &(path, _) in &entries {
        if path.as_os_str() == query {
            return Ok(path.to_path_buf());
        }
    }

    // 2. Exact repo name (last path component)
    for &(path, _) in &entries {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == query {
                return Ok(path.to_path_buf());
            }
        }
    }

    // 3. Exact slug match
    for &(path, slug) in &entries {
        if slug == Some(query) {
            return Ok(path.to_path_buf());
        }
    }

    // 4. Unique substring match against name and slug
    let mut matches: Vec<PathBuf> = Vec::new();
    for &(path, slug) in &entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(query) || slug.map_or(false, |s| s.contains(query)) {
            matches.push(path.to_path_buf());
        }
    }

    match matches.len() {
        0 => Err(ResolveError::NotFound(query.to_string())),
        1 => Ok(matches.into_iter().next().expect("checked len")),
        _ => Err(ResolveError::Ambiguous { query: query.to_string(), candidates: matches }),
    }
}
```

- [ ] **Step 4: Add module declaration to lib.rs**

In `crates/flotilla-core/src/lib.rs`, add `pub mod resolve;`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-core resolve`
Expected: all 7 tests PASS

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/resolve.rs crates/flotilla-core/src/lib.rs
git commit -m "feat: add repo slug resolution (#282)"
```

---

## Chunk 2: DaemonHandle trait, InProcessDaemon, conversion

### Task 5: Assertion-to-DiscoveryEntry conversion

**Files:**
- Modify: `crates/flotilla-core/src/convert.rs`

- [ ] **Step 1: Write test for assertion conversion**

Add tests to `crates/flotilla-core/src/convert.rs` (create a `#[cfg(test)] mod tests` block if one doesn't exist):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::discovery::EnvironmentAssertion;
    use std::path::PathBuf;

    #[test]
    fn convert_binary_available() {
        let assertion = EnvironmentAssertion::BinaryAvailable {
            name: "git".into(),
            path: PathBuf::from("/usr/bin/git"),
            version: Some("2.40".into()),
        };
        let entry = assertion_to_discovery_entry(&assertion);
        assert_eq!(entry.kind, "binary_available");
        assert_eq!(entry.detail["name"], "git");
        assert_eq!(entry.detail["path"], "/usr/bin/git");
        assert_eq!(entry.detail["version"], "2.40");
    }

    #[test]
    fn convert_auth_file_exists() {
        let assertion = EnvironmentAssertion::AuthFileExists {
            provider: "github".into(),
            path: PathBuf::from("/home/.config/gh/hosts.yml"),
        };
        let entry = assertion_to_discovery_entry(&assertion);
        assert_eq!(entry.kind, "auth_file_exists");
        assert_eq!(entry.detail["provider"], "github");
    }

    #[test]
    fn convert_socket_available() {
        let assertion = EnvironmentAssertion::SocketAvailable {
            name: "shpool".into(),
            path: PathBuf::from("/tmp/shpool.sock"),
        };
        let entry = assertion_to_discovery_entry(&assertion);
        assert_eq!(entry.kind, "socket_available");
        assert_eq!(entry.detail["name"], "shpool");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-core convert`
Expected: FAIL — `assertion_to_discovery_entry` not found

- [ ] **Step 3: Implement assertion_to_discovery_entry**

Add to `crates/flotilla-core/src/convert.rs`:

```rust
use flotilla_protocol::DiscoveryEntry;
use crate::providers::discovery::EnvironmentAssertion;

pub fn assertion_to_discovery_entry(assertion: &EnvironmentAssertion) -> DiscoveryEntry {
    let mut detail = std::collections::HashMap::new();
    let kind = match assertion {
        EnvironmentAssertion::BinaryAvailable { name, path, version } => {
            detail.insert("name".into(), name.clone());
            detail.insert("path".into(), path.display().to_string());
            if let Some(v) = version {
                detail.insert("version".into(), v.clone());
            }
            "binary_available"
        }
        EnvironmentAssertion::EnvVarSet { key, value } => {
            detail.insert("key".into(), key.clone());
            detail.insert("value".into(), value.clone());
            "env_var_set"
        }
        EnvironmentAssertion::VcsCheckoutDetected { root, kind, is_main_checkout } => {
            detail.insert("root".into(), root.display().to_string());
            detail.insert("kind".into(), format!("{kind:?}"));
            detail.insert("is_main_checkout".into(), is_main_checkout.to_string());
            "vcs_checkout_detected"
        }
        EnvironmentAssertion::RemoteHost { platform, owner, repo, remote_name } => {
            detail.insert("platform".into(), format!("{platform:?}"));
            detail.insert("owner".into(), owner.clone());
            detail.insert("repo".into(), repo.clone());
            detail.insert("remote_name".into(), remote_name.clone());
            "remote_host"
        }
        EnvironmentAssertion::AuthFileExists { provider, path } => {
            detail.insert("provider".into(), provider.clone());
            detail.insert("path".into(), path.display().to_string());
            "auth_file_exists"
        }
        EnvironmentAssertion::SocketAvailable { name, path } => {
            detail.insert("name".into(), name.clone());
            detail.insert("path".into(), path.display().to_string());
            "socket_available"
        }
    };
    DiscoveryEntry { kind: kind.into(), detail }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-core convert`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/convert.rs
git commit -m "feat: add assertion-to-DiscoveryEntry conversion (#282)"
```

---

### Task 6: ProviderRegistry provider_info helper

**Files:**
- Modify: `crates/flotilla-core/src/providers/registry.rs`

- [ ] **Step 1: Write test for provider_info iteration**

Add to the tests in `crates/flotilla-core/src/providers/registry.rs` (or create a test block):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_infos_from_empty_registry() {
        let registry = ProviderRegistry::new();
        let infos = registry.provider_infos();
        assert!(infos.is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core provider_infos`
Expected: FAIL — method not found

- [ ] **Step 3: Implement provider_infos method**

Add to the `impl ProviderRegistry` block in `crates/flotilla-core/src/providers/registry.rs`:

```rust
/// Build a list of provider info summaries for all registered providers.
/// Category strings match the keys used in `compute_provider_health`.
pub fn provider_infos(&self) -> Vec<(String, String)> {
    let mut infos = Vec::new();
    for (desc, _) in self.vcs.values() {
        infos.push(("vcs".into(), desc.display_name.clone()));
    }
    for (desc, _) in self.checkout_managers.values() {
        infos.push(("checkout_manager".into(), desc.display_name.clone()));
    }
    for (desc, _) in self.code_review.values() {
        infos.push(("code_review".into(), desc.display_name.clone()));
    }
    for (desc, _) in self.issue_trackers.values() {
        infos.push(("issue_tracker".into(), desc.display_name.clone()));
    }
    for (desc, _) in self.cloud_agents.values() {
        infos.push(("cloud_agent".into(), desc.display_name.clone()));
    }
    for (desc, _) in self.ai_utilities.values() {
        infos.push(("ai_utility".into(), desc.display_name.clone()));
    }
    if let Some((desc, _)) = &self.workspace_manager {
        infos.push(("workspace_manager".into(), desc.display_name.clone()));
    }
    if let Some((desc, _)) = &self.terminal_pool {
        infos.push(("terminal_pool".into(), desc.display_name.clone()));
    }
    infos
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core provider_infos`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-core/src/providers/registry.rs
git commit -m "feat: add ProviderRegistry::provider_infos() (#282)"
```

---

### Task 7: DaemonHandle trait extensions

**Files:**
- Modify: `crates/flotilla-core/src/daemon.rs`

- [ ] **Step 1: Add four new methods to DaemonHandle**

Add to the trait in `crates/flotilla-core/src/daemon.rs`, after the existing methods:

```rust
/// High-level status: repos, health, counts.
async fn get_status(&self) -> Result<StatusResponse, String>;

/// Repo detail: work items, provider health, errors.
async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String>;

/// Repo discovery: host/repo assertions, providers, unmet requirements.
async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String>;

/// Repo work items.
async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String>;
```

Add the necessary imports from `flotilla_protocol`:
```rust
use flotilla_protocol::{RepoDetailResponse, RepoProvidersResponse, RepoWorkResponse, StatusResponse};
```

- [ ] **Step 2: Verify it fails to compile (implementations missing)**

Run: `cargo build -p flotilla-core`
Expected: FAIL — `InProcessDaemon` doesn't implement the new methods

- [ ] **Step 3: Add stub implementations to InProcessDaemon**

In `crates/flotilla-core/src/in_process.rs`, add stubs inside the `#[async_trait] impl DaemonHandle for InProcessDaemon` block:

```rust
async fn get_status(&self) -> Result<StatusResponse, String> {
    todo!("Task 8")
}

async fn get_repo_detail(&self, _slug: &str) -> Result<RepoDetailResponse, String> {
    todo!("Task 8")
}

async fn get_repo_providers(&self, _slug: &str) -> Result<RepoProvidersResponse, String> {
    todo!("Task 8")
}

async fn get_repo_work(&self, _slug: &str) -> Result<RepoWorkResponse, String> {
    todo!("Task 8")
}
```

Add imports for the response types.

- [ ] **Step 4: Add stub implementations to SocketDaemon**

In `crates/flotilla-client/src/lib.rs`, add stubs inside the `#[async_trait] impl DaemonHandle for SocketDaemon` block:

```rust
async fn get_status(&self) -> Result<StatusResponse, String> {
    todo!("Task 10")
}

async fn get_repo_detail(&self, _slug: &str) -> Result<RepoDetailResponse, String> {
    todo!("Task 10")
}

async fn get_repo_providers(&self, _slug: &str) -> Result<RepoProvidersResponse, String> {
    todo!("Task 10")
}

async fn get_repo_work(&self, _slug: &str) -> Result<RepoWorkResponse, String> {
    todo!("Task 10")
}
```

Add imports for the response types.

- [ ] **Step 5: Verify it compiles**

Run: `cargo build`
Expected: compiles cleanly

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-core/src/daemon.rs crates/flotilla-core/src/in_process.rs crates/flotilla-client/src/lib.rs
git commit -m "feat: add query methods to DaemonHandle trait (#282)

Stubs in InProcessDaemon and SocketDaemon, to be implemented next."
```

---

### Task 8: InProcessDaemon query implementations

**Files:**
- Modify: `crates/flotilla-core/src/in_process.rs`
- Test: `crates/flotilla-core/tests/in_process_daemon.rs`

- [ ] **Step 1: Write test for get_status**

Add to `crates/flotilla-core/tests/in_process_daemon.rs`:

```rust
#[tokio::test]
async fn get_status_returns_repo_summaries() {
    let (_repo, daemon) = daemon_for_cwd().await;
    // Wait for initial snapshot
    let mut rx = daemon.subscribe();
    recv_event(&mut rx).await;

    let status = daemon.get_status().await.expect("get_status failed");
    assert!(!status.repos.is_empty());
    let summary = &status.repos[0];
    assert!(summary.path.exists());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core --test in_process_daemon get_status`
Expected: FAIL — `todo!()` panic

- [ ] **Step 3: Implement get_status**

In `crates/flotilla-core/src/in_process.rs`, replace the `get_status` stub. Follow the existing `get_state` pattern for building snapshots — `build_repo_snapshot_with_peers` takes `(path, seq, base, cache, search_results, host_name, peer_overlay)`:

```rust
async fn get_status(&self) -> Result<StatusResponse, String> {
    let peer_providers = self.peer_providers.read().await;
    let repos = self.repos.read().await;
    let repo_order = self.repo_order.read().await;
    let mut summaries = Vec::new();

    for path in repo_order.iter() {
        let Some(state) = repos.get(path) else { continue };
        let peer_overlay = peer_providers.get(path).cloned();
        let snapshot = build_repo_snapshot_with_peers(
            path,
            state.seq,
            &state.last_snapshot,
            &state.issue_cache,
            &state.search_results,
            &self.host_name,
            peer_overlay.as_deref(),
        );
        summaries.push(RepoSummary {
            path: path.clone(),
            slug: state.slug.clone(),
            provider_health: snapshot.provider_health,
            work_item_count: snapshot.work_items.len(),
            error_count: snapshot.errors.len(),
        });
    }

    Ok(StatusResponse { repos: summaries })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core --test in_process_daemon get_status`
Expected: PASS

- [ ] **Step 5: Add slug resolution helper**

Add a private method to `InProcessDaemon`:

```rust
/// Resolve a slug to a repo path using the current repo map.
async fn resolve_slug(&self, slug: &str) -> Result<PathBuf, String> {
    let repos = self.repos.read().await;
    let entries: Vec<_> = repos.iter().map(|(path, state)| (path.as_path(), state.slug.as_deref())).collect();
    crate::resolve::resolve_repo(slug, entries.into_iter()).map_err(|e| e.to_string())
}
```

- [ ] **Step 6: Write test for get_repo_work**

Add to `crates/flotilla-core/tests/in_process_daemon.rs`:

```rust
#[tokio::test]
async fn get_repo_work_returns_work_items() {
    let (repo, daemon) = daemon_for_cwd().await;
    let mut rx = daemon.subscribe();
    recv_event(&mut rx).await;

    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    let work = daemon.get_repo_work(repo_name).await.expect("get_repo_work failed");
    assert_eq!(work.path, repo);
    // Work items should exist (at least the main checkout)
    assert!(!work.work_items.is_empty());
}
```

- [ ] **Step 7: Implement get_repo_detail, get_repo_providers, get_repo_work**

Replace the remaining stubs in `InProcessDaemon`. Follow the `get_state` pattern: acquire `peer_providers` lock first, get per-repo overlay, then build snapshot with `build_repo_snapshot_with_peers`:

```rust
async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String> {
    let path = self.resolve_slug(slug).await?;
    let peer_overlay = {
        let pp = self.peer_providers.read().await;
        pp.get(&path).cloned()
    };
    let repos = self.repos.read().await;
    let state = repos.get(&path).ok_or_else(|| format!("repo not found: {}", path.display()))?;
    let snapshot = build_repo_snapshot_with_peers(
        &path, state.seq, &state.last_snapshot, &state.issue_cache,
        &state.search_results, &self.host_name, peer_overlay.as_deref(),
    );
    Ok(RepoDetailResponse {
        path,
        slug: state.slug.clone(),
        provider_health: snapshot.provider_health,
        work_items: snapshot.work_items,
        errors: snapshot.errors,
    })
}

async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String> {
    let path = self.resolve_slug(slug).await?;
    let peer_overlay = {
        let pp = self.peer_providers.read().await;
        pp.get(&path).cloned()
    };
    let repos = self.repos.read().await;
    let state = repos.get(&path).ok_or_else(|| format!("repo not found: {}", path.display()))?;
    let snapshot = build_repo_snapshot_with_peers(
        &path, state.seq, &state.last_snapshot, &state.issue_cache,
        &state.search_results, &self.host_name, peer_overlay.as_deref(),
    );

    let host_discovery = self.host_bag.assertions().iter().map(assertion_to_discovery_entry).collect();
    let repo_discovery = state.repo_bag.assertions().iter().map(assertion_to_discovery_entry).collect();

    let provider_infos: Vec<ProviderInfo> = state.model.registry.provider_infos().into_iter().map(|(category, name)| {
        let healthy = snapshot.provider_health
            .get(&category)
            .and_then(|providers| providers.get(&name))
            .copied()
            .unwrap_or(true);
        ProviderInfo { category, name, healthy }
    }).collect();

    let unmet_requirements = state.unmet.iter().map(|(factory, req)| {
        UnmetRequirementInfo {
            factory: factory.clone(),
            requirement: format!("{req:?}"),
        }
    }).collect();

    Ok(RepoProvidersResponse {
        path,
        slug: state.slug.clone(),
        host_discovery,
        repo_discovery,
        providers: provider_infos,
        unmet_requirements,
    })
}

async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String> {
    let path = self.resolve_slug(slug).await?;
    let peer_overlay = {
        let pp = self.peer_providers.read().await;
        pp.get(&path).cloned()
    };
    let repos = self.repos.read().await;
    let state = repos.get(&path).ok_or_else(|| format!("repo not found: {}", path.display()))?;
    let snapshot = build_repo_snapshot_with_peers(
        &path, state.seq, &state.last_snapshot, &state.issue_cache,
        &state.search_results, &self.host_name, peer_overlay.as_deref(),
    );
    Ok(RepoWorkResponse {
        path,
        slug: state.slug.clone(),
        work_items: snapshot.work_items,
    })
}
```

Add import for `assertion_to_discovery_entry` from `crate::convert`.

- [ ] **Step 8: Run all tests**

Run: `cargo test -p flotilla-core`
Expected: PASS

- [ ] **Step 9: Commit**

```bash
git add crates/flotilla-core/src/in_process.rs crates/flotilla-core/tests/in_process_daemon.rs
git commit -m "feat: implement InProcessDaemon query methods (#282)"
```

---

## Chunk 3: Wire protocol, SocketDaemon, CLI

### Task 9: Daemon server dispatch

**Files:**
- Modify: `crates/flotilla-daemon/src/server.rs`

- [ ] **Step 1: Add new method dispatch cases**

In `dispatch_request` in `crates/flotilla-daemon/src/server.rs`, add four new match arms before the `unknown` catch-all:

```rust
"get_status" => match daemon.get_status().await {
    Ok(status) => Message::ok_response(id, &status),
    Err(e) => Message::error_response(id, e),
},
"get_repo_detail" => {
    let slug = params.get("slug").and_then(|v| v.as_str()).ok_or("missing 'slug' parameter");
    match slug {
        Err(e) => Message::error_response(id, e.to_string()),
        Ok(slug) => match daemon.get_repo_detail(slug).await {
            Ok(detail) => Message::ok_response(id, &detail),
            Err(e) => Message::error_response(id, e),
        },
    }
},
"get_repo_providers" => {
    let slug = params.get("slug").and_then(|v| v.as_str()).ok_or("missing 'slug' parameter");
    match slug {
        Err(e) => Message::error_response(id, e.to_string()),
        Ok(slug) => match daemon.get_repo_providers(slug).await {
            Ok(providers) => Message::ok_response(id, &providers),
            Err(e) => Message::error_response(id, e),
        },
    }
},
"get_repo_work" => {
    let slug = params.get("slug").and_then(|v| v.as_str()).ok_or("missing 'slug' parameter");
    match slug {
        Err(e) => Message::error_response(id, e.to_string()),
        Ok(slug) => match daemon.get_repo_work(slug).await {
            Ok(work) => Message::ok_response(id, &work),
            Err(e) => Message::error_response(id, e),
        },
    }
},
```

This follows the existing `dispatch_request` pattern: `Message::ok_response(id, &value)` for success, `Message::error_response(id, e)` for errors. If the file uses helper functions like `extract_string_param`, use those instead.

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p flotilla-daemon`
Expected: compiles cleanly

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-daemon/src/server.rs
git commit -m "feat: add query method dispatch to daemon server (#282)"
```

---

### Task 10: SocketDaemon query implementations

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs`
- Test: `crates/flotilla-daemon/tests/socket_roundtrip.rs`

- [ ] **Step 1: Implement SocketDaemon query methods**

Replace the stubs in `crates/flotilla-client/src/lib.rs`:

```rust
async fn get_status(&self) -> Result<StatusResponse, String> {
    let resp = self.request("get_status", serde_json::json!({})).await?;
    resp.parse()
}

async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String> {
    let resp = self.request("get_repo_detail", serde_json::json!({ "slug": slug })).await?;
    resp.parse()
}

async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String> {
    let resp = self.request("get_repo_providers", serde_json::json!({ "slug": slug })).await?;
    resp.parse()
}

async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String> {
    let resp = self.request("get_repo_work", serde_json::json!({ "slug": slug })).await?;
    resp.parse()
}
```

- [ ] **Step 2: Add socket roundtrip test for query methods**

Add to `crates/flotilla-daemon/tests/socket_roundtrip.rs` (after the existing test or as a new test):

```rust
#[tokio::test]
async fn query_commands_roundtrip() {
    // Reuse existing setup pattern from socket_roundtrip test
    let tmp = tempfile::TempDir::new().unwrap();
    let socket_path = tmp.path().join("test.sock");
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest_dir.parent().unwrap().parent().unwrap().to_path_buf();
    let config = Arc::new(ConfigStore::with_base(tmp.path().join("config")));

    let server = DaemonServer::new(vec![repo.clone()], config, socket_path.clone(), Duration::from_secs(300))
        .await
        .expect("server start");
    let server_handle = tokio::spawn(async move { server.run().await });

    let client = loop {
        match flotilla_client::SocketDaemon::connect(&socket_path).await {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };

    // Wait for initial snapshot
    let mut rx = client.subscribe();
    let _ = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await;

    // get_status
    let status = client.get_status().await.expect("get_status");
    assert!(!status.repos.is_empty());
    assert_eq!(status.repos[0].path, repo);

    // get_repo_detail by name
    let repo_name = repo.file_name().unwrap().to_str().unwrap();
    let detail = client.get_repo_detail(repo_name).await.expect("get_repo_detail");
    assert_eq!(detail.path, repo);

    // get_repo_providers
    let providers = client.get_repo_providers(repo_name).await.expect("get_repo_providers");
    assert_eq!(providers.path, repo);
    assert!(!providers.providers.is_empty()); // at least VCS

    // get_repo_work
    let work = client.get_repo_work(repo_name).await.expect("get_repo_work");
    assert_eq!(work.path, repo);

    // slug resolution error
    let err = client.get_repo_detail("nonexistent").await;
    assert!(err.is_err());

    server_handle.abort();
}
```

- [ ] **Step 3: Run the roundtrip test**

Run: `cargo test -p flotilla-daemon query_commands_roundtrip -- --nocapture`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-client/src/lib.rs crates/flotilla-daemon/tests/socket_roundtrip.rs
git commit -m "feat: implement SocketDaemon query methods with roundtrip test (#282)"
```

---

### Task 11: CLI subcommands and handler functions

**Files:**
- Modify: `src/main.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`
- Modify: `Cargo.toml` (root, add `comfy-table` dep to flotilla-tui)

- [ ] **Step 1: Add comfy-table dependency**

Run: `cargo add comfy-table -p flotilla-tui`

- [ ] **Step 2: Add RepoSubCommand and extend SubCommand**

In `src/main.rs`, add the new enum and extend `SubCommand`:

```rust
#[derive(clap::Subcommand)]
enum RepoSubCommand {
    /// Show provider discovery and instances
    Providers,
    /// Show work items
    Work,
}
```

Add the `Repo` variant to `SubCommand`:

```rust
/// Query a specific repo
Repo {
    /// Repo path, name, or slug (e.g. "owner/repo")
    slug: String,
    /// Output as JSON instead of human-readable text
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    command: Option<RepoSubCommand>,
},
```

- [ ] **Step 3: Add dispatch logic for Repo subcommand**

In the `match &cli.command` block in `src/main.rs`:

```rust
Some(SubCommand::Repo { slug, json, command }) => {
    let format = OutputFormat::from_json_flag(*json);
    run_repo(&cli, slug, format, command.as_ref()).await
}
```

Add the `run_repo` function. Use `cli.socket_path()` and `cli.config_dir()` (the existing helper methods on `Cli`), and `flotilla_tui::socket::connect_or_spawn` (re-exported from `flotilla_client`, consistent with `run_tui`):

```rust
async fn run_repo(cli: &Cli, slug: &str, format: OutputFormat, command: Option<&RepoSubCommand>) -> color_eyre::Result<()> {
    reset_sigpipe();
    let socket_path = cli.socket_path();
    let config_dir = cli.config_dir();
    let daemon = flotilla_tui::socket::connect_or_spawn(
        &socket_path,
        &config_dir,
        cli.config_dir.as_deref(),
        cli.socket.as_deref(),
    )
    .await
    .map_err(|e| color_eyre::eyre::eyre!(e))?;

    let result = match command {
        None => flotilla_tui::cli::run_repo_detail(&*daemon, slug, format).await,
        Some(RepoSubCommand::Providers) => flotilla_tui::cli::run_repo_providers(&*daemon, slug, format).await,
        Some(RepoSubCommand::Work) => flotilla_tui::cli::run_repo_work(&*daemon, slug, format).await,
    };
    result.map_err(|e| color_eyre::eyre::eyre!(e))
}
```

- [ ] **Step 4: Implement CLI handler functions**

In `crates/flotilla-tui/src/cli.rs`, add the handler functions and formatters:

```rust
use comfy_table::{presets::UTF8_FULL_CONDENSED, Table, Cell};
use flotilla_protocol::{
    output::OutputFormat,
    RepoDetailResponse, RepoProvidersResponse, RepoWorkResponse, StatusResponse,
};

pub async fn run_repo_detail(daemon: &dyn DaemonHandle, slug: &str, format: OutputFormat) -> Result<(), String> {
    let detail = daemon.get_repo_detail(slug).await?;
    let output = match format {
        OutputFormat::Human => format_repo_detail_human(&detail),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&detail),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_repo_providers(daemon: &dyn DaemonHandle, slug: &str, format: OutputFormat) -> Result<(), String> {
    let providers = daemon.get_repo_providers(slug).await?;
    let output = match format {
        OutputFormat::Human => format_repo_providers_human(&providers),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&providers),
    };
    print!("{output}");
    Ok(())
}

pub async fn run_repo_work(daemon: &dyn DaemonHandle, slug: &str, format: OutputFormat) -> Result<(), String> {
    let work = daemon.get_repo_work(slug).await?;
    let output = match format {
        OutputFormat::Human => format_repo_work_human(&work),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&work),
    };
    print!("{output}");
    Ok(())
}
```

- [ ] **Step 5: Implement human formatters**

```rust
fn format_repo_detail_human(detail: &RepoDetailResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", detail.path.display()));
    if let Some(slug) = &detail.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }
    out.push('\n');

    if !detail.work_items.is_empty() {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Kind", "Branch", "Description"]);
        for item in &detail.work_items {
            table.add_row(vec![
                Cell::new(format!("{:?}", item.kind)),
                Cell::new(item.branch.as_deref().unwrap_or("-")),
                Cell::new(&item.description),
            ]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !detail.errors.is_empty() {
        out.push_str("\nErrors:\n");
        for err in &detail.errors {
            out.push_str(&format!("  [{}/{}] {}\n", err.category, err.provider, err.message));
        }
    }
    out
}

fn format_repo_providers_human(resp: &RepoProvidersResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", resp.path.display()));
    if let Some(slug) = &resp.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }

    if !resp.host_discovery.is_empty() {
        out.push_str("\nHost Discovery:\n");
        for entry in &resp.host_discovery {
            let details: Vec<String> = entry.detail.iter().map(|(k, v)| format!("{k}={v}")).collect();
            out.push_str(&format!("  {} ({})\n", entry.kind, details.join(", ")));
        }
    }

    if !resp.repo_discovery.is_empty() {
        out.push_str("\nRepo Discovery:\n");
        for entry in &resp.repo_discovery {
            let details: Vec<String> = entry.detail.iter().map(|(k, v)| format!("{k}={v}")).collect();
            out.push_str(&format!("  {} ({})\n", entry.kind, details.join(", ")));
        }
    }

    if !resp.providers.is_empty() {
        out.push_str("\nProviders:\n");
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Category", "Name", "Health"]);
        for p in &resp.providers {
            table.add_row(vec![
                Cell::new(&p.category),
                Cell::new(&p.name),
                Cell::new(if p.healthy { "ok" } else { "error" }),
            ]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }

    if !resp.unmet_requirements.is_empty() {
        out.push_str("\nUnmet Requirements:\n");
        for ur in &resp.unmet_requirements {
            out.push_str(&format!("  {}: {}\n", ur.factory, ur.requirement));
        }
    }
    out
}

fn format_repo_work_human(resp: &RepoWorkResponse) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", resp.path.display()));
    if let Some(slug) = &resp.slug {
        out.push_str(&format!("Slug: {slug}\n"));
    }
    out.push('\n');

    if resp.work_items.is_empty() {
        out.push_str("No work items.\n");
    } else {
        let mut table = Table::new();
        table.load_preset(UTF8_FULL_CONDENSED);
        table.set_header(vec!["Kind", "Branch", "Description"]);
        for item in &resp.work_items {
            table.add_row(vec![
                Cell::new(format!("{:?}", item.kind)),
                Cell::new(item.branch.as_deref().unwrap_or("-")),
                Cell::new(&item.description),
            ]);
        }
        out.push_str(&table.to_string());
        out.push('\n');
    }
    out
}
```

- [ ] **Step 6: Migrate existing status command**

Update `run_status` in `crates/flotilla-tui/src/cli.rs` to use `get_status()` instead of `list_repos()`:

```rust
pub async fn run_status(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path).await?;
    let status = daemon.get_status().await?;
    let output = match format {
        OutputFormat::Human => format_status_response_human(&status),
        OutputFormat::Json => flotilla_protocol::output::json_pretty(&status),
    };
    print!("{output}");
    Ok(())
}
```

Write a new `format_status_response_human` function that formats `StatusResponse`:

```rust
fn format_status_response_human(status: &StatusResponse) -> String {
    if status.repos.is_empty() {
        return "No repos tracked.\n".into();
    }
    let mut table = Table::new();
    table.load_preset(UTF8_FULL_CONDENSED);
    table.set_header(vec!["Repo", "Path", "Work Items", "Errors", "Health"]);
    for repo in &status.repos {
        let name = repo.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let health: Vec<String> = repo.provider_health.iter().flat_map(|(cat, providers)| {
            providers.iter().map(move |(name, ok)| {
                format!("{cat}/{name}: {}", if *ok { "ok" } else { "error" })
            })
        }).collect();
        let health_str = if health.is_empty() { "-".into() } else { health.join(", ") };
        table.add_row(vec![
            Cell::new(&name),
            Cell::new(repo.path.display()),
            Cell::new(repo.work_item_count),
            Cell::new(repo.error_count),
            Cell::new(&health_str),
        ]);
    }
    format!("{table}\n")
}
```

Remove the old `format_status_human` and `format_status_json` functions and their tests. The existing `status_human` and `status_json` test modules in `cli.rs::tests` (which test the old formatters with `RepoInfo` slices) must be replaced with tests for the new `format_status_response_human` function using `StatusResponse`/`RepoSummary`:

```rust
mod status_human {
    use super::*;
    use crate::cli::format_status_response_human;
    use flotilla_protocol::{RepoSummary, StatusResponse};

    #[test]
    fn empty_repos() {
        let status = StatusResponse { repos: vec![] };
        assert_eq!(format_status_response_human(&status), "No repos tracked.\n");
    }

    #[test]
    fn single_repo_with_health() {
        let status = StatusResponse {
            repos: vec![RepoSummary {
                path: PathBuf::from("/tmp/my-repo"),
                slug: Some("org/my-repo".into()),
                provider_health: health(&[("vcs", "Git", true)]),
                work_item_count: 3,
                error_count: 0,
            }],
        };
        let output = format_status_response_human(&status);
        assert!(output.contains("my-repo"), "should contain repo name");
        assert!(output.contains("3"), "should show work item count");
    }
}
```

Remove the `status_json` test module — JSON output is now `json_pretty(&status)` which is covered by serde derives. Also remove the `make_repo` and `health` helpers if they become unused (check if `health` is still used by remaining tests first).

- [ ] **Step 7: Verify it compiles**

Run: `cargo build`
Expected: compiles cleanly

- [ ] **Step 8: Commit**

```bash
git add src/main.rs crates/flotilla-tui/src/cli.rs crates/flotilla-tui/Cargo.toml Cargo.lock
git commit -m "feat: add CLI query subcommands with human/JSON formatting (#282)"
```

---

### Task 12: End-to-end verification

**Files:**
- No new files — testing the integrated system

- [ ] **Step 1: Run full test suite**

Run: `cargo test --locked`
Expected: all tests pass

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`
Expected: no warnings

- [ ] **Step 3: Run formatter**

Run: `cargo +nightly fmt`
Expected: formats cleanly

- [ ] **Step 4: Manual smoke test (if daemon can run)**

```bash
# Start daemon in background
cargo run -- daemon &
DAEMON_PID=$!
sleep 2

# Test status
cargo run -- status
cargo run -- status --json

# Test repo commands (use current repo name)
cargo run -- repo flotilla
cargo run -- repo flotilla --json
cargo run -- repo flotilla providers
cargo run -- repo flotilla providers --json
cargo run -- repo flotilla work
cargo run -- repo flotilla work --json

# Test slug resolution error
cargo run -- repo nonexistent 2>&1 || true

kill $DAEMON_PID
```

- [ ] **Step 5: Final commit if any fixes needed**

```bash
git add -A
git commit -m "fix: address review findings from end-to-end testing (#282)"
```
