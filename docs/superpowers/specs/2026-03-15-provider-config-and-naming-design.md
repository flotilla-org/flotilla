# Provider Configuration and Naming Model

## Problem

Provider identity conflates three distinct concepts: what capability is provided (VCS, AI utility), what backend technology delivers it (GitHub, Claude), and what implementation route our code uses (API, CLI). Category strings are scattered hardcoded literals with no central enum. Only checkout managers read config for provider selection — all other categories are environment-driven with no user override. The `CodeReview` name misrepresents the capability (it tracks change requests, not just reviews). Registry field naming is inconsistent between singular and plural.

## Design

### 1. ProviderCategory Enum

Replace all hardcoded category strings with a central enum in the discovery module:

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
```

Each variant exposes:
- `slug()` → `&'static str` — config key and health map key (`"vcs"`, `"change_request"`, etc.)
- `display_name()` → `&'static str` — human-readable label (`"VCS"`, `"Change Requests"`, etc.)

Note: `CheckoutManager` has its own category internally (separate trait, `ProviderSet`, probe cycle) but its config lives under `[vcs.git]` since checkout management is a VCS sub-concern. The slug `"checkout_manager"` is used for health keys and display, not as a config section key.

Used by `ProviderDescriptor`, `provider_infos()`, `provider_names_from_registry()`, `compute_provider_health()`, and config deserialization.

### 2. ProviderDescriptor: Backend and Implementation

Replace the single `name` field with explicit `backend` and `implementation` fields, and add `category`:

```rust
pub struct ProviderDescriptor {
    pub category: ProviderCategory,
    pub backend: String,          // "claude", "github", "git", "tmux", etc.
    pub implementation: String,   // "api", "cli", "wt", "git", etc.
    pub display_name: String,
    pub abbreviation: String,
    pub section_label: String,
    pub item_noun: String,
}
```

`ProviderSet` keys on `implementation` (unique within a category). All `insert()` call sites migrate from `desc.name.clone()` to `desc.implementation.clone()` as the key. When only one implementation exists for a backend, `implementation` equals `backend` (e.g. `"github"` / `"github"`).

The existing convenience constructors migrate:
- `ProviderDescriptor::named(name)` → sets both `backend` and `implementation` to `name`, requires `category`
- `ProviderDescriptor::labeled(name, display_name, abbr, section, noun)` → becomes `labeled(category, backend, implementation, display_name, abbr, section, noun)`. For single-implementation backends, a shorter form `labeled_simple(category, backend, display_name, abbr, section, noun)` sets `implementation = backend`.

Current provider mappings:

| Category | Backend | Implementation | Display Name |
|----------|---------|---------------|-------------|
| Vcs | git | git | Git |
| CheckoutManager | git | wt | wt |
| CheckoutManager | git | git | git worktrees |
| ChangeRequest | github | github | GitHub Pull Requests |
| IssueTracker | github | github | GitHub Issues |
| CloudAgent | claude | claude | Claude |
| CloudAgent | cursor | cursor | Cursor |
| CloudAgent | codex | codex | Codex |
| AiUtility | claude | api | Claude API |
| AiUtility | claude | cli | Claude CLI |
| WorkspaceManager | cmux | cmux | cmux Workspaces |
| WorkspaceManager | zellij | zellij | zellij Workspaces |
| WorkspaceManager | tmux | tmux | tmux Workspaces |
| TerminalPool | shpool | shpool | shpool |
| TerminalPool | passthrough | passthrough | passthrough |

### 3. CodeReview → ChangeRequestTracker Rename

The capability is managing change requests (PRs, MRs), not just code review.

| Before | After |
|--------|-------|
| Trait: `CodeReview` | `ChangeRequestTracker` |
| Module: `code_review/` | `change_request/` |
| Registry field: `code_review` | `change_requests` |
| Factory type alias: `CodeReviewFactory` | `ChangeRequestFactory` |
| Factory impl: `GitHubCodeReviewFactory` | `GitHubChangeRequestFactory` |
| Test support: `FakeCodeReview` | `FakeChangeRequest` |
| Test support: `FakeCodeReviewFactory` | `FakeChangeRequestFactory` |
| `FactoryRegistry` field: `code_review` | `change_requests` |
| Category: `"code_review"` | `ChangeRequest` (slug: `"change_request"`) |
| Config key: `code_review` | `change_request` |
| Protocol: `RepoLabels.code_review` | `RepoLabels.change_requests` |

This is a protocol-breaking change to `RepoLabels` and `provider_names` keys in `RepoInfo`. Acceptable in the no-backwards-compat phase.

### 4. Registry Field Pluralization

All `ProviderSet` fields become consistently plural. In `ProviderRegistry`:

| Before | After |
|--------|-------|
| `workspace_manager: ProviderSet` | `workspace_managers: ProviderSet` |
| `terminal_pool: ProviderSet` | `terminal_pools: ProviderSet` |

Note: `FactoryRegistry` already uses plural for `workspace_managers` and `terminal_pools`, so only `ProviderRegistry` needs pluralization updates for those two fields. The `code_review` → `change_requests` rename in Section 3 applies to both registries.

### 5. Config Structure

Each category gets an optional top-level section in `~/.config/flotilla/config.toml`. The `backend` field selects which backend is preferred. Backend-specific sections hold backend config and an optional `implementation` override.

```toml
[change_request]
backend = "github"

[change_request.github]
# github-specific config later

[issue_tracker]
backend = "github"

[cloud_agent]
backend = "claude"

[ai_utility]
backend = "claude"

[ai_utility.claude]
implementation = "api"    # optional: force api over cli

[workspace_manager]
backend = "zellij"

[terminal_pool]
backend = "shpool"
```

All fields are optional. Omitting `backend` means auto-selection (current behavior: first successful probe wins).

**Checkout manager config** remains nested under VCS since checkout management is inherently a VCS sub-concern. The current `vcs.git.checkouts` nesting level is flattened:

```toml
[vcs]
backend = "git"

[vcs.git]
checkout_strategy = "wt"    # "wt", "git", or "auto"
checkout_path = "{{ repo_path }}/../{{ repo }}.{{ branch | sanitize }}"
```

This replaces the current `vcs.git.checkouts.provider` → `vcs.git.checkout_strategy` and `vcs.git.checkouts.path` → `vcs.git.checkout_path`, removing the intermediate `checkouts` table.

**Per-repo overrides** in `~/.config/flotilla/repos/{slug}.toml` follow the same structure, with all fields optional to allow selective overrides.

### 6. Config Types

```rust
/// Per-category provider preference.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProviderPreference {
    pub backend: Option<String>,
}

/// Category-specific config sections.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChangeRequestConfig {
    #[serde(flatten)]
    pub preference: ProviderPreference,
    pub github: Option<GitHubChangeRequestConfig>,
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

// etc. for each category that needs backend-specific config
```

Categories with no backend-specific config yet still get a section type with just `ProviderPreference`, so adding config later is non-breaking.

### 7. ProviderSet Ordering

Discovery continues to probe and register all successful providers. Config preferences affect ordering, not membership.

After discovery completes:
1. For each category, read the config's `backend` and optional `implementation` preferences
2. Reorder the `ProviderSet` so the preferred provider is first
3. `preferred()` continues to return the first entry — all existing call sites unchanged
4. All providers remain accessible via `iter()`, `get()`

If the configured backend/implementation isn't available (failed to probe), the default probe-order priority applies and a warning is logged.

`ProviderSet` gains two methods that use the stored `ProviderDescriptor` to match:

```rust
impl<T: ?Sized> ProviderSet<T> {
    /// Reorder so that the first entry whose descriptor.backend matches is first.
    /// No-op if no entry matches.
    pub fn prefer_by_backend(&mut self, backend: &str) { ... }

    /// Reorder so that the entry with the given implementation key is first.
    /// No-op if no entry matches.
    pub fn prefer_by_implementation(&mut self, implementation: &str) { ... }
}
```

Discovery calls these after all probes complete:

```rust
// Apply backend preference
if let Some(backend) = config.ai_utility.preference.backend.as_deref() {
    registry.ai_utilities.prefer_by_backend(backend);
}

// Apply implementation override
if let Some(impl_name) = config.ai_utility.claude.as_ref().and_then(|c| c.implementation.as_deref()) {
    registry.ai_utilities.prefer_by_implementation(impl_name);
}
```

### 8. Discovery Changes

- All categories use `probe_all` — no more `probe_first`. Every successful factory gets registered.
- This is a deliberate behavioral change: categories that previously used `probe_first` (checkout managers, workspace managers, terminal pools) will now register multiple providers. For example, both `wt` and `git` checkout managers will register when both are available, and multiple workspace managers (cmux, zellij, tmux) may all register. Only the preferred one (first after reordering) is used by `preferred()`, but all remain accessible via `iter()` and `get()`.
- Refresh code for checkout managers, workspace managers, and terminal pools must use `preferred()` rather than `iter()` to avoid duplicate data (e.g. both `wt` and `git worktrees` listing the same underlying worktrees). Categories where multiple providers produce distinct data (cloud agents, code review, issue trackers) continue to use `iter()`.
- Factory list order in `FactoryRegistry` still determines default priority (first registered = default preferred).
- After probing, config preferences reorder each `ProviderSet`.
- The checkout manager factories stop reading config themselves (remove the `provider != "auto" && provider != "wt"` guards from `WtCheckoutManagerFactory::probe()` and `GitCheckoutManagerFactory::probe()`). Instead, discovery reads `vcs.git.checkout_strategy` and calls `registry.checkout_managers.prefer_by_implementation(strategy)` after probing. When `checkout_strategy` is `"auto"` or absent, no `prefer_by_implementation` call is made and default probe-order priority applies. Tests asserting `checkout_managers.len() == 1` update to reflect multiple registrations.

### 9. Scope Boundaries

**In scope:**
- `ProviderCategory` enum replacing string literals
- `ProviderDescriptor` backend/implementation split
- `CodeReview` → `ChangeRequestTracker` rename (trait, module, registry, factory, protocol)
- Registry field pluralization (`workspace_managers`, `terminal_pools`)
- Config structure for all categories
- `ProviderSet::prefer_by_backend()` / `prefer_by_implementation()` ordering
- Checkout config migration (flatten `vcs.git.checkouts` to `vcs.git`)
- Discovery uses `probe_all` uniformly

**Out of scope (future):**
- Per-operation provider routing (e.g. use different AI for different tasks)
- Full provider ordering policies (cost-based, quota-based)
- Shared implementation-style config (e.g. common CLI config across claude-cli and codex-cli)
- Backend-specific config beyond provider selection (API keys, model preferences, etc. in config file)
