# CommandRunner Trait Design

Issue: #73 — introduce CommandRunner trait for mockable CLI providers

## Problem

Coverage sits at ~35%. Most providers shell out via the bare `run_cmd()` free function, which has no injectable seam. Unit testing provider logic requires real CLI tools installed.

## Design

### Trait

```rust
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(&self, cmd: &str, args: &[&str], cwd: &Path) -> Result<String, String>;
    async fn exists(&self, cmd: &str, args: &[&str]) -> bool;
}
```

Defined in `crates/flotilla-core/src/providers/mod.rs`.

### Default implementation

`ProcessCommandRunner` wraps existing `tokio::process::Command` logic from `run_cmd()` and `command_exists()`. Stdin detached, stdout/stderr captured.

### Injection

All affected structs gain an `runner: Arc<dyn CommandRunner>` field:

- `GitVcs`, `WtCheckoutManager`, `GitWorktreeCheckoutManager`
- `CmuxWorkspaceManager`, `TmuxWorkspaceManager`, `ZellijWorkspaceManager`
- `GhApiClient`, `ClaudeCodingAgent`
- `Executor`

`discovery.rs` creates one shared `Arc<ProcessCommandRunner>` and passes it to all providers.

### Migration

- 28 `run_cmd()` call sites become `self.runner.run()`
- 5 `command_exists()` call sites become `self.runner.exists()`
- `zellij.rs` direct `Command::new()` calls migrate to `self.runner.run()`
- `claude.rs` keychain `Command::new()` call migrates to `self.runner.run()`
- `resolve_claude_path()` takes `&dyn CommandRunner` parameter

### Unchanged

- `git.rs` `resolve_repo_root` — synchronous, cannot use async trait

### Follow-up

Executor's direct `gh`/`git` calls should be delegated to providers (separate issue).
