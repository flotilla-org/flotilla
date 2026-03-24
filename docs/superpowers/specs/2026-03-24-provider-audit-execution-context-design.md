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

### 1. Path policy module (#367)

Centralize path resolution. All flotilla-managed paths resolve through a single module that checks env vars before falling back to `dirs::`:

```rust
pub struct PathPolicy {
    config_dir: PathBuf,  // XDG_CONFIG_HOME/flotilla or FLOTILLA_ROOT/config
    data_dir: PathBuf,    // XDG_DATA_HOME/flotilla or FLOTILLA_ROOT/data
    state_dir: PathBuf,   // XDG_STATE_HOME/flotilla or FLOTILLA_ROOT/state
    cache_dir: PathBuf,   // XDG_CACHE_HOME/flotilla or FLOTILLA_ROOT/cache
}

impl PathPolicy {
    pub fn from_env(env: &dyn EnvVars) -> Self;           // host discovery (reads process env)
    pub fn from_env_vars(vars: &HashMap<String, String>) -> Self; // container discovery (Phase C: raw vars from EnvironmentHandle::env_vars())
}
```

Resolution order per category:
1. `FLOTILLA_ROOT` → `<root>/<category>`
2. Category-specific XDG env var
3. `dirs::` fallback
4. Hardcoded default only if all else fails

This replaces every `flotilla_config_dir()` call and every `dirs::config_dir()` / `dirs::home_dir()` call for flotilla-managed paths. Binary lookups (like claude's `~/.claude/local/claude`) resolve `HOME` from env vars rather than `dirs::home_dir()`.

### 2. Push probe-time resolution

For each provider that re-reads at runtime:

**Codex:** Resolve `codex_home` path during probe (from `EnvironmentBag` which already has `$CODEX_HOME` and home dir assertions). Read auth file during probe. Pass resolved auth data to constructor.

**Cursor:** Pass `$CURSOR_API_KEY` value to constructor (already validated during probe).

**Zellij:** Always use `session_name_override` path — factory already supports this. Remove the `std::env::var` fallback.

**Cmux:** Resolve binary path from `EnvironmentBag` during probe (like cleat does), pass to constructor. Remove hardcoded `/Applications/` path.

### 3. Injectable ConfigStore base path

`ConfigStore::new()` takes an explicit `base: PathBuf` instead of computing it from `dirs::home_dir()`. The caller (typically `InProcessDaemon` or `DiscoveryRuntime`) resolves the path via the path policy module.

```rust
impl ConfigStore {
    pub fn new(base: PathBuf) -> Self { ... }
    // Remove: pub fn new() that calls dirs::home_dir()
}
```

### 4. State persistence via path policy

Tmux and Zellij `state_path()` methods use the path policy's `state_dir` instead of `dirs::config_dir()`. This also addresses #367's concern about mixing config and state.

```rust
// Before:
fn state_path(session: &str) -> Result<PathBuf, String> {
    let config_dir = dirs::config_dir().ok_or(...)?;
    Ok(config_dir.join("flotilla/tmux").join(session).join("state.toml"))
}

// After: accept state_dir as parameter or from stored PathPolicy
fn state_path(state_dir: &Path, session: &str) -> PathBuf {
    state_dir.join("tmux").join(session).join("state.toml")
}
```

## What this does NOT address (tracked for Phase C)

### Store data model changes

The stores (AttachableStore, AgentStateStore) will need environment awareness when environments exist. Terminals, agents, and attachable sets that live inside an environment need `environment_id: Option<EnvironmentId>` — where `None` means the daemon's ambient environment. This is a Phase C data model change, not a Phase B concern. Phase B ensures the stores' *initialization* is injectable (base path comes from path policy, not hardcoded); Phase C adds the environment dimension to the *data* they store.

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

### Full ConfigStore abstraction

Making ConfigStore a trait (like AttachableStore) would allow environment-specific config projections, read-only views, and in-memory test implementations. This is valuable but not needed for Phase B. The injectable base path is sufficient.

### Daemon process lifecycle

`CommandRunner` is run-and-wait. Shpool needs spawn-and-background for its daemon. A `spawn_background()` method on the runner (or a separate `ProcessLifecycle` trait) would make this container-compatible. Tracked separately — shpool's daemon spawning works fine for the host case and doesn't block Phase C.

## Implementation Plan

### Step 1: Path policy module

New module (probably `crates/flotilla-core/src/path_policy.rs`) implementing `PathPolicy::from_env()`. Replaces all `flotilla_config_dir()` calls. Classify existing files into config/data/state/cache per #367.

### Step 2: Thread PathPolicy through initialization

`DiscoveryRuntime`, `InProcessDaemon`, `ConfigStore`, `AttachableStore`, `AgentStateStore` all receive paths from a `PathPolicy` instance rather than computing them.

### Step 3: Fix probe-time re-reads

Codex auth, Cursor API key, Zellij session name, Cmux binary path — resolve at probe, pass to constructor.

### Step 4: Fix detector host assumptions

Claude and Codex detectors use `HOME` from env vars (via `EnvVars` trait) instead of `dirs::home_dir()`.

### Step 5: Verification

Run existing test suite. Add test that constructs a `PathPolicy` from explicit env vars and verifies all paths resolve to the expected locations. Verify that `Factory::probe()` for git and cleat works with a mock runner and custom env vars (simulating container-interior discovery).
