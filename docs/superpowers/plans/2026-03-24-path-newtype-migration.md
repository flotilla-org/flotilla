# Path Newtype Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce `DaemonHostPath` and `ExecutionEnvironmentPath` newtypes to enforce the config-context / execution-context separation at compile time.

**Architecture:** Define two newtype wrappers over `PathBuf` in a new `path_context` module in `flotilla-core`. Migrate every `PathBuf`/`&Path` in the provider, discovery, config, store, executor, and step layers to use the appropriate newtype. The compiler flags every conflation point — this PR IS the audit.

**Tech Stack:** Rust newtypes, `std::path::{Path, PathBuf}`, `AsRef` impls (no `Deref` — force explicit `.as_path()` to surface crossing points), serde `transparent` for serialized paths.

**Notes:**
- `Deref<Target = Path>` is intentionally omitted. Forcing `.as_path()` makes boundary crossings visible in code review.
- Test files need mechanical updates (wrapping `PathBuf::from(...)` in `ExecutionEnvironmentPath::new(...)`) — expect ~30 test sites across stores, executor, hop chain, and provider tests.
- Protocol crate paths stay as `PathBuf`/`String` (wire format). `convert.rs` wraps/unwraps at the core↔protocol boundary.

**Spec:** `docs/superpowers/specs/2026-03-24-provider-audit-execution-context-design.md`

---

### Task 1: Define the newtype module

**Files:**
- Create: `crates/flotilla-core/src/path_context.rs`
- Modify: `crates/flotilla-core/src/lib.rs` (add `pub mod path_context;`)

- [ ] **Step 1: Create the newtype definitions**

```rust
// crates/flotilla-core/src/path_context.rs

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A path on the daemon host's filesystem.
/// Config, state, sockets, store data.
/// Never valid inside an execution environment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DaemonHostPath(PathBuf);

/// A path inside an execution environment.
/// Repo roots, binary locations, working directories, checkout paths.
/// Resolved via CommandRunner + EnvVars, not from daemon config.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionEnvironmentPath(PathBuf);

// --- DaemonHostPath impls ---

impl DaemonHostPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn join(&self, suffix: impl AsRef<Path>) -> Self {
        Self(self.0.join(suffix))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for DaemonHostPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for DaemonHostPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl From<PathBuf> for DaemonHostPath {
    fn from(p: PathBuf) -> Self {
        Self(p)
    }
}

// --- ExecutionEnvironmentPath impls ---

impl ExecutionEnvironmentPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn join(&self, suffix: impl AsRef<Path>) -> Self {
        Self(self.0.join(suffix))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl AsRef<Path> for ExecutionEnvironmentPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl fmt::Display for ExecutionEnvironmentPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.display().fmt(f)
    }
}

impl From<PathBuf> for ExecutionEnvironmentPath {
    fn from(p: PathBuf) -> Self {
        Self(p)
    }
}
```

- [ ] **Step 2: Register the module**

Add to `crates/flotilla-core/src/lib.rs`:
```rust
pub mod path_context;
```

- [ ] **Step 3: Compile to verify**

Run: `cargo build -p flotilla-core`
Expected: PASS (no consumers yet)

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/path_context.rs crates/flotilla-core/src/lib.rs
git commit -m "feat: introduce DaemonHostPath and ExecutionEnvironmentPath newtypes"
```

---

### Task 2: Migrate EnvironmentBag assertions

The assertion enum in `discovery/mod.rs` carries paths in its variants. Classify each:
- `BinaryAvailable { path }` → `ExecutionEnvironmentPath` (binary in execution env)
- `VcsCheckoutDetected { root }` → `ExecutionEnvironmentPath` (repo in execution env)
- `AuthFileExists { path }` → `ExecutionEnvironmentPath` (auth file in user's home, varies per env)
- `SocketAvailable { path }` → `DaemonHostPath` (daemon-managed socket)

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs` (assertion enum + accessors)
- Modify: all detectors that create assertions (`detectors/generic.rs`, `detectors/git.rs`, `detectors/claude.rs`, `detectors/codex.rs`, `detectors/cmux.rs`)
- Modify: all factories that read assertions via `env.find_binary()`, `env.find_socket()`, `env.find_vcs_checkout()`

- [ ] **Step 1: Change `EnvironmentAssertion` variants**

In `crates/flotilla-core/src/providers/discovery/mod.rs`, change the path types in the enum variants and their constructor helpers. Update `find_binary()` to return `Option<&ExecutionEnvironmentPath>`, `find_socket()` to return `Option<&DaemonHostPath>`, `find_vcs_checkout()` to return `Option<(&ExecutionEnvironmentPath, bool)>`.

- [ ] **Step 2: Compile and fix cascading errors**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

Fix each error by wrapping the appropriate newtype at the call site. Detectors create assertions — they wrap paths when constructing `EnvironmentAssertion` values. Factories read from the bag — they receive the typed paths.

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add -u crates/flotilla-core/src/providers/discovery/
git commit -m "refactor: type EnvironmentBag assertion paths with newtypes"
```

---

### Task 3: Migrate discovery pipeline signatures

`repo_root: &Path` → `&ExecutionEnvironmentPath` in the `Factory` trait, `RepoDetector` trait, `discover_providers()` entry point, and all implementations.

**Files:**
- Modify: `crates/flotilla-core/src/providers/discovery/mod.rs` (trait definitions + `discover_providers()` + `probe_all()`)
- Modify: `crates/flotilla-core/src/providers/discovery/factories/*.rs` (all 11 factories)
- Modify: `crates/flotilla-core/src/providers/discovery/detectors/git.rs` (`VcsRepoDetector`, `RemoteHostDetector`)
- Modify: `crates/flotilla-core/src/providers/discovery/test_support.rs` (test factories + test harness)

- [ ] **Step 1: Change the trait signatures**

In `crates/flotilla-core/src/providers/discovery/mod.rs`, change both `Factory::probe()` and `RepoDetector::detect()`:
```rust
// Factory trait
async fn probe(
    &self,
    env: &EnvironmentBag,
    config: &ConfigStore,
    repo_root: &ExecutionEnvironmentPath,  // was &Path
    runner: Arc<dyn CommandRunner>,
) -> Result<Arc<Self::Output>, Vec<UnmetRequirement>>;

// RepoDetector trait
async fn detect(
    &self,
    repo_root: &ExecutionEnvironmentPath,  // was &Path
    runner: &dyn CommandRunner,
    env: &dyn EnvVars,
) -> Vec<EnvironmentAssertion>;
```

Also change `discover_providers()` and the internal `probe_all()` helper — these are the entry points where `repo_root` enters the discovery system.

- [ ] **Step 2: Compile and fix all factory and detector implementations**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

Each factory's `probe()` and each detector's `detect()` signature must match. Most factories pass `repo_root` through to provider constructors or use it for config resolution — update those callsites. `config.resolve_checkout_config(repo_root)` will need to accept `&ExecutionEnvironmentPath`.

- [ ] **Step 3: Fix callers of discover_providers()**

The entry points (`InProcessDaemon`, test harnesses) call `discover_providers()` — wrap `repo_root` in `ExecutionEnvironmentPath` where it enters the system.

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -u crates/flotilla-core/
git commit -m "refactor: type Factory::probe() repo_root as ExecutionEnvironmentPath"
```

---

### Task 4: Migrate ConfigStore

Internal paths become `DaemonHostPath`. Repo root paths that pass through ConfigStore (loaded from config, saved by user) become `ExecutionEnvironmentPath`.

**Files:**
- Modify: `crates/flotilla-core/src/config.rs`

- [ ] **Step 1: Change struct fields and constructors**

```rust
pub struct ConfigStore {
    base: DaemonHostPath,  // was PathBuf
    // ...
}
```

`flotilla_config_dir()` returns `DaemonHostPath`. Methods like `repos_dir()`, `tab_order_file()` return `DaemonHostPath`. Methods like `load_repos()`, `load_tab_order()`, `save_repo()` use `ExecutionEnvironmentPath` for repo root paths.

- [ ] **Step 2: Compile and fix cascading errors**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

Fix errors by wrapping at the appropriate boundaries. Key decision points:
- `base_path()` → returns `&DaemonHostPath` (or remove if we want opacity)
- `resolve_checkout_config(repo_root)` → takes `&ExecutionEnvironmentPath`
- `save_repo(path)` → takes `&ExecutionEnvironmentPath`

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-core/src/config.rs
git commit -m "refactor: type ConfigStore paths with DaemonHostPath and ExecutionEnvironmentPath"
```

---

### Task 5: Migrate stores (AttachableStore, AgentStateStore)

Store base paths and file paths are `DaemonHostPath`. Working directories stored inside attachables are `ExecutionEnvironmentPath`.

**Files:**
- Modify: `crates/flotilla-core/src/attachable/store.rs`
- Modify: `crates/flotilla-core/src/attachable/mod.rs` (if `working_directory` fields exist)
- Modify: `crates/flotilla-core/src/agents/store.rs`

- [ ] **Step 1: Change store base paths to DaemonHostPath**

Store constructors and `path` fields → `DaemonHostPath`. The `flotilla_config_dir()` calls that initialize them already return `DaemonHostPath` (from Task 4).

- [ ] **Step 2: Change working directory fields to ExecutionEnvironmentPath**

In `AttachableStoreApi` trait methods, `working_directory: PathBuf` → `ExecutionEnvironmentPath`. This cascades to three impl blocks: `AttachableStore`, `InMemoryAttachableStore`, and `AttachableStoreState` (~12 method signatures across `ensure_terminal_attachable()` and `ensure_terminal_attachable_with_change()`).

- [ ] **Step 3: Compile and fix**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -u crates/flotilla-core/src/attachable/ crates/flotilla-core/src/agents/
git commit -m "refactor: type store paths with DaemonHostPath and ExecutionEnvironmentPath"
```

---

### Task 6: Migrate Vcs and CheckoutManager providers

All paths in the Vcs/CheckoutManager traits and implementations are `ExecutionEnvironmentPath` — they operate on repos and checkouts in the execution environment.

**Files:**
- Modify: `crates/flotilla-core/src/providers/vcs/mod.rs` (traits)
- Modify: `crates/flotilla-core/src/providers/vcs/git.rs`
- Modify: `crates/flotilla-core/src/providers/vcs/git_worktree.rs`
- Modify: `crates/flotilla-core/src/providers/vcs/wt.rs`

- [ ] **Step 1: Change trait signatures**

`Vcs` and `CheckoutManager` method params and returns: `&Path`/`PathBuf` → `&ExecutionEnvironmentPath`/`ExecutionEnvironmentPath`.

- [ ] **Step 2: Compile and fix implementations**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

Git and wt implementations update to match. Internal operations use `as_path()` where `&Path` is needed for std library calls.

- [ ] **Step 3: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 4: Commit**

```bash
git add -u crates/flotilla-core/src/providers/vcs/
git commit -m "refactor: type Vcs/CheckoutManager paths as ExecutionEnvironmentPath"
```

---

### Task 7: Migrate TerminalPool and workspace providers

Terminal working directories are `ExecutionEnvironmentPath`. Workspace state paths (tmux, zellij) are `DaemonHostPath`. Shpool socket/config paths are `DaemonHostPath`.

**Files:**
- Modify: `crates/flotilla-core/src/providers/terminal/mod.rs` (trait)
- Modify: `crates/flotilla-core/src/providers/terminal/cleat.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/shpool.rs`
- Modify: `crates/flotilla-core/src/providers/terminal/passthrough.rs`
- Modify: `crates/flotilla-core/src/providers/workspace/mod.rs` (trait)
- Modify: `crates/flotilla-core/src/providers/workspace/tmux.rs`
- Modify: `crates/flotilla-core/src/providers/workspace/zellij.rs`
- Modify: `crates/flotilla-core/src/providers/workspace/cmux.rs`

- [ ] **Step 1: Change TerminalPool trait**

`ensure_session(cwd: &Path)` → `cwd: &ExecutionEnvironmentPath`. `TerminalSession.working_directory` → `Option<ExecutionEnvironmentPath>`.

- [ ] **Step 2: Change WorkspaceManager trait**

Working directory params → `ExecutionEnvironmentPath`.

- [ ] **Step 3: Change shpool struct fields**

`socket_path` and `config_path` → `DaemonHostPath`.

- [ ] **Step 4: Change tmux/zellij state_path()**

Return type → `DaemonHostPath`. Internal `dirs::config_dir()` calls produce `DaemonHostPath`.

- [ ] **Step 5: Compile and fix all implementations**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

- [ ] **Step 6: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 7: Commit**

```bash
git add -u crates/flotilla-core/src/providers/terminal/ crates/flotilla-core/src/providers/workspace/
git commit -m "refactor: type terminal/workspace paths with newtypes"
```

---

### Task 8: Migrate TerminalManager and hop chain

Terminal manager working directories are `ExecutionEnvironmentPath`. Daemon socket path is `DaemonHostPath`.

**Files:**
- Modify: `crates/flotilla-core/src/terminal_manager.rs`
- Modify: `crates/flotilla-core/src/hop_chain/mod.rs` (if ResolutionContext has paths)
- Modify: `crates/flotilla-core/src/hop_chain/remote.rs` (if SSH working directory)

- [ ] **Step 1: Change TerminalManager fields**

`working_directory` fields → `ExecutionEnvironmentPath`. `daemon_socket_path` → `Option<DaemonHostPath>`.

- [ ] **Step 2: Change hop chain working_directory**

`ResolutionContext.working_directory` → `Option<ExecutionEnvironmentPath>`.

- [ ] **Step 3: Compile and fix**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

- [ ] **Step 4: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -u crates/flotilla-core/src/terminal_manager.rs crates/flotilla-core/src/hop_chain/
git commit -m "refactor: type terminal manager and hop chain paths with newtypes"
```

---

### Task 9: Migrate executor and step system

The executor mixes both path types — repo roots and checkout paths are `ExecutionEnvironmentPath`, config base and daemon socket are `DaemonHostPath`.

**Files:**
- Modify: `crates/flotilla-core/src/executor.rs`
- Modify: `crates/flotilla-core/src/executor/checkout.rs`
- Modify: `crates/flotilla-core/src/executor/terminals.rs`
- Modify: `crates/flotilla-core/src/executor/workspace.rs`
- Modify: `crates/flotilla-core/src/executor/session_actions.rs` (TeleportSessionActionService, TeleportFlow)
- Modify: `crates/flotilla-core/src/step.rs`

- [ ] **Step 1: Change step.rs StepAction paths**

Checkout paths, working directories → `ExecutionEnvironmentPath`. Repo path in step context → `ExecutionEnvironmentPath`.

- [ ] **Step 2: Change executor struct fields**

`root` → `ExecutionEnvironmentPath`. `config_base` → `DaemonHostPath`. `daemon_socket_path` → `Option<DaemonHostPath>`.

- [ ] **Step 3: Change executor sub-modules**

`checkout.rs` repo_root and checkout paths → `ExecutionEnvironmentPath`.
`terminals.rs` daemon_socket_path → `DaemonHostPath`, working dirs → `ExecutionEnvironmentPath`.
`workspace.rs` repo_root → `ExecutionEnvironmentPath`, config_base → `DaemonHostPath`.
`session_actions.rs` repo_root → `ExecutionEnvironmentPath`, config_base → `DaemonHostPath`, daemon_socket_path → `Option<DaemonHostPath>`.

**Key decision points in this task:**
- `workspace_config()` (executor.rs ~line 633): reads `.flotilla/workspace.yaml` from `repo_root` (execution env) and falls back to `config_base.join("workspace.yaml")` (daemon host). This is a real conflation — template search crosses the daemon/execution boundary. Flag with a comment.
- `local_workspace_directory()` (executor.rs ~line 656): uses `dirs::home_dir()` as fallback. This is an `ExecutionEnvironmentPath` but currently resolves from the daemon host's HOME. Flag as needing Phase B PR 2 fix (use EnvVars).

- [ ] **Step 4: Compile and fix**

Run: `cargo build -p flotilla-core 2>&1 | head -80`

This task will likely surface the most conflation errors — the executor is where daemon and execution paths meet. Each error is a classification decision.

- [ ] **Step 5: Run tests**

Run: `cargo test -p flotilla-core --locked`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add -u crates/flotilla-core/src/executor/ crates/flotilla-core/src/executor.rs crates/flotilla-core/src/step.rs
git commit -m "refactor: type executor and step system paths with newtypes"
```

---

### Task 10: Migrate protocol and remaining crates

Paths in the protocol crate (snapshot, commands, provider_data) and other crates that reference paths crossing crate boundaries.

**Files:**
- Modify: `crates/flotilla-protocol/src/commands.rs` (if checkout/repo paths exist)
- Modify: `crates/flotilla-protocol/src/snapshot.rs` (if path fields exist)
- Modify: `crates/flotilla-protocol/src/provider_data.rs` (if checkout paths)
- Modify: `crates/flotilla-core/src/in_process.rs` (daemon initialization)
- Modify: `crates/flotilla-core/src/convert.rs` (core-to-protocol conversion)
- Modify: any remaining files that fail to compile

- [ ] **Step 1: Fix protocol crate paths**

Protocol paths are serialized — they cross the wire between daemons. Decide: do protocol paths carry the newtype, or do they stay as `String`/`PathBuf` and get wrapped/unwrapped at the core↔protocol boundary? Recommendation: protocol stays `PathBuf`/`String` (it's a wire format), and `convert.rs` wraps/unwraps.

- [ ] **Step 2: Fix InProcessDaemon initialization**

`in_process.rs` constructs ConfigStore, stores, and calls discovery. Paths that enter the system here get wrapped at the boundary.

- [ ] **Step 3: Compile the full workspace**

Run: `cargo build --workspace 2>&1 | head -100`

Fix any remaining errors in TUI, daemon, or client crates that reference core types.

- [ ] **Step 4: Run full test suite**

Run: `cargo test --workspace --locked`
Expected: PASS

- [ ] **Step 5: Run CI gates**

Run: `cargo +nightly-2026-03-12 fmt --check && cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "refactor: complete path newtype migration across workspace"
```

---

### Task 11: Review classification decisions

After the mechanical migration, review the decisions made during compilation. Look for:

- [ ] **Step 1: Check for forced conversions**

Search for `.into_path_buf()` and `.as_path()` calls — these are boundary crossings. Each should be justified. Any that look like "I couldn't figure out which type this should be" need investigation.

Run: `grep -rn "into_path_buf\|as_path" crates/flotilla-core/src/ --include="*.rs" | grep -v test | grep -v "mod.rs.*pub fn"`

- [ ] **Step 2: Check for remaining bare PathBuf in provider/discovery layer**

Run: `grep -rn "PathBuf\|&Path" crates/flotilla-core/src/providers/ crates/flotilla-core/src/config.rs crates/flotilla-core/src/executor/ crates/flotilla-core/src/step.rs --include="*.rs" | grep -v test | grep -v "//"`

Any remaining bare `PathBuf` should either be in code that genuinely doesn't care about the distinction (rare) or be flagged for follow-up.

- [ ] **Step 3: Document any tricky classifications**

Add a comment at each non-obvious crossing point explaining why the conversion happens there. Especially:
- Shpool socket (DaemonHostPath created, mounted at ExecutionEnvironmentPath inside container)
- `local_workspace_directory()` in executor.rs (reads HOST's HOME — needs Phase B PR 2 fix)
- `resolve_repo_roots()` in config.rs (reads repo paths from daemon config files, but they refer to execution environment locations — the paths are stored daemon-side but point into execution contexts)
- `workspace_config()` in executor.rs (template search crosses daemon/execution boundary)
- Protocol boundary (`HostPath.path` is an `ExecutionEnvironmentPath` unwrapped to `PathBuf` for serialization)
- `EnvironmentAssertion::AuthFileExists` path — auth files are in the user's home in the execution environment, not daemon state

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "refactor: document path newtype boundary crossings"
```
