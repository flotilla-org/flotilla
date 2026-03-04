# Provider Abstractions Design

Date: 2026-03-04

## Problem

cmux-controller integrates with several external tools via direct subprocess calls:
`wt`, `git`, `gh`, `cmux`, `claude`, and the Anthropic API. Each integration is
hardcoded. To support alternative backends (jj, GitLab, Linear, zellij, tmux,
Codex, libgit2, etc.), we need trait-based abstractions with pluggable
implementations.

## Design Principles

- **Trait objects** (`dyn Trait`) throughout — multiple providers of the same type
  can coexist (e.g. GitHub Issues + Linear).
- **Discovery over configuration** — detect VCS, remote host, tools, and workspace
  manager from the environment. Config only needed for overrides.
- **Provider name vs implementation type** — the registry key (e.g. `"git"`)
  identifies *what* is provided; the `type` field (e.g. `"git-cli"` vs
  `"git-libgit2"`) selects *how*. New implementations ship behind non-default
  types for gradual rollout.
- **Correlation by typed keys** — providers emit `CorrelationKey` values with
  their data. The correlation engine groups related items by shared keys,
  replacing hardcoded branch-name matching.
- **Auth is per-service** — no standalone auth trait for now. Implementations
  manage their own credentials (subprocess tools handle auth, keychain hack for
  Anthropic API). Extract shared auth infra when a real need arises.

## Trait Taxonomy

Seven traits across four domains:

### Source Control

- **`Vcs`** — repo queries: list branches (local + remote), commit log,
  ahead/behind, working tree status.
- **`CheckoutManager`** — parallel checkout lifecycle: list, create, remove.
  Agnostic to mechanism (git worktrees, jj workspaces, clones). Subordinate to
  `Vcs` — a git factory produces a git-flavored checkout manager.

### Remote Platforms

- **`CodeReview`** — list/view change requests (PRs/MRs), status, open in
  browser. Generic `ChangeRequest` type normalizes GitHub PRs, GitLab MRs, etc.
- **`IssueTracker`** — list/view issues, labels, open in browser. Uses string
  IDs to handle both `"42"` (GitHub) and `"PROJ-43"` (Linear). Independent of
  `CodeReview` — can mix GitHub PRs with Linear issues.

### AI Services

- **`CodingAgent`** — cloud session lifecycle: list sessions, archive,
  attach/teleport. Returns `CloudAgentSession`.
- **`AiUtility`** — quick one-shot tasks: generate branch name from issue
  context.

### Workspace

- **`WorkspaceManager`** — thin trait: list workspaces, create workspace (from
  opaque config), select/switch. Template rendering and pane orchestration stay
  in implementations. Detected from environment variables (`$CMUX_SESSION`,
  `$ZELLIJ`, `$TMUX`), not PATH.

## Trait Definitions

```rust
#[async_trait]
trait Vcs: Send + Sync {
    fn display_name(&self) -> &str;

    async fn list_local_branches(&self, repo_root: &Path) -> Result<Vec<BranchInfo>>;
    async fn list_remote_branches(&self, repo_root: &Path) -> Result<Vec<String>>;
    async fn commit_log(
        &self, repo_root: &Path, branch: &str, limit: usize,
    ) -> Result<Vec<CommitInfo>>;
    async fn ahead_behind(
        &self, repo_root: &Path, branch: &str, reference: &str,
    ) -> Result<AheadBehind>;
    async fn working_tree_status(
        &self, repo_root: &Path, checkout_path: &Path,
    ) -> Result<WorkingTreeStatus>;
}

#[async_trait]
trait CheckoutManager: Send + Sync {
    fn display_name(&self) -> &str;

    async fn list_checkouts(&self, repo_root: &Path) -> Result<Vec<Checkout>>;
    async fn create_checkout(&self, repo_root: &Path, branch: &str) -> Result<Checkout>;
    async fn remove_checkout(&self, repo_root: &Path, branch: &str) -> Result<()>;
}

#[async_trait]
trait CodeReview: Send + Sync {
    fn display_name(&self) -> &str;

    async fn list_change_requests(
        &self, repo_root: &Path, limit: usize,
    ) -> Result<Vec<ChangeRequest>>;
    async fn get_change_request(&self, repo_root: &Path, id: i64) -> Result<ChangeRequest>;
    async fn open_in_browser(&self, repo_root: &Path, id: i64) -> Result<()>;
}

#[async_trait]
trait IssueTracker: Send + Sync {
    fn display_name(&self) -> &str;

    async fn list_issues(&self, repo_root: &Path, limit: usize) -> Result<Vec<Issue>>;
    async fn open_in_browser(&self, repo_root: &Path, id: &str) -> Result<()>;
}

#[async_trait]
trait CodingAgent: Send + Sync {
    fn display_name(&self) -> &str;

    async fn list_sessions(&self) -> Result<Vec<CloudAgentSession>>;
    async fn archive_session(&self, session_id: &str) -> Result<()>;
    /// Returns the CLI command to attach/teleport into a session.
    async fn attach_command(&self, session_id: &str) -> Result<String>;
}

#[async_trait]
trait AiUtility: Send + Sync {
    fn display_name(&self) -> &str;

    async fn generate_branch_name(&self, context: &str) -> Result<String>;
}

#[async_trait]
trait WorkspaceManager: Send + Sync {
    fn display_name(&self) -> &str;

    async fn list_workspaces(&self) -> Result<Vec<Workspace>>;
    async fn create_workspace(&self, config: &WorkspaceConfig) -> Result<Workspace>;
    async fn select_workspace(&self, ws_ref: &str) -> Result<()>;
}
```

## Shared Data Types

Provider-agnostic types returned by traits. Current types like `Worktree`,
`GithubPr` become implementation details.

```rust
struct BranchInfo {
    name: String,
    is_trunk: bool,
}

struct Checkout {
    branch: String,
    path: PathBuf,
    is_trunk: bool,
    correlation_keys: Vec<CorrelationKey>,
}

struct WorkingTreeStatus {
    staged: usize,
    modified: usize,
    untracked: usize,
}

struct ChangeRequest {
    id: i64,
    title: String,
    branch: String,
    status: ChangeRequestStatus,
    body: Option<String>,
    correlation_keys: Vec<CorrelationKey>,
}

enum ChangeRequestStatus {
    Open,
    Draft,
    Merged,
    Closed,
}

struct Issue {
    id: String,       // String to handle "42" and "PROJ-43"
    title: String,
    labels: Vec<Label>,
    correlation_keys: Vec<CorrelationKey>,
}

struct CloudAgentSession {
    id: String,
    title: String,
    status: SessionStatus,
    model: Option<String>,
    correlation_keys: Vec<CorrelationKey>,
}

enum SessionStatus {
    Running,
    Idle,
    Archived,
}

struct Workspace {
    ws_ref: String,              // opaque handle for select_workspace()
    name: String,
    directories: Vec<PathBuf>,
    correlation_keys: Vec<CorrelationKey>,
}

struct WorkspaceConfig {
    name: String,
    working_directory: PathBuf,
    template_vars: HashMap<String, String>,
    template: Option<WorkspaceTemplate>,   // implementation-specific
}
```

## Correlation System

Every returned data type carries `correlation_keys: Vec<CorrelationKey>`.
Providers emit the keys they know about. The correlation engine groups items
with shared keys into `WorkItem`s, replacing hardcoded branch-name matching.

```rust
enum CorrelationKey {
    Branch(String),                       // cross-cutting, no source qualifier
    RepoPath(PathBuf),                    // cross-cutting
    IssueRef(String, String),             // (provider_name, issue_id)
    ChangeRequestRef(String, i64),        // (provider_name, CR number)
    SessionRef(String, String),           // (provider_name, session_id)
}
```

Correlation is transitive: if checkout has `Branch("feat-x")` and a PR has
`Branch("feat-x")` + `IssueRef("linear", "PROJ-43")`, an issue with
`IssueRef("linear", "PROJ-43")` joins the same group even though it shares no
key with the checkout directly.

The "Fixes #N" parsing moves into the GitHub `CodeReview` implementation —
it parses PR bodies and emits `IssueRef` correlation keys. Other implementations
emit whatever linking they know about.

## Provider Registry

Named providers in ordered maps:

```rust
struct ProviderRegistry {
    vcs: IndexMap<String, Box<dyn Vcs>>,
    checkout_managers: IndexMap<String, Box<dyn CheckoutManager>>,
    code_review: IndexMap<String, Box<dyn CodeReview>>,
    issue_trackers: IndexMap<String, Box<dyn IssueTracker>>,
    coding_agents: IndexMap<String, Box<dyn CodingAgent>>,
    ai_utilities: IndexMap<String, Box<dyn AiUtility>>,
    workspace_manager: Option<(String, Box<dyn WorkspaceManager>)>,
}
```

Provider names serve triple duty:
1. Config keys: `[issue_tracker.linear]`
2. Correlation source: `IssueRef("linear", "PROJ-43")`
3. UI provenance: displayed alongside items

## Discovery & Factory System

Providers are detected from the environment, not configured from scratch.
Config overrides detection results.

```
Detection pipeline (per repo):

1. VCS detection (first match wins)
   - .jj/ present? → JjFactory
   - .git/ present? → GitFactory
   Factory internally:
     - runs `git --version`, checks minimum version
     - tries `wt --version` → WtCheckoutManager
     - fallback → GitWorktreeCheckoutManager
     - option → GitCloneCheckoutManager

2. Remote host detection
   - parse `git remote get-url origin`
   - github.com → GitHubFactory (runs `gh --version`)
     - produces CodeReview + IssueTracker
   - gitlab.com → GitLabFactory (future)
   Config can ADD more (Linear) or disable defaults

3. Coding agent detection
   - `which claude` → ClaudeCodingAgent

4. AI utility detection
   - `which claude` → ClaudeAiUtility

5. Workspace manager detection (env vars, not PATH)
   - $CMUX_SESSION → CmuxWorkspaceManager
   - $ZELLIJ → ZellijWorkspaceManager (future)
   - $TMUX → TmuxWorkspaceManager (future)
```

Factories validate tools (version checks, connectivity) and degrade gracefully:
`wt` too old → warn, fall back to plain worktrees. `gh` missing → skip GitHub
integration. No workspace manager → app works, just can't create/switch
workspaces.

## Configuration

Three-tier merge, highest priority wins:

```
~/.config/cmux-controller/repos/<slug>.toml   (personal per-repo override)
    overrides
<repo-root>/.cmux-controller.toml              (repo-homed, committable)
    overrides
~/.config/cmux-controller/config.toml          (global defaults)
```

Config adds to or overrides detection results. Most users need zero config.

```toml
# Override implementation type (gradual backend rollout)
[vcs.git]
type = "git-libgit2"    # default would be "git-cli"

# Add a provider not detected automatically
[issue_tracker.linear]
type = "linear"
project = "PROJ"

# Disable an auto-detected provider
[issue_tracker.github]
enabled = false

# Override checkout strategy
[checkout_manager.git]
type = "git-clone"      # instead of auto-detected "wt-cli"
```

Provider name (the TOML key) identifies *what* is provided.
Implementation type identifies *how*. New implementations ship behind
non-default types — zero risk to existing users.

## Module Structure

```
src/
  providers/
    mod.rs              // trait definitions, shared data types, CorrelationKey
    registry.rs         // ProviderRegistry, IndexMap-based storage
    correlation.rs      // correlation engine (union-find grouping)
    discovery.rs        // detection pipeline, factory orchestration

    vcs/
      mod.rs            // Vcs + CheckoutManager traits, VcsBundle
      git.rs            // GitVcs (git-cli), GitWorktreeCheckoutManager
      wt.rs             // WtCheckoutManager
      // jj.rs          // future

    code_review/
      mod.rs            // CodeReview trait, ChangeRequest types
      github.rs         // GitHubCodeReview (gh CLI)
      // gitlab.rs      // future

    issue_tracker/
      mod.rs            // IssueTracker trait, Issue types
      github.rs         // GitHubIssueTracker (gh CLI)
      // linear.rs      // future

    coding_agent/
      mod.rs            // CodingAgent trait, CloudAgentSession
      claude.rs         // ClaudeCodingAgent (API + keychain)
      // codex.rs       // future

    ai_utility/
      mod.rs            // AiUtility trait
      claude.rs         // ClaudeAiUtility (claude -p)

    workspace/
      mod.rs            // WorkspaceManager trait, WorkspaceConfig
      cmux.rs           // CmuxWorkspaceManager + template rendering
      // zellij.rs      // future
      // tmux.rs        // future

  config.rs             // three-tier config loading + merge
  app.rs                // App holds ProviderRegistry
  data.rs               // DataStore fetches via registry, correlates via engine
```

## Migration Path

Current `fetch_*()` functions in `data.rs` become method bodies of the concrete
implementations (GitHubCodeReview, WtCheckoutManager, etc.). `DataStore::refresh()`
calls through the registry. `correlate()` is replaced by the correlation engine.
`actions.rs` dispatches through the registry instead of calling tools directly.

## Future Considerations

- **Auth extraction** — when a second auth mechanism appears, extract shared
  infra. Auth is naturally per-service with shared infrastructure.
- **Web/browser abstraction** — if we want to render PR descriptions or issue
  details in-app rather than shelling out to a browser.
- **Template system** — stays implementation-specific. Each workspace manager
  has its own layout config. `WorkspaceConfig::template` is `Option` for this
  reason.
