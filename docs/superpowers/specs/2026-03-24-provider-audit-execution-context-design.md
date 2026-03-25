# Provider Audit: Execution Context Independence

**Issue:** #472 (Phase B of #442)
**Date:** 2026-03-24

## Problem

Provider factories receive injected `CommandRunner`, `EnvironmentBag`, and `ConfigStore` at probe time, but many providers bypass these abstractions at runtime — re-reading env vars directly, computing paths from `dirs::home_dir()`, or using `Command::new()` without the runner. These assumptions break when discovery runs inside a Docker container via an injected `EnvironmentRunner`.

**Severity levels:** Must-fix blocks container-interior discovery for the Phase C critical path. Should-fix is workaround-able but creates maintenance burden. Tracking issue defers non-blocking work that needs a deeper abstraction.

**Scope:** this audit covers provider factories and their runtime implementations. Test-only code (`#[cfg(test)]` blocks) is excluded — `Command::new()` and `std::env::var()` in test helpers are fine. `ProcessEnvVars` and `ProcessCommandRunner` are the legitimate production implementations of the injected traits, not violations.

## Audit Results

### Clean (no changes needed)

| Provider | Factory | Runtime | Notes |
|----------|---------|---------|-------|
| git (Vcs) | Clean | Clean | All commands via injected runner |
| cleat (TerminalPool) | Clean | Clean | Binary path resolved at probe, stored in struct |
| passthrough (TerminalPool) | Clean | Clean | No-op provider |
| claude (CloudAgent) | Clean | Clean | Runner and HTTP client injected via constructor |
| github (ChangeRequest) | Clean | Clean | Runner and API client injected via constructor |

### Violations found

**Infrastructure (cascading impact):**

| Location | Problem | Severity |
|----------|---------|----------|
| `config.rs:293` `flotilla_config_dir()` | `dirs::home_dir()` hardcoded | Must-fix |
| `config.rs:333` `ConfigStore::new()` | `dirs::home_dir()` hardcoded | Must-fix |
| `discovery/mod.rs:338` `resolve_claude_path()` | `dirs::home_dir()` + `path.is_file()` | Must-fix |
| `detectors/claude.rs:29` | `dirs::home_dir()` + `path.is_file()` | Must-fix |
| `detectors/codex.rs:23` | `dirs::home_dir()` for auth check | Should-fix |
| `factories/shpool.rs:34` | Calls `flotilla_config_dir()` | Cascading (fixed by infra fix) |

**Provider runtime re-reads:**

| Location | Problem | Severity |
|----------|---------|----------|
| `codex.rs:40` `codex_home()` | `std::env::var("CODEX_HOME")` + `dirs::home_dir()` | Must-fix |
| `codex.rs:68` `read_auth()` | Direct `fs::read_to_string()` at host path | Must-fix |
| `cursor.rs:24` `api_key()` | `std::env::var("CURSOR_API_KEY")` at runtime | Must-fix |
| `zellij.rs:108` `session_name()` | `std::env::var("ZELLIJ_SESSION_NAME")` | Should-fix |
| `cmux.rs:12` `CMUX_BIN` | Hardcoded `/Applications/cmux.app/...` path | Should-fix |
| `shpool.rs:265` `start_daemon()` | `tokio::process::Command::new()` bypasses runner | Tracking issue |

**State persistence paths:**

| Location | Problem | Severity |
|----------|---------|----------|
| `tmux.rs:47` `state_path()` | `dirs::config_dir()` | Should-fix |
| `zellij.rs:113` `state_path()` | `dirs::config_dir()` | Should-fix |

## Root Cause Patterns

### Pattern 1: Path resolution is ad-hoc

`dirs::home_dir()` and `dirs::config_dir()` are scattered throughout. #367 already identifies this — a centralized path policy module with env-var-based resolution (`HOME`, `XDG_*`, `FLOTILLA_ROOT`) fixes the cascading infrastructure issues and makes container discovery work because the `EnvironmentRunner` can set these vars appropriately.

### Pattern 2: Providers re-read at runtime what was available at probe

The `Factory::probe()` signature provides everything a provider needs (`env`, `config`, `repo_root`, `runner`). But some providers re-read env vars or auth files at runtime instead of resolving during probe and storing the result. The fix pattern: **detect at probe, pass to constructor, never re-read.**

| Provider | Re-reads at runtime | Should instead |
|----------|-------------------|----------------|
| Codex | `$CODEX_HOME`, auth file | Resolve auth path at probe, pass to constructor |
| Cursor | `$CURSOR_API_KEY` | Already checked at probe — pass value to constructor |
| Zellij | `$ZELLIJ_SESSION_NAME` | Already has `session_name_override` — always use it |
| Cmux | Hardcoded `/Applications/` path | Resolve binary from `EnvironmentBag` at probe, like cleat |

### Pattern 3: ConfigStore is not abstract

`AttachableStore` and `AgentStateStore` are trait-based with test impls. `ConfigStore` is a concrete struct with `dirs::home_dir()` baked in. For Phase B, making its base path injectable (constructor takes `PathBuf`) is sufficient. Full trait abstraction is a Phase C concern.

### Pattern 4: Daemon spawning needs a different abstraction

Shpool's `start_daemon()` uses `tokio::process::Command::new()` because `CommandRunner` is run-and-wait, not spawn-and-background. This is a real limitation — a container-compatible runner would need a `spawn_background()` method. This is out of scope for Phase B; tracked separately.

## Design

### Key distinction: config context vs execution context

Two completely different categories of "path" exist in the system, and the current code conflates them:

**Config/state context (daemon-side):** User preferences, terminal state, agent state, socket locations. These live on the daemon host. They're *about* things that may run in containers, but they don't exist *inside* containers. Access is through abstract store interfaces — nobody sees file paths.

**Execution context (environment-side):** Where is git? Where is HOME? Where is cleat? These are discovered by running commands *inside* the execution environment via the injected `CommandRunner` and `EnvironmentBag`. They come from the environment, not from config.

**Enforce with newtypes:** introduce `DaemonHostPath` and `ExecutionEnvironmentPath` to make the type system catch conflation at compile time:

```rust
/// A path on the daemon host's filesystem. Config, state, sockets.
/// Never valid inside an execution environment.
pub struct DaemonHostPath(PathBuf);

/// A path inside an execution environment. Repo roots, binary locations, working dirs.
/// Resolved via CommandRunner + EnvVars.
pub struct ExecutionEnvironmentPath(PathBuf);
```

Wrap as early as possible — at the point where a path is created or received. Key placements:
- `ConfigStore` internals, `PathPolicy`, store paths → `DaemonHostPath`
- `repo_root` in `Factory::probe()` → `ExecutionEnvironmentPath`
- `EnvironmentBag` binary locations → `ExecutionEnvironmentPath`
- Provider struct fields (binary paths, working dirs) → `ExecutionEnvironmentPath`
- Hop chain working directories → `ExecutionEnvironmentPath`

The crossing point is mount boundaries: a daemon socket is a `DaemonHostPath` when created, but becomes accessible at an `ExecutionEnvironmentPath` inside the container. The mount configuration explicitly maps between the two domains.

When discovery runs inside a container:
- ConfigStore stays on the daemon host, serves preferences to whoever asks (which backend to use, checkout strategy, etc.)
- The runner + env vars point inside the container
- `Factory::probe()` already receives *both*: `config` (daemon-side prefs) and `runner` + `env` (environment-side execution)

The fix is not "make ConfigStore work inside the container." It is: make ConfigStore opaque so nobody computes paths from its internals, and ensure execution-context paths come from the runner/env.

### 1. Opaque ConfigStore

Nobody calls `config.base_path()` and does path arithmetic on the result. The store provides data through methods; its storage layout is an internal detail.

Currently, code outside ConfigStore calls `flotilla_config_dir()` to compute paths for shpool sockets, tmux state files, etc. These paths are not config — they are runtime state. They should go through a state storage abstraction, not through path arithmetic on a config directory.

```rust
// Before (leaks paths):
let socket_path = flotilla_config_dir().join("shpool/shpool.socket");
let state_path = dirs::config_dir().join("flotilla/tmux").join(session).join("state.toml");

// After (opaque):
// Config: ask for config data, get data back
let checkout_config = config.resolve_checkout_config(repo_root);

// State: ask the state store, it manages its own paths
let socket_path = state_store.shpool_socket_path();
let state = state_store.load_workspace_state("tmux", session);
```

ConfigStore's constructor still needs a base path internally (for its own file I/O), but this is resolved by the daemon at startup via the path policy module and never exposed to consumers.

### 2. Path policy module (#367, internal to stores)

Centralize the daemon's own file layout. All daemon-managed paths resolve through a single module:

```rust
pub struct PathPolicy {
    config_dir: PathBuf,  // XDG_CONFIG_HOME/flotilla or FLOTILLA_ROOT/config
    data_dir: PathBuf,    // XDG_DATA_HOME/flotilla or FLOTILLA_ROOT/data
    state_dir: PathBuf,   // XDG_STATE_HOME/flotilla or FLOTILLA_ROOT/state
    cache_dir: PathBuf,   // XDG_CACHE_HOME/flotilla or FLOTILLA_ROOT/cache
}
```

Resolution order: `FLOTILLA_ROOT` → XDG env var → `dirs::` fallback.

This is an **internal implementation detail** of the stores and the daemon, not exposed to providers. Providers never see daemon-side paths — they see config values and execution results.

### 3. Push probe-time resolution (execution context)

For each provider that re-reads execution-context values at runtime:

**Codex:** Resolve `codex_home` path during probe (from `EnvironmentBag` which already has `$CODEX_HOME` and home dir assertions). Read auth file during probe. Pass resolved auth data to constructor.

**Cursor:** Pass `$CURSOR_API_KEY` value to constructor (already validated during probe).

**Zellij:** Always use `session_name_override` path — factory already supports this. Remove the `std::env::var` fallback.

**Cmux:** The binary is a macOS app bundle at `/Applications/cmux.app/.../cmux` — it's not on PATH by design. The factory probe should detect the binary location using platform-aware logic (check known locations using the runner, not hardcoded path constants in the provider struct). Pass resolved binary path to constructor, like cleat does.

### 4. Execution-context binary and path discovery

Binary lookups for tools like Claude (`~/.claude/local/claude`) and Cmux resolve `HOME` from the injected `EnvVars` trait, not from `dirs::home_dir()`. The detectors already receive `runner` and `env` — they should use them consistently. Platform-specific known paths are checked via the runner (`runner.exists(path, &["--version"])`) with `HOME` from env vars.

## What this does NOT address (tracked for Phase C)

### Store data model changes

The stores (AttachableStore, AgentStateStore) will need environment awareness when environments exist. Terminals, agents, and attachable sets that live inside an environment need `environment_id: Option<EnvironmentId>` — where `None` means the daemon's ambient environment. This is a Phase C data model change. Phase B makes the stores opaque and properly separated (config vs state vs data); Phase C adds the environment dimension to the *data* they store.

Note: these stores remain daemon-side even with environments. The daemon stores data *about* things running in containers — the stores themselves don't move into containers. The `EnvironmentHandle` provides a runner for executing commands inside the container; the stores track what the daemon knows about those commands' results.

### HostName semantics

`HostName` currently conflates three concepts:
- **Routing target** — which daemon handles commands
- **Physical machine** — where hardware resources are
- **Execution context** — the environment where code runs

With managed environments (no daemon inside), the execution context separates from the daemon node. The host's bare-metal context becomes the "ambient environment" — an always-present environment that doesn't need provisioning.

The data model implication: every attachable, agent session, and checkout exists within an environment. The ambient environment is `None` (or a sentinel `EnvironmentId`). This means:
- `AttachableSet.host_affinity` might become `(HostName, Option<EnvironmentId>)`
- Provider trees become per-environment, not per-host
- ConfigStore stays host-scoped (environments receive projected config, not their own)

These are Phase C design decisions. Phase B's job is to not paint into a corner — which the path policy and probe-time resolution changes achieve by making the infrastructure environment-agnostic without requiring environment awareness.

### Full ConfigStore trait abstraction

Phase B makes ConfigStore opaque (no path leakage) but keeps it as a concrete struct. Making it a trait (like AttachableStore) would enable in-memory test implementations, environment-specific config projections, and read-only views. Valuable but deferred — the opacity change is sufficient for now.

### Daemon process lifecycle

`CommandRunner` is run-and-wait. Shpool needs spawn-and-background for its daemon. A `spawn_background()` method on the runner (or a separate `ProcessLifecycle` trait) would make this container-compatible. Tracked separately — shpool's daemon spawning works fine for the host case and doesn't block Phase C.

## Implementation Plan

### PR 1: Newtype migration (the audit)

Introduce `DaemonHostPath` and `ExecutionEnvironmentPath`. Classify every `PathBuf`/`&Path` in the provider and discovery layer as one or the other. Change signatures, let the compiler find every conflation point.

Key signatures that change:
- `Factory::probe()`: `repo_root: &Path` → `&ExecutionEnvironmentPath`
- Provider struct fields: binary paths, working dirs → `ExecutionEnvironmentPath`
- `ConfigStore` internals, store base paths → `DaemonHostPath`
- `EnvironmentBag` assertion paths (binary locations) → `ExecutionEnvironmentPath`

This PR is the audit. Instead of a document listing violations, the compiler finds them all. Every place where a `DaemonHostPath` is passed where an `ExecutionEnvironmentPath` is expected (or vice versa) is a conflation that needs resolving.

Classification decisions that will surface:
- Shpool socket: `DaemonHostPath` on the daemon, but mounted at an `ExecutionEnvironmentPath` inside a container. The mount configuration is the explicit crossing point.
- Cmux binary: `ExecutionEnvironmentPath` — it's discovered in the execution environment via the runner.
- `repo_root`: `ExecutionEnvironmentPath` — it's where code lives in the execution environment.

### PR 2: Fix detector host assumptions

Claude, Codex, and Cmux detectors use `env.get("HOME")` (from the injected `EnvVars` trait) instead of `dirs::home_dir()`. Detectors resolve HOME-dependent paths and put results into the bag. Factories then read resolved paths from the bag, never computing paths from HOME themselves.

Cmux: the `/Applications/cmux.app/...` path is correct for macOS — cmux is never on PATH. The detector checks this known location via the runner (`runner.exists(path, &["--version"])`), not via `path.is_file()`. Inside a container, the detector won't find cmux, probe fails with `UnmetRequirement`, provider isn't constructed. Correct behavior.

### PR 3: Fix probe-time re-reads

**Codex:** resolve auth *path* at probe (from `EnvironmentBag`), pass to constructor. Runtime re-reads of auth.json go through the runner at the resolved path — not frozen at probe time, because auth tokens refresh. The change: the *location* is resolved once; the *content* is read at runtime through the runner.

**Cursor:** pass `$CURSOR_API_KEY` value to constructor (already validated during probe).

**Zellij:** always use `session_name_override` — factory already supports this. Remove `std::env::var` fallback.

### PR 4: ConfigStore opacity + path policy

Remove `flotilla_config_dir()` as a public function. ConfigStore constructor takes an explicit `DaemonHostPath` base. Path policy module resolves the base internally (XDG → `dirs::` fallback). State paths (shpool socket, tmux/zellij state) move to appropriate `DaemonHostPath` locations via the path policy, not computed from config base. No paths leak through public APIs.

### Verification

Each PR runs the full CI gate (fmt, clippy, test). PR 1 (newtypes) adds a compile-time guarantee that no new conflation is introduced. PR 4 adds a test that constructs a `PathPolicy` from explicit env vars and verifies all paths resolve correctly.
