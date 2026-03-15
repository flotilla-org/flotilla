# DaemonHandle cleanup: trait slimming, command renames, and repo addressing

## Problem

The `DaemonHandle` trait has accumulated inconsistencies:

1. **Duplicated methods**: `add_repo`, `remove_repo`, and `refresh` exist as both dedicated trait methods and `CommandAction` variants. The trait methods bypass the command envelope (`Command { host, context_repo, action }`), preventing remote routing.

2. **Misleading names**: `AddRepo` suggests adding a repository, but the operation takes a checkout path, discovers which repo it belongs to (via `RepoIdentity`), and starts tracking it. `RemoveRepo` removes a tracked path — it doesn't delete anything from disk.

3. **Inconsistent repo addressing**: Query methods use a mix of `&Path` (`get_state`) and `&str` slug (`get_repo_detail`, `get_repo_providers`, `get_repo_work`). Four `CommandAction` variants (`SetIssueViewport`, `FetchMoreIssues`, `SearchIssues`, `ClearIssueSearch`) use raw `PathBuf` instead of `RepoSelector`.

4. **Signature mismatch between trait and command**: The trait method `remove_repo(&self, path: &Path)` only accepts a path, while the corresponding `CommandAction::RemoveRepo` takes a `RepoSelector` (supporting path, query, or identity). The trait method is strictly less capable.

## Design

### 1. Slim the DaemonHandle trait

Remove three methods that duplicate `CommandAction` variants:

```rust
// Remove:
async fn add_repo(&self, path: &Path) -> Result<(), String>;
async fn remove_repo(&self, path: &Path) -> Result<(), String>;
async fn refresh(&self, repo: &Path) -> Result<(), String>;
```

Callers switch to `execute(Command { action: CommandAction::TrackRepoPath { .. }, .. })` etc. The implementation logic stays in `InProcessDaemon` but moves behind the executor's command dispatch. `SocketDaemon` gets simpler since it already forwards commands over the wire.

The remaining trait surface:

- `subscribe` — event stream
- `execute` / `cancel` — command/event path
- `replay_since` — gap recovery
- `get_state`, `list_repos`, `get_status`, `get_repo_detail`, `get_repo_providers`, `get_repo_work` — repo queries
- `list_hosts`, `get_host_status`, `get_host_providers`, `get_topology` — host queries

### 2. Rename CommandAction variants

| Current | New | Rationale |
|---------|-----|-----------|
| `AddRepo { path: PathBuf }` | `TrackRepoPath { path: PathBuf }` | The caller provides a filesystem path; the daemon discovers the repo identity |
| `RemoveRepo { repo: RepoSelector }` | `UntrackRepo { repo: RepoSelector }` | Stops tracking — no disk cleanup. Distinct from `RemoveCheckout` which deletes worktrees |

### 3. Rename CommandResult variants

| Current | New |
|---------|-----|
| `RepoAdded { path: PathBuf }` | `RepoTracked { path: PathBuf, resolved_from: Option<PathBuf> }` |
| `RepoRemoved { path: PathBuf }` | `RepoUntracked { path: PathBuf }` |

`resolved_from` is `Some(original_path)` when the user provided a worktree path that was normalized to the repo root. This lets callers inform the user that the path was resolved.

### 4. Rename DaemonEvent variants

| Current | New |
|---------|-----|
| `RepoAdded(Box<RepoInfo>)` | `RepoTracked(Box<RepoInfo>)` |
| `RepoRemoved { repo_identity, path }` | `RepoUntracked { repo_identity, path }` |

### 5. Fix raw PathBuf in CommandAction

Four variants use `repo: PathBuf` where `repo: RepoSelector` is appropriate:

```rust
// Before:
SetIssueViewport { repo: PathBuf, visible_count: usize }
FetchMoreIssues { repo: PathBuf, desired_count: usize }
SearchIssues { repo: PathBuf, query: String }
ClearIssueSearch { repo: PathBuf }

// After:
SetIssueViewport { repo: RepoSelector, visible_count: usize }
FetchMoreIssues { repo: RepoSelector, desired_count: usize }
SearchIssues { repo: RepoSelector, query: String }
ClearIssueSearch { repo: RepoSelector }
```

### 6. Unify query method signatures

Query methods on `DaemonHandle` change from mixed `&Path`/`&str` to `&RepoSelector`:

```rust
// Before:
async fn get_state(&self, repo: &Path) -> Result<Snapshot, String>;
async fn get_repo_detail(&self, slug: &str) -> Result<RepoDetailResponse, String>;
async fn get_repo_providers(&self, slug: &str) -> Result<RepoProvidersResponse, String>;
async fn get_repo_work(&self, slug: &str) -> Result<RepoWorkResponse, String>;

// After:
async fn get_state(&self, repo: &RepoSelector) -> Result<Snapshot, String>;
async fn get_repo_detail(&self, repo: &RepoSelector) -> Result<RepoDetailResponse, String>;
async fn get_repo_providers(&self, repo: &RepoSelector) -> Result<RepoProvidersResponse, String>;
async fn get_repo_work(&self, repo: &RepoSelector) -> Result<RepoWorkResponse, String>;
```

### 7. Path normalization in TrackRepoPath

When the user provides a worktree path, resolve it to the canonical repo root (the directory containing `.git`). This can be done with `git rev-parse --git-common-dir` or by walking up from the worktree's `.git` file.

If normalization changes the path, set `resolved_from: Some(original_path)` in the `RepoTracked` result so the UI can inform the user (e.g. "Tracking `/repo` (resolved from `/repo/wt-feat`)").

## What does NOT change

- **`RepoIdentity` stays as the primary key** in `repos: HashMap<RepoIdentity, RepoState>`. Multiple clones of the same remote are correctly merged under one identity with multiple roots.
- **`RepoState` / `RepoRootState` structure** stays as-is. Each tracked path gets its own provider stack and discovery bag.
- **`RemoveCheckout`** (the destructive worktree deletion) is unchanged.
- **Config persistence** model is unchanged — one `.toml` per tracked path, `tab-order.json` for ordering.

## Key migration points

**TUI event loop** (`crates/flotilla-tui/src/run.rs:101`): The 'r' key handler calls `daemon.refresh(&repo)` directly — the primary live code path to migrate.

**Server RPC dispatch** (`crates/flotilla-daemon/src/server.rs:1836-1867`): The `"refresh"`, `"add_repo"`, and `"remove_repo"` RPC method handlers become dead code once `SocketDaemon` stops sending them. These should be removed, with `SocketDaemon` routing through `"execute"` instead.

**Server peer-overlay cleanup** (`crates/flotilla-daemon/src/server.rs:927,1243`): The peer manager calls `daemon.remove_repo()` directly when virtual repos are cleaned up on peer disconnect. These internal callers should continue calling the implementation method directly (not through the command envelope) since peer cleanup is an internal server concern, not a client-initiated command.

**SocketDaemon RPC methods** (`crates/flotilla-client/src/lib.rs:577-589`): The `"refresh"`, `"add_repo"`, and `"remove_repo"` RPC calls are replaced by `"execute"` with the appropriate `Command`.

**StubDaemon** (`crates/flotilla-tui/src/app/test_support.rs`): Implements all three removed trait methods — must be updated.

## Affected crates

| Crate | Changes |
|-------|---------|
| `flotilla-protocol` | Rename `CommandAction`/`CommandResult`/`DaemonEvent` variants; change `PathBuf` → `RepoSelector` in four commands; update serde renames |
| `flotilla-core` | Remove three `DaemonHandle` methods; update `InProcessDaemon` to route through executor; update query signatures; add path normalization. Tests: `in_process_daemon.rs` has 6+ calls to migrate |
| `flotilla-client` | Update `SocketDaemon`: remove dedicated `"refresh"`/`"add_repo"`/`"remove_repo"` RPC methods, route through `"execute"` instead |
| `flotilla-tui` | Migrate `run.rs` refresh caller and `StubDaemon` in test support; update query call sites |
| `flotilla-daemon` | Remove dead `"refresh"`/`"add_repo"`/`"remove_repo"` RPC dispatch arms; keep peer-overlay cleanup calling implementation directly. Tests: `socket_roundtrip.rs`, `peer_connect_flow.rs`, `multi_host.rs` have multiple calls to migrate |
| `flotilla` (root) | Update CLI command construction for renames |
