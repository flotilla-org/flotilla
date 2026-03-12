# Modular Provider Discovery

Refactor the monolithic `detect_providers` function into a two-phase pipeline: environment detection followed by typed provider construction. Separates the concerns of probing the environment, describing provider identity, and constructing provider instances.

Addresses issue #171.

## Pipeline

```
Host Detectors ──→ HostBag ─┐
                             ├─→ merged EnvironmentBag + ConfigStore ──→ Typed Factories ──→ ProviderRegistry
Repo Detectors ──→ RepoBag ─┘                                                             └→ Vec<UnmetRequirement>
```

**Phase 1 — Detection.** Modular detectors probe the environment and contribute typed assertions to a bag. Host detectors run once at startup (binary availability, env vars, auth files). Repo detectors run per repository (VCS checkout, remote hosts).

**Phase 2 — Construction.** Typed factory traits (one per provider category) inspect the merged bag and config. Each factory either constructs a provider or reports unmet requirements.

## Core Types

### EnvironmentAssertion

Typed enum — what detectors contribute to the bag:

```rust
enum EnvironmentAssertion {
    BinaryAvailable { name: String, path: PathBuf, version: Option<String> },
    EnvVarSet { key: String, value: String },
    VcsCheckoutDetected { root: PathBuf, kind: VcsKind, is_main_checkout: bool },
    RemoteHost { platform: HostPlatform, owner: String, repo: String, remote_name: String },
    AuthFileExists { provider: String, path: PathBuf },
    SocketAvailable { name: String, path: PathBuf },
}

enum VcsKind { Git, Jj }
enum HostPlatform { GitHub, GitLab }
```

### EnvironmentBag

Collected assertions with query helpers:

```rust
struct EnvironmentBag {
    assertions: Vec<EnvironmentAssertion>,
}

impl EnvironmentBag {
    fn find_binary(&self, name: &str) -> Option<&PathBuf>;
    fn find_env_var(&self, key: &str) -> Option<&str>;
    fn find_remote_host(&self, platform: HostPlatform) -> Option<(&str, &str, &str)>;
    fn remote_hosts(&self) -> Vec<&EnvironmentAssertion>; // all RemoteHost assertions
    fn has_auth(&self, provider: &str) -> bool;
    fn find_socket(&self, name: &str) -> Option<&PathBuf>;
    fn find_vcs_checkout(&self, kind: VcsKind) -> Option<(&Path, bool)>;
    fn repo_slug(&self) -> Option<String>; // "{owner}/{repo}" from preferred RemoteHost
    fn merge(&self, other: &EnvironmentBag) -> EnvironmentBag;
}
```

`find_remote_host` returns the preferred remote, applying the existing priority logic (tracking branch remote > origin > first available). `remote_hosts` returns all of them when callers need the full set. `repo_slug` derives `"{owner}/{repo}"` from the preferred remote — replaces the standalone `extract_repo_slug` function.

### UnmetRequirement

What factories report when they cannot construct a provider:

```rust
enum UnmetRequirement {
    MissingBinary(String),
    MissingEnvVar(String),
    MissingAuth(String),
    MissingRemoteHost(HostPlatform),
    NoVcsCheckout,
}
```

Structured for future UI surfacing and remediation hints (a follow-up concern, not part of this refactor).

### ProviderDescriptor

Identity and metadata for a provider kind. Replaces the label methods currently scattered across provider traits:

```rust
struct ProviderDescriptor {
    name: String,           // "github", "cmux", "git"
    display_name: String,   // "GitHub", "cmux"
    abbreviation: String,   // "CR", "CO"
    section_label: String,  // "Change Requests"
    item_noun: String,      // "pull request"
}
```

## Detector Traits

Two traits, split by scope:

```rust
#[async_trait]
trait HostDetector: Send + Sync {
    fn name(&self) -> &str;
    async fn detect(&self, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion>;
}

#[async_trait]
trait RepoDetector: Send + Sync {
    fn name(&self) -> &str;
    async fn detect(&self, repo_root: &Path, runner: &dyn CommandRunner) -> Vec<EnvironmentAssertion>;
}
```

`HostDetector` has no access to `repo_root` — enforced by the type system. Both contribute the same `EnvironmentAssertion` type.

### Detector Implementations

**Host detectors** (run once):
- `GitBinaryDetector` — checks `git` binary, extracts version
- `GhCliDetector` — checks `gh` binary
- `ClaudeDetector` — checks `claude` binary
- `CursorDetector` — checks `agent` binary, `CURSOR_API_KEY` env var
- `CmuxDetector` — checks `cmux` binary (including hardcoded macOS app path fallback), `CMUX_SOCKET_PATH` env var
- `TmuxDetector` — checks `TMUX` env var
- `ZellijDetector` — checks `ZELLIJ` env var, extracts version for compatibility checking
- `ShpoolDetector` — checks `shpool` binary

**Repo detectors** (run per repo):
- `VcsRepoDetector` — checks `.git` (directory or file), contributes `VcsCheckoutDetected`
- `RemoteHostDetector` — parses git remote URLs, contributes `RemoteHost`
- `CodexAuthDetector` — checks codex auth file existence

## Factory Traits

One typed factory trait per provider category. Each returns the concrete trait object — no downcasting:

```rust
#[async_trait]
trait VcsFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn Vcs>, Vec<UnmetRequirement>>;
}

#[async_trait]
trait CheckoutManagerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn CheckoutManager>, Vec<UnmetRequirement>>;
}

#[async_trait]
trait CodeReviewFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn CodeReview>, Vec<UnmetRequirement>>;
}

#[async_trait]
trait IssueTrackerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn IssueTracker>, Vec<UnmetRequirement>>;
}

#[async_trait]
trait CloudAgentFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn CloudAgentService>, Vec<UnmetRequirement>>;
}

#[async_trait]
trait AiUtilityFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn AiUtility>, Vec<UnmetRequirement>>;
}

#[async_trait]
trait WorkspaceManagerFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn WorkspaceManager>, Vec<UnmetRequirement>>;
}

#[async_trait]
trait TerminalPoolFactory: Send + Sync {
    fn descriptor(&self) -> ProviderDescriptor;
    async fn probe(&self, env: &EnvironmentBag, config: &ConfigStore,
                   repo_root: &Path, runner: Arc<dyn CommandRunner>)
        -> Result<Arc<dyn TerminalPool>, Vec<UnmetRequirement>>;
}
```

A single struct (e.g. `GitHubFactory`) can implement both `CodeReviewFactory` and `IssueTrackerFactory`, sharing its descriptor.

## Registration

Explicit functions, gated by cargo features:

```rust
fn default_host_detectors() -> Vec<Box<dyn HostDetector>> {
    vec![
        Box::new(GitBinaryDetector),
        #[cfg(feature = "github")]
        Box::new(GhCliDetector),
        #[cfg(feature = "claude")]
        Box::new(ClaudeDetector),
        // ...
    ]
}

fn default_repo_detectors() -> Vec<Box<dyn RepoDetector>> {
    vec![
        Box::new(VcsRepoDetector),
        Box::new(RemoteHostDetector),
        // ...
    ]
}

fn default_vcs_factories() -> Vec<Box<dyn VcsFactory>> { ... }
fn default_code_review_factories() -> Vec<Box<dyn CodeReviewFactory>> { ... }
// ... one function per category
```

A `FactoryRegistry` struct holds all the factory lists:

```rust
struct FactoryRegistry {
    pub vcs: Vec<Box<dyn VcsFactory>>,
    pub checkout_managers: Vec<Box<dyn CheckoutManagerFactory>>,
    pub code_review: Vec<Box<dyn CodeReviewFactory>>,
    pub issue_trackers: Vec<Box<dyn IssueTrackerFactory>>,
    pub cloud_agents: Vec<Box<dyn CloudAgentFactory>>,
    pub ai_utilities: Vec<Box<dyn AiUtilityFactory>>,
    pub workspace_managers: Vec<Box<dyn WorkspaceManagerFactory>>,
    pub terminal_pools: Vec<Box<dyn TerminalPoolFactory>>,
}
```

## Orchestrator

Replaces the current `detect_providers` function:

```rust
pub struct DiscoveryResult {
    pub registry: ProviderRegistry,
    pub bag: EnvironmentBag,
    pub repo_slug: Option<String>,
    pub unmet: Vec<UnmetRequirement>,
}

pub async fn discover_providers(
    host_bag: &EnvironmentBag,
    repo_root: &Path,
    repo_detectors: &[Box<dyn RepoDetector>],
    factories: &FactoryRegistry,
    config: &ConfigStore,
    runner: Arc<dyn CommandRunner>,
) -> DiscoveryResult
```

The caller runs host detectors once, then calls `discover_providers` per repo. The orchestrator merges host and repo bags, runs all factories, and derives `repo_slug` from the bag's preferred `RemoteHost` assertion.

**Repo identity:** callers that need `RepoIdentity` for multi-host peer matching construct it from the bag's `RemoteHost` assertions. The existing `first_remote_url` and `extract_repo_identity` helpers remain available but are no longer called during discovery — the `RemoteHostDetector` captures the same information. Callers can query the merged bag (exposed on `DiscoveryResult`) or continue using the standalone helpers if they prefer.

**Follower mode:** handled by passing a filtered `FactoryRegistry` that omits code review, issue tracker, cloud agent, and AI utility factories.

**Checkout manager selection:** the orchestrator iterates checkout manager factories in registration order. Each factory checks `config.resolve_checkouts_config(repo_root)` — if the config forces a specific provider (e.g. `"wt"` or `"git"`), a factory whose name doesn't match returns `Err`. In `"auto"` mode, factories probe the environment normally and the first to succeed wins. The orchestrator stops after the first successful checkout manager factory (at-most-one semantics), matching the current behavior where only one checkout manager is registered.

**Workspace manager and terminal pool** follow the same at-most-one pattern: factories are tried in priority order (e.g. `CMUX_SOCKET_PATH` > `ZELLIJ` > `TMUX` > cmux binary fallback for workspace; shpool > passthrough for terminal). The orchestrator stops at the first success. `PassthroughTerminalPoolFactory` always succeeds, serving as the unconditional fallback.

## ProviderRegistry Changes

Stores descriptors alongside providers:

```rust
pub struct ProviderRegistry {
    pub vcs: IndexMap<String, (ProviderDescriptor, Arc<dyn Vcs>)>,
    pub checkout_managers: IndexMap<String, (ProviderDescriptor, Arc<dyn CheckoutManager>)>,
    pub code_review: IndexMap<String, (ProviderDescriptor, Arc<dyn CodeReview>)>,
    pub issue_trackers: IndexMap<String, (ProviderDescriptor, Arc<dyn IssueTracker>)>,
    pub cloud_agents: IndexMap<String, (ProviderDescriptor, Arc<dyn CloudAgentService>)>,
    pub ai_utilities: IndexMap<String, (ProviderDescriptor, Arc<dyn AiUtility>)>,
    pub workspace_manager: Option<(ProviderDescriptor, Arc<dyn WorkspaceManager>)>,
    pub terminal_pool: Option<(ProviderDescriptor, Arc<dyn TerminalPool>)>,
}
```

`labels_from_registry` reads descriptors directly instead of calling provider trait methods. All label and display methods are removed from provider traits — this includes `display_name()` on `Vcs`, `AiUtility`, `WorkspaceManager`, and `TerminalPool`, as well as `section_label()`, `item_noun()`, and `abbreviation()` on `CheckoutManager`, `CodeReview`, `IssueTracker`, and `CloudAgentService`. `provider_names_from_registry` reads `ProviderDescriptor::display_name` instead of calling trait methods.

## File Structure

```
providers/
  discovery.rs            → orchestrator function (replaces monolithic detect_providers)
  registry.rs             → ProviderRegistry (updated with descriptors)
  discovery/
    mod.rs                → types (assertions, bag, requirements, descriptor),
                            detector/factory traits, FactoryRegistry, registration fns
    detectors/
      mod.rs
      git.rs              → GitBinaryDetector, VcsRepoDetector, RemoteHostDetector
      github.rs           → GhCliDetector
      claude.rs           → ClaudeDetector
      cursor.rs           → CursorDetector
      cmux.rs             → CmuxDetector
      tmux.rs             → TmuxDetector
      env.rs              → ZellijDetector, general env var detection
      shpool.rs           → ShpoolDetector
      codex.rs            → CodexAuthDetector
    factories/
      mod.rs
      git.rs              → GitVcsFactory, WtCheckoutManagerFactory, GitCheckoutManagerFactory
      github.rs           → GitHubCodeReviewFactory, GitHubIssueTrackerFactory
      claude.rs           → ClaudeCodingAgentFactory, ClaudeAiUtilityFactory
      cursor.rs           → CursorCodingAgentFactory
      codex.rs            → CodexCodingAgentFactory
      cmux.rs             → CmuxWorkspaceManagerFactory
      tmux.rs             → TmuxWorkspaceManagerFactory
      zellij.rs           → ZellijWorkspaceManagerFactory
      shpool.rs           → ShpoolTerminalPoolFactory
      passthrough.rs      → PassthroughTerminalPoolFactory
```

Provider implementations (`vcs/git.rs`, `coding_agent/claude.rs`, etc.) stay where they are. This refactor touches discovery and construction only.

## Testing

Each layer tests independently:

**Detectors** — given a mock `CommandRunner`, verify correct assertions. e.g. `GitBinaryDetector` with `git` on PATH produces `BinaryAvailable { "git", ... }`.

**EnvironmentBag** — unit tests for query helpers. Pure data, no mocking.

**Factories** — given a hand-built `EnvironmentBag` + `ConfigStore`, verify correct provider construction or correct `UnmetRequirement`s. No filesystem access — factories inspect the bag only. This replaces the bulk of current `discovery.rs` tests.

**Orchestrator** — small number of integration tests wiring detectors and factories together.

## Out of Scope

- **Auth as a discovery concern** — auth validation stays inside provider implementations. Future work may promote it to a first-class discovery concept with up-front checks and remediation.
- **Remediation hints on UnmetRequirement** — the enum is structured for future extension (install instructions, links), but this refactor only reports what's missing.
- **Runtime plugin loading** — compile-time modularity only. The architecture could support plugins later without a rewrite.
- **Cargo feature gating** — the registration functions show where `#[cfg]` attributes go, but defining the actual features is a separate task.
