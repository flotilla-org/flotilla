# Provider Configuration and Naming Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `ProviderCategory` enum, split `ProviderDescriptor` into backend/implementation, rename CodeReview→ChangeRequestTracker, add per-category config with `prefer_by_backend`/`prefer_by_implementation` ordering, and unify discovery to use `probe_all` everywhere.

**Architecture:** Four sequential phases: (1) rename CodeReview→ChangeRequestTracker throughout the codebase, (2) add ProviderCategory enum and restructure ProviderDescriptor, (3) pluralize remaining registry fields, (4) add config types, ProviderSet ordering, and discovery changes. Each phase produces a compiling, test-passing commit.

**Tech Stack:** Rust, serde (TOML deserialization), indexmap, async-trait

**Spec:** `docs/superpowers/specs/2026-03-15-provider-config-and-naming-design.md`

---

## Chunk 1: CodeReview → ChangeRequestTracker Rename

Pure mechanical rename. Gets naming right before building new abstractions on top.

### Task 1: Rename trait, module, and core references

**Files:**
- Rename: `crates/flotilla-core/src/providers/code_review/` → `crates/flotilla-core/src/providers/change_request/`
- Modify: `crates/flotilla-core/src/providers/change_request/mod.rs`
- Modify: `crates/flotilla-core/src/providers/change_request/github.rs`
- Modify: `crates/flotilla-core/src/providers/mod.rs`

- [ ] **Step 1: Rename the module directory**

```bash
mv crates/flotilla-core/src/providers/code_review crates/flotilla-core/src/providers/change_request
```

- [ ] **Step 2: Rename the trait in mod.rs**

In `crates/flotilla-core/src/providers/change_request/mod.rs`, rename `trait CodeReview` to `trait ChangeRequestTracker`.

- [ ] **Step 3: Update module declaration in providers/mod.rs**

Change `pub mod code_review;` to `pub mod change_request;`.

- [ ] **Step 4: Update the GitHub implementation**

In `crates/flotilla-core/src/providers/change_request/github.rs`:
- Rename `GitHubCodeReview` struct to `GitHubChangeRequest`
- Update `impl CodeReview for GitHubCodeReview` to `impl ChangeRequestTracker for GitHubChangeRequest`
- Update `super::CodeReview` import to `super::ChangeRequestTracker`

- [ ] **Step 5: Verify build compiles (expect errors in downstream files)**

Run: `cargo build 2>&1 | grep "^error" | wc -l`
Expected: errors from files that still reference `code_review` module and `CodeReview` trait

### Task 2: Update registry and discovery references

**Files:**
- Modify: `crates/flotilla-core/src/providers/registry.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/github.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/mod.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`

- [ ] **Step 1: Update registry.rs**

- Change import `code_review::CodeReview` → `change_request::ChangeRequestTracker`
- Change field `code_review: ProviderSet<dyn CodeReview>` → `change_requests: ProviderSet<dyn ChangeRequestTracker>`

- [ ] **Step 2: Update discovery/mod.rs**

- Change import `code_review::CodeReview` → `change_request::ChangeRequestTracker`
- Change type alias `CodeReviewFactory` → `ChangeRequestFactory` with `Output = dyn ChangeRequestTracker`
- Change `FactoryRegistry` field `code_review` → `change_requests`
- Update `discover_providers` to use `registry.change_requests`
- Update `is_follower` check from `self.factories.code_review.is_empty()` → `self.factories.change_requests.is_empty()`

- [ ] **Step 3: Update discovery/factories/github.rs**

- Rename `GitHubCodeReviewFactory` → `GitHubChangeRequestFactory`
- Update import from `code_review::CodeReview` → `change_request::ChangeRequestTracker`
- Update `Factory` impl type `Output = dyn CodeReview` → `Output = dyn ChangeRequestTracker`
- Update descriptor name if needed

- [ ] **Step 4: Update discovery/factories/mod.rs**

- Change `claude::` etc. references as needed
- Change `code_review:` field in `default_all()` and `for_follower()` → `change_requests:`
- Update test assertions for the field name

- [ ] **Step 5: Update discovery/test_support.rs**

- Rename `FakeCodeReview` → `FakeChangeRequest`
- Rename `FakeCodeReviewFactory` → `FakeChangeRequestFactory`
- Update all trait impl references

### Task 3: Update consumers (model, refresh, executor, in_process, data)

**Files:**
- Modify: `crates/flotilla-core/src/model.rs`
- Modify: `crates/flotilla-core/src/refresh.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/data.rs` (SectionLabels.code_review field)
- Modify: `crates/flotilla-core/src/host_summary.rs` (if references exist)

- [ ] **Step 1: Update model.rs**

- Change `registry.code_review` → `registry.change_requests` in `labels_from_registry()` and `provider_names_from_registry()`
- Change category string `"code_review"` → `"change_request"`

- [ ] **Step 2: Update refresh.rs**

- Change `registry.code_review` → `registry.change_requests` in `refresh_providers()` and `compute_provider_health()`
- Change category string `"code_review"` → `"change_request"`
- Update test code

- [ ] **Step 3: Update data.rs**

- Change `SectionLabels` field `code_review` → `change_requests` and all references.

- [ ] **Step 4: Update executor.rs**

- Change `registry.code_review` → `registry.change_requests` in all executor functions
- Update test code

- [ ] **Step 5: Update in_process.rs**

- Change any `code_review` references to `change_requests`
- Update test code

### Task 4: Update protocol types and remaining references

Use `cargo build` and `grep -r code_review crates/` to find all remaining references. Key files include:

**Files:**
- Modify: `crates/flotilla-protocol/src/snapshot.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Modify: `crates/flotilla-protocol/src/query.rs` (comment)
- Modify: `crates/flotilla-core/src/convert.rs`
- Modify: `crates/flotilla-tui/src/ui.rs`
- Modify: `crates/flotilla-tui/src/app/mod.rs`
- Modify: `crates/flotilla-tui/src/app/intent.rs`
- Modify: `crates/flotilla-tui/src/cli.rs`
- Modify: `crates/flotilla-tui/tests/snapshots.rs`
- Modify: `crates/flotilla-tui/tests/support/mod.rs` (if references exist)
- Modify: `crates/flotilla-daemon/tests/multi_host.rs`
- Modify: `crates/flotilla-core/tests/in_process_daemon.rs`
- Modify: `examples/debug_sessions.rs`

- [ ] **Step 1: Update protocol RepoLabels**

In `crates/flotilla-protocol/src/snapshot.rs`, change `code_review: CategoryLabels` → `change_requests: CategoryLabels`.

- [ ] **Step 2: Update flotilla-protocol/src/lib.rs**

Change any `"code_review"` string literals.

- [ ] **Step 3: Update flotilla-tui references**

Grep for `code_review` across `crates/flotilla-tui/` and update all references in `ui.rs`, `app/mod.rs`, `cli.rs`.

- [ ] **Step 4: Update convert.rs**

Change `code_review` references in core-to-protocol conversion.

- [ ] **Step 5: Update all test files**

- `crates/flotilla-tui/tests/snapshots.rs` — update string literals and field names
- `crates/flotilla-daemon/tests/multi_host.rs` — update string literals
- `crates/flotilla-core/tests/in_process_daemon.rs` — update string literals

- [ ] **Step 6: Build, test, lint**

Run:
```bash
cargo build 2>&1 | tail -3
cargo test --workspace 2>&1 | grep "^test result:"
cargo clippy --all-targets --locked -- -D warnings 2>&1 | tail -3
```
Expected: all pass

- [ ] **Step 7: Format and commit**

```bash
cargo +nightly-2026-03-12 fmt
git add -A
git commit -m "refactor: rename CodeReview to ChangeRequestTracker

Rename trait, module, registry field, factory types, and all
references from code_review/CodeReview to change_request/
ChangeRequestTracker. The capability is managing change requests
(PRs, MRs), not just code review."
```

---

## Chunk 2: ProviderCategory Enum and ProviderDescriptor Restructure

### Task 5: Define ProviderCategory enum

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`

- [ ] **Step 1: Write tests for ProviderCategory**

Add to the test module in `discovery/mod.rs`:

```rust
#[test]
fn provider_category_slug_round_trip() {
    use super::ProviderCategory;
    let categories = [
        (ProviderCategory::Vcs, "vcs"),
        (ProviderCategory::CheckoutManager, "checkout_manager"),
        (ProviderCategory::ChangeRequest, "change_request"),
        (ProviderCategory::IssueTracker, "issue_tracker"),
        (ProviderCategory::CloudAgent, "cloud_agent"),
        (ProviderCategory::AiUtility, "ai_utility"),
        (ProviderCategory::WorkspaceManager, "workspace_manager"),
        (ProviderCategory::TerminalPool, "terminal_pool"),
    ];
    for (cat, expected_slug) in categories {
        assert_eq!(cat.slug(), expected_slug);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flotilla-core provider_category_slug 2>&1 | tail -5`
Expected: FAIL — `ProviderCategory` doesn't exist yet

- [ ] **Step 3: Implement ProviderCategory enum**

Add to `crates/flotilla-core/src/providers/discovery/mod.rs`, before `ProviderDescriptor`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderCategory {
    Vcs,
    CheckoutManager,
    ChangeRequest,
    IssueTracker,
    CloudAgent,
    AiUtility,
    WorkspaceManager,
    TerminalPool,
}

impl ProviderCategory {
    pub fn slug(&self) -> &'static str {
        match self {
            Self::Vcs => "vcs",
            Self::CheckoutManager => "checkout_manager",
            Self::ChangeRequest => "change_request",
            Self::IssueTracker => "issue_tracker",
            Self::CloudAgent => "cloud_agent",
            Self::AiUtility => "ai_utility",
            Self::WorkspaceManager => "workspace_manager",
            Self::TerminalPool => "terminal_pool",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Vcs => "VCS",
            Self::CheckoutManager => "Checkout Manager",
            Self::ChangeRequest => "Change Requests",
            Self::IssueTracker => "Issue Tracker",
            Self::CloudAgent => "Cloud Agent",
            Self::AiUtility => "AI Utility",
            Self::WorkspaceManager => "Workspace Manager",
            Self::TerminalPool => "Terminal Pool",
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flotilla-core provider_category_slug 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
cargo +nightly-2026-03-12 fmt
git add crates/flotilla-core/src/providers/discovery/mod.rs
git commit -m "feat: add ProviderCategory enum"
```

### Task 6: Restructure ProviderDescriptor

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`

- [ ] **Step 1: Update ProviderDescriptor struct and constructors**

Replace the struct and constructors:

```rust
pub struct ProviderDescriptor {
    pub category: ProviderCategory,
    pub backend: String,
    pub implementation: String,
    pub display_name: String,
    pub abbreviation: String,
    pub section_label: String,
    pub item_noun: String,
}

impl ProviderDescriptor {
    /// For single-implementation backends where backend == implementation.
    pub fn named(category: ProviderCategory, name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            category,
            backend: name.clone(),
            implementation: name.clone(),
            display_name: name,
            abbreviation: String::new(),
            section_label: String::new(),
            item_noun: String::new(),
        }
    }

    /// Full descriptor with distinct backend and implementation.
    pub fn labeled(
        category: ProviderCategory,
        backend: impl Into<String>,
        implementation: impl Into<String>,
        display_name: impl Into<String>,
        abbreviation: impl Into<String>,
        section_label: impl Into<String>,
        item_noun: impl Into<String>,
    ) -> Self {
        Self {
            category,
            backend: backend.into(),
            implementation: implementation.into(),
            display_name: display_name.into(),
            abbreviation: abbreviation.into(),
            section_label: section_label.into(),
            item_noun: item_noun.into(),
        }
    }

    /// Shorthand for backends with a single implementation (implementation = backend).
    pub fn labeled_simple(
        category: ProviderCategory,
        backend: impl Into<String>,
        display_name: impl Into<String>,
        abbreviation: impl Into<String>,
        section_label: impl Into<String>,
        item_noun: impl Into<String>,
    ) -> Self {
        let backend = backend.into();
        Self {
            category,
            implementation: backend.clone(),
            backend,
            display_name: display_name.into(),
            abbreviation: abbreviation.into(),
            section_label: section_label.into(),
            item_noun: item_noun.into(),
        }
    }
}
```

- [ ] **Step 2: Update probe_all/probe_first error paths**

In `discover_providers` (discovery/mod.rs), `probe_all` and `probe_first` use `factory.descriptor().name.clone()` for error reporting. Change to `factory.descriptor().implementation.clone()` (or `backend` — the key used for unmet requirement reporting).

- [ ] **Step 3: Compile to identify all call sites that need updating**

Run: `cargo build 2>&1 | grep "^error" | head -20`
Expected: errors at all `ProviderDescriptor::named()` and `labeled()` call sites (missing `category` arg)

### Task 7: Update all factory descriptors

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/factories/git.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/github.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/claude.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/cursor.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/codex.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/cmux.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/tmux.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/zellij.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/shpool.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/passthrough.rs`

- [ ] **Step 1: Update git.rs factories**

Use `ProviderCategory` import. Update descriptors:
- `GitVcsFactory`: `labeled_simple(ProviderCategory::Vcs, "git", "Git", "", "", "")`
- `WtCheckoutManagerFactory`: `labeled(ProviderCategory::CheckoutManager, "git", "wt", "wt", "CO", "Checkouts", "checkout")`
- `GitCheckoutManagerFactory`: `labeled(ProviderCategory::CheckoutManager, "git", "git", "git worktrees", "WT", "Checkouts", "worktree")`

- [ ] **Step 2: Update github.rs factories**

- `GitHubChangeRequestFactory`: `labeled_simple(ProviderCategory::ChangeRequest, "github", "GitHub Pull Requests", "PR", "Pull Requests", "pull request")`
- `GitHubIssueTrackerFactory`: `labeled_simple(ProviderCategory::IssueTracker, "github", "GitHub Issues", "#", "Issues", "issue")`

- [ ] **Step 3: Update claude.rs factories**

- `ClaudeCodingAgentFactory`: `labeled_simple(ProviderCategory::CloudAgent, "claude", "Claude", "S", "Sessions", "session")`
- `ClaudeApiAiUtilityFactory`: `labeled(ProviderCategory::AiUtility, "claude", "api", "Claude API", "", "", "")`
- `ClaudeCliAiUtilityFactory`: `labeled(ProviderCategory::AiUtility, "claude", "cli", "Claude CLI", "", "", "")`

- [ ] **Step 4: Update cursor.rs, codex.rs factories**

- `CursorCodingAgentFactory`: `labeled_simple(ProviderCategory::CloudAgent, "cursor", "Cursor", "S", "Sessions", "session")`
- `CodexCodingAgentFactory`: `labeled_simple(ProviderCategory::CloudAgent, "codex", "Codex", "S", "Sessions", "session")`

- [ ] **Step 5: Update workspace manager factories (cmux.rs, tmux.rs, zellij.rs)**

- cmux: `labeled_simple(ProviderCategory::WorkspaceManager, "cmux", "cmux Workspaces", "", "", "")`
- tmux: `labeled_simple(ProviderCategory::WorkspaceManager, "tmux", "tmux Workspaces", "", "", "")`
- zellij: `labeled_simple(ProviderCategory::WorkspaceManager, "zellij", "zellij Workspaces", "", "", "")`

- [ ] **Step 6: Update terminal pool factories (shpool.rs, passthrough.rs)**

- shpool: `named(ProviderCategory::TerminalPool, "shpool")`
- passthrough: `named(ProviderCategory::TerminalPool, "passthrough")`

### Task 8: Update ProviderSet key and all insert sites

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`

- [ ] **Step 1: Update all insert calls to use `desc.implementation.clone()`**

In `discover_providers`, change all closures from:
```rust
|desc, provider| { registry.X.insert(desc.name.clone(), desc, provider); }
```
to:
```rust
|desc, provider| { registry.X.insert(desc.implementation.clone(), desc, provider); }
```

Same for the checkout_managers probe_first if-let.

- [ ] **Step 2: Update ProviderDescriptor test assertions**

Update any tests that check `desc.name` to check `desc.backend` and `desc.implementation` instead.

### Task 9: Update test helpers and remaining consumers

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs`
- Modify: `crates/flotilla-core/src/refresh.rs` (test helper `desc()`)
- Modify: `crates/flotilla-core/src/executor.rs` (test helper `desc()`)
- Modify: `crates/flotilla-core/src/model.rs` (test helpers)

- [ ] **Step 1: Update test_support.rs factory descriptors**

Update `FakeVcsFactory`, `FakeChangeRequestFactory`, `FakeIssueTrackerFactory` descriptors to include `category`, `backend`, `implementation`.

- [ ] **Step 2: Update refresh.rs test helpers**

The `desc()` helper likely uses `ProviderDescriptor::named()` — add `category` parameter or use a specific category.

- [ ] **Step 3: Update executor.rs test helpers**

Same pattern — update `desc()` helper.

- [ ] **Step 4: Update model.rs test helpers**

Same pattern — update `labeled_desc()` and any other helpers.

### Task 10: Replace hardcoded category strings with ProviderCategory

**Files:**
- Modify: `crates/flotilla-core/src/providers/registry.rs` (`provider_infos()`)
- Modify: `crates/flotilla-core/src/model.rs` (`provider_names_from_registry()`)
- Modify: `crates/flotilla-core/src/refresh.rs` (`compute_provider_health()`)

- [ ] **Step 1: Update provider_infos() in registry.rs**

Replace hardcoded strings with `desc.category.slug()` — since each entry's descriptor now carries its category:

```rust
pub fn provider_infos(&self) -> Vec<(String, String)> {
    let mut infos = Vec::new();
    fn collect<T: ?Sized>(infos: &mut Vec<(String, String)>, set: &ProviderSet<T>) {
        for (desc, _) in set.iter() {
            infos.push((desc.category.slug().into(), desc.display_name.clone()));
        }
    }
    collect(&mut infos, &self.vcs);
    collect(&mut infos, &self.checkout_managers);
    collect(&mut infos, &self.change_requests);
    collect(&mut infos, &self.issue_trackers);
    collect(&mut infos, &self.cloud_agents);
    collect(&mut infos, &self.ai_utilities);
    collect(&mut infos, &self.workspace_managers);
    collect(&mut infos, &self.terminal_pools);
    infos
}
```

- [ ] **Step 2: Update provider_names_from_registry() in model.rs**

Replace hardcoded category strings. The `collect_names` helper can use `desc.category.slug()` from the first entry:

```rust
fn collect_names<T: ?Sized>(names: &mut HashMap<String, Vec<String>>, set: &ProviderSet<T>) {
    if let Some((desc, _)) = set.preferred_with_desc() {
        let list: Vec<String> = set.display_names().map(|s| s.to_string()).collect();
        if !list.is_empty() {
            names.insert(desc.category.slug().into(), list);
        }
    }
}
```

- [ ] **Step 3: Update compute_provider_health() in refresh.rs**

Replace hardcoded category strings with `ProviderCategory::X.slug()`.

- [ ] **Step 4: Build, test, lint, format, commit**

```bash
cargo build && cargo test --workspace && cargo clippy --all-targets --locked -- -D warnings
cargo +nightly-2026-03-12 fmt
git add -A
git commit -m "feat: add ProviderCategory enum and restructure ProviderDescriptor

Replace ProviderDescriptor.name with category/backend/implementation
fields. Add ProviderCategory enum replacing hardcoded category strings
in provider_infos, provider_names, and health computation."
```

---

## Chunk 3: Registry Field Pluralization

### Task 11: Pluralize workspace_manager and terminal_pool

**Files:**
- Modify: `crates/flotilla-core/src/providers/registry.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`
- Modify: `crates/flotilla-core/src/model.rs`
- Modify: `crates/flotilla-core/src/refresh.rs`
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/in_process.rs`
- Modify: `examples/debug_sessions.rs`

- [ ] **Step 1: Rename fields in registry.rs**

Change `workspace_manager` → `workspace_managers` and `terminal_pool` → `terminal_pools` in `ProviderRegistry` struct and `new()`.

- [ ] **Step 2: Compile to find all consumer sites**

Run: `cargo build 2>&1 | grep "^error" | head -30`

- [ ] **Step 3: Update discovery/mod.rs**

Change `registry.workspace_manager` → `registry.workspace_managers` and `registry.terminal_pool` → `registry.terminal_pools`.

- [ ] **Step 4: Update model.rs**

Change field references in `provider_names_from_registry()` and `labels_from_registry()`.

- [ ] **Step 5: Update refresh.rs**

Change field references in `refresh_providers()` and `compute_provider_health()`.

- [ ] **Step 6: Update executor.rs**

Change all `registry.workspace_manager` → `registry.workspace_managers` and `registry.terminal_pool` → `registry.terminal_pools` references.

- [ ] **Step 7: Update in_process.rs**

Change any references.

- [ ] **Step 8: Update examples and remaining files**

Update `examples/debug_sessions.rs` and any other references found by the compiler.

- [ ] **Step 9: Update test code across all files**

Fix all test code that constructs registries with the old field names.

- [ ] **Step 10: Build, test, lint, format, commit**

```bash
cargo build && cargo test --workspace && cargo clippy --all-targets --locked -- -D warnings
cargo +nightly-2026-03-12 fmt
git add -A
git commit -m "refactor: pluralize workspace_manager and terminal_pool registry fields

All ProviderSet fields now use consistent plural naming:
workspace_managers and terminal_pools."
```

---

## Chunk 4: Config, ProviderSet Ordering, and Discovery Changes

### Task 12: Add prefer_by_backend and prefer_by_implementation to ProviderSet

**Files:**
- Modify: `crates/flotilla-core/src/providers/registry.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn prefer_by_backend_reorders() {
    use super::*;
    use crate::providers::discovery::ProviderCategory;

    let mut set = ProviderSet::<dyn std::fmt::Debug>::new();
    // Would need a concrete test type — use the pattern from existing tests
    // Test that prefer_by_backend moves the matching entry to first position
}

#[test]
fn prefer_by_backend_noop_when_not_found() { ... }

#[test]
fn prefer_by_implementation_reorders() { ... }
```

Note: Since `ProviderSet` is generic over `T: ?Sized`, tests will need a concrete trait. Use `AiUtility` or create a test-only trait. Follow existing test patterns in the file.

- [ ] **Step 2: Implement prefer_by_backend**

Add to `ProviderSet<T>`:

```rust
/// Reorder so that the first entry whose descriptor.backend matches is first.
/// No-op if no entry matches.
pub fn prefer_by_backend(&mut self, backend: &str) {
    if let Some(idx) = self.inner.values().position(|(desc, _)| desc.backend == backend) {
        if idx > 0 {
            self.inner.move_index(idx, 0);
        }
    }
}

/// Reorder so that the entry with the given implementation key is first.
/// No-op if no entry matches.
pub fn prefer_by_implementation(&mut self, implementation: &str) {
    if let Some(idx) = self.inner.get_index_of(implementation) {
        if idx > 0 {
            self.inner.move_index(idx, 0);
        }
    }
}
```

- [ ] **Step 3: Run tests, verify pass**

Run: `cargo test -p flotilla-core prefer_by 2>&1 | tail -5`

- [ ] **Step 4: Commit**

```bash
cargo +nightly-2026-03-12 fmt
git add crates/flotilla-core/src/providers/registry.rs
git commit -m "feat: add prefer_by_backend and prefer_by_implementation to ProviderSet"
```

### Task 13: Add config types for provider preferences

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`

- [ ] **Step 1: Add ProviderPreference and category config types**

```rust
/// Per-category provider preference.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProviderPreference {
    pub backend: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChangeRequestConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IssueTrackerConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct CloudAgentConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AiUtilityConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
    pub claude: Option<ClaudeAiUtilityConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClaudeAiUtilityConfig {
    pub implementation: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct WorkspaceManagerConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TerminalPoolConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
}
```

- [ ] **Step 2: Add fields to FlotillaConfig**

```rust
pub struct FlotillaConfig {
    #[serde(default)]
    pub vcs: VcsConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub change_request: ChangeRequestConfig,
    #[serde(default)]
    pub issue_tracker: IssueTrackerConfig,
    #[serde(default)]
    pub cloud_agent: CloudAgentConfig,
    #[serde(default)]
    pub ai_utility: AiUtilityConfig,
    #[serde(default)]
    pub workspace_manager: WorkspaceManagerConfig,
    #[serde(default)]
    pub terminal_pool: TerminalPoolConfig,
}
```

- [ ] **Step 3: Migrate checkout config**

Flatten `CheckoutsConfig` fields into `GitConfig`:

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GitConfig {
    #[serde(default = "default_checkout_strategy")]
    pub checkout_strategy: String,
    #[serde(default = "default_checkout_path")]
    pub checkout_path: String,
}
```

Update `RepoGitConfig` and `RepoCheckoutsOverride` → `RepoGitOverride` similarly. Update `resolve_checkouts_config` to read from the new fields.

- [ ] **Step 4: Write config parsing test**

Test that TOML with new structure deserializes correctly:

```rust
#[test]
fn parse_config_with_provider_preferences() {
    let toml = r#"
[ai_utility]
backend = "claude"

[ai_utility.claude]
implementation = "api"

[workspace_manager]
backend = "zellij"

[vcs.git]
checkout_strategy = "wt"
checkout_path = "/tmp/{{ branch }}"
"#;
    let config: FlotillaConfig = toml::from_str(toml).unwrap();
    assert_eq!(config.ai_utility.preference.backend.as_deref(), Some("claude"));
    assert_eq!(config.ai_utility.claude.unwrap().implementation.as_deref(), Some("api"));
    assert_eq!(config.workspace_manager.preference.backend.as_deref(), Some("zellij"));
    assert_eq!(config.vcs.git.checkout_strategy, "wt");
}
```

- [ ] **Step 5: Build, test, commit**

```bash
cargo build && cargo test -p flotilla-core 2>&1 | grep "^test result:"
cargo +nightly-2026-03-12 fmt
git add crates/flotilla-core/src/config.rs
git commit -m "feat: add per-category provider preference config types

Add ProviderPreference with optional backend field for each category.
Migrate checkout config from vcs.git.checkouts.{provider,path} to
vcs.git.{checkout_strategy,checkout_path}."
```

### Task 14: Switch to probe_all everywhere and apply config preferences

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs`
- Modify: `crates/flotilla-core/src/providers/discovery/factories/git.rs`
- Modify: `crates/flotilla-core/src/refresh.rs`

- [ ] **Step 1: Switch checkout_managers to probe_all**

In `discover_providers`, replace:
```rust
if let Some((desc, provider)) = probe_first(&factories.checkout_managers, ...).await {
    registry.checkout_managers.insert(desc.name.clone(), desc, provider);
}
```
with:
```rust
probe_all(&factories.checkout_managers, &combined, config, repo_root, &runner, &mut unmet, |desc, provider| {
    registry.checkout_managers.insert(desc.implementation.clone(), desc, provider);
}).await;
```

- [ ] **Step 2: Switch workspace_managers and terminal_pools to probe_all**

Same pattern — replace `probe_first` with `probe_all` and insert closure.

- [ ] **Step 3: Remove config-reading from checkout factories**

In `crates/flotilla-core/src/providers/discovery/factories/git.rs`, remove only the `if provider != "auto" && provider != "wt"` / `"git"` guard clauses from `WtCheckoutManagerFactory::probe()` and `GitCheckoutManagerFactory::probe()`. Keep the `resolve_checkouts_config` call that reads the path template — the `GitCheckoutManager` constructor still needs it. Only the provider-selection guards are removed.

- [ ] **Step 4: Apply config preferences after probing**

In `discover_providers`, after all probes complete and before returning, add:

```rust
// Apply config preferences
let global_config = config.load_config();

if let Some(backend) = global_config.change_request.preference.backend.as_deref() {
    registry.change_requests.prefer_by_backend(backend);
}
if let Some(backend) = global_config.issue_tracker.preference.backend.as_deref() {
    registry.issue_trackers.prefer_by_backend(backend);
}
if let Some(backend) = global_config.cloud_agent.preference.backend.as_deref() {
    registry.cloud_agents.prefer_by_backend(backend);
}
if let Some(backend) = global_config.ai_utility.preference.backend.as_deref() {
    registry.ai_utilities.prefer_by_backend(backend);
}
if let Some(impl_name) = global_config.ai_utility.claude.as_ref().and_then(|c| c.implementation.as_deref()) {
    registry.ai_utilities.prefer_by_implementation(impl_name);
}
if let Some(backend) = global_config.workspace_manager.preference.backend.as_deref() {
    registry.workspace_managers.prefer_by_backend(backend);
}
if let Some(backend) = global_config.terminal_pool.preference.backend.as_deref() {
    registry.terminal_pools.prefer_by_backend(backend);
}

// Checkout strategy (nested under vcs.git)
let checkout_config = config.resolve_checkouts_config(repo_root);
let strategy = checkout_config.checkout_strategy.as_str();
if strategy != "auto" {
    registry.checkout_managers.prefer_by_implementation(strategy);
}
```

- [ ] **Step 5: Update refresh to use preferred() for checkout_managers**

In `crates/flotilla-core/src/refresh.rs`, change `refresh_providers()` to use `preferred_with_desc()` for checkout managers instead of `iter()`, matching the workspace/terminal pattern:

```rust
let checkouts_fut = async {
    if let Some((desc, cm)) = registry.checkout_managers.preferred_with_desc() {
        let name = desc.display_name.clone();
        match cm.list_checkouts(repo_root).await {
            Ok(entries) => (entries, vec![]),
            Err(e) => (vec![], vec![(name, e)]),
        }
    } else {
        (vec![], vec![])
    }
};
```

- [ ] **Step 6: Update tests that assert checkout_managers.len() == 1**

Find and update any tests that assert only one checkout manager is registered. They should now expect multiple when both wt and git are available.

- [ ] **Step 7: Add integration test for config preference reordering**

Write a test that constructs a `ConfigStore` with e.g. `workspace_manager.backend = "tmux"`, runs discovery with multiple workspace manager factories, and asserts that `registry.workspace_managers.preferred()` returns the tmux provider even though it's not first in factory registration order.

- [ ] **Step 8: Remove probe_first function if no longer called**

If `probe_first` has no remaining callers, remove it.

- [ ] **Step 9: Build, test, lint, format, commit**

```bash
cargo build && cargo test --workspace && cargo clippy --all-targets --locked -- -D warnings
cargo +nightly-2026-03-12 fmt
git add -A
git commit -m "feat: config-driven provider selection with probe_all everywhere

All categories now use probe_all — every available provider registers.
Config preferences (backend, implementation) reorder ProviderSets via
prefer_by_backend/prefer_by_implementation. Checkout managers read
config from vcs.git.checkout_strategy instead of self-filtering in
factory probe methods. Refresh uses preferred() for checkout managers
to avoid duplicate data."
```
