# Flotilla

A system for improving multi-agent developer workflows. Consists of a daemon mesh, CLI, and TUI dashboard — managing development workspaces across git worktrees, code review (GitHub PRs), issue trackers, cloud agents (Claude Code, Codex, Cursor), terminal persistence (cleat, shpool), and multiplexers (cmux, tmux, zellij).

## Development Phase

We are in a **no backwards compatibility** phase. Protocol types, snapshot formats, config file formats, and wire formats can all change freely without migration logic or deprecation paths.

## Quick Reference

```bash
cargo build                                    # build
cargo +nightly-2026-03-12 fmt --check          # CI format gate
cargo clippy --workspace --all-targets --locked -- -D warnings  # CI clippy gate
cargo test --workspace --locked                # CI test gate
cargo +nightly-2026-03-12 fmt                  # apply pinned formatting
cargo dylint --all -- --all-targets             # custom lints (requires cargo-dylint + dylint-link)
cargo run                                      # run, auto-detect repo from cwd
```

Before pushing, run the exact CI commands: `cargo +nightly-2026-03-12 fmt --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`, and `cargo test --workspace --locked`.

**Nightly toolchain:** All nightly-dependent tools (rustfmt, llvm-cov, Dylint) are pinned to `nightly-2026-03-12`. Install with `rustup toolchain install nightly-2026-03-12 --component rustfmt llvm-tools-preview`.

In the Codex sandbox, prefer `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests` so native dependencies can create temp files and socket-bind tests stay skipped.

Package-local `flotilla-core` daemon integration coverage uses shared discovery test helpers behind `test-support`, so run it as:

```bash
cargo test -p flotilla-core --locked --features test-support --test in_process_daemon
```

## Testing Philosophy

- Prefer behavior tests that run through injected collaborators over tests that depend on real filesystem state, subprocess orchestration, sockets, or live multi-host setup.
- When a subsystem has multiple storage or transport implementations, specify the behavior once and run the same contract tests against each implementation.
- Use in-memory implementations for most logical scenario tests when they make setup clearer and failures easier to reason about.
- Keep real-backed implementations covered too, but verify them against the same behavioral contract instead of forcing all behavior tests through the real backing store.
- Favor reusable test harnesses over ad hoc setup. The goal is to make new multi-step scenarios cheap to express and debug.
- For multi-host orchestration logic, prefer `InProcessDaemon`-level tests unless the bug specifically depends on real process or transport boundaries.
- **Snapshot tests are a signal, not a formality.** Never run `cargo insta accept` or update snapshots just because a test failed. A failing snapshot means the rendered output changed — investigate *why* it changed. If the change is an intended consequence of the current work, accept it with a clear justification. If the change is unintended, it's a bug — fix the code, don't update the snapshot.

## Claude Code Web (changedirection/flotilla fork)

This fork exists for Claude Code Web sessions. Two things to be aware of:

- **Upstream repo for gh commands**: Issues and PRs live on the upstream `flotilla-org/flotilla`. Always pass `-R flotilla-org/flotilla` to `gh issue` and `gh pr` commands (e.g. `gh issue list -R flotilla-org/flotilla`, `gh pr create -R flotilla-org/flotilla`). PRs should target `flotilla-org/flotilla:main` as the base.
- **No local servers**: The sandbox blocks binding ports, so MCP servers and visualization tools that start local HTTP servers won't work.

## Architecture

Provider-based plugin system with data correlation:

```
Providers (git, wt, github, claude, codex, cursor, cmux, tmux, zellij, cleat, shpool)
  → RepoModel (providers, health, correlation groups)
    → Correlation engine (union-find groups items by shared keys)
      → WorkItems (correlated work units)
        → UI (ratatui widget tree) / CLI / daemon events
```

User actions flow: **Intent → Command → Executor → Provider call → Refresh**

### Crate Structure

| Crate | Role |
|-------|------|
| `flotilla-core` | Providers, correlation, refresh, executor, config, agents, attachables, step plans, `DaemonHandle` trait, `InProcessDaemon` |
| `flotilla-protocol` | Serde-only types: commands, results, snapshots, events, envelope |
| `flotilla-client` | Socket client: `SocketDaemon`, `connect_or_spawn`, gap recovery |
| `flotilla-tui` | UI rendering (widget tree), input handling, binding table, keymap, event loop, CLI parsing |
| `flotilla-daemon` | Socket server, peer networking, multi-host command routing |
| `cleat` | Terminal I/O: PTY management, VT engine, session persistence |
| `flotilla` (root) | Thin `src/main.rs` entry point |

### Key Modules

| Path | Role |
|------|------|
| `src/main.rs` | Entry point, CLI dispatch |
| `crates/flotilla-core/src/daemon.rs` | `DaemonHandle` trait |
| `crates/flotilla-core/src/in_process.rs` | `InProcessDaemon` implementation |
| `crates/flotilla-core/src/executor.rs` | Executes commands against providers, returns `CommandResult` |
| `crates/flotilla-core/src/executor/` | Executor submodules: checkout, workspace, terminals, session actions |
| `crates/flotilla-core/src/model.rs` | `AppModel` (repos, labels), `RepoModel` (per-repo data) |
| `crates/flotilla-core/src/data.rs` | `WorkItem`, `TableEntry`, correlation + table building |
| `crates/flotilla-core/src/step.rs` | Step planning and execution system |
| `crates/flotilla-core/src/agents/` | Agent hook handling and state management |
| `crates/flotilla-core/src/attachable/` | Attachable session set management |
| `crates/flotilla-core/src/convert.rs` | Core-to-protocol type conversion |
| `crates/flotilla-core/src/providers/` | Provider traits, implementations, registry, discovery, correlation |
| `crates/flotilla-core/src/config.rs` | Persistence to `~/.config/flotilla/` |
| `crates/flotilla-core/src/template.rs` | `.flotilla/workspace.yaml` pane templates |
| `crates/flotilla-protocol/src/lib.rs` | `Message` envelope, `DaemonEvent` |
| `crates/flotilla-protocol/src/commands.rs` | `ProtoCommand`, `CommandResult` |
| `crates/flotilla-protocol/src/snapshot.rs` | `Snapshot`, `ProtoWorkItem`, `RepoInfo` |
| `crates/flotilla-daemon/src/server.rs` | Daemon server with peer networking |
| `crates/flotilla-daemon/src/server/` | Server submodules: client/peer connections, request dispatch, remote commands |
| `crates/flotilla-tui/src/app/mod.rs` | `App` struct, key/mouse dispatch, mode transitions |
| `crates/flotilla-tui/src/app/intent.rs` | `Intent` enum, resolves to `ProtoCommand` |
| `crates/flotilla-tui/src/app/executor.rs` | Thin executor: routes to core, interprets results into UI state |
| `crates/flotilla-tui/src/app/key_handlers.rs` | Key handling logic per binding mode |
| `crates/flotilla-tui/src/app/navigation.rs` | Navigation logic |
| `crates/flotilla-tui/src/app/ui_state.rs` | `UiState`, `TabId`, `UiMode`, per-repo UI state |
| `crates/flotilla-tui/src/binding_table.rs` | `BindingModeId`, flat keybinding table |
| `crates/flotilla-tui/src/keymap.rs` | Keymap management, configurable key bindings |
| `crates/flotilla-tui/src/cli.rs` | CLI subcommand parsing |
| `crates/flotilla-tui/src/widgets/` | Widget tree: `screen`, `tabs`, `repo_page`, `overview_page`, `command_palette`, `work_item_table`, etc. |
| `crates/flotilla-tui/src/ui_helpers.rs` | Shared rendering utilities |
| `crates/flotilla-tui/src/event.rs` | Terminal event stream (key, mouse, tick) |
| `crates/flotilla-tui/src/event_log.rs` | In-app tracing log with level filtering |

### Provider Traits

Each trait lives in `crates/flotilla-core/src/providers/<category>/mod.rs` with implementations alongside:

- **Vcs** + **CheckoutManager** (`vcs/git.rs`, `vcs/wt.rs`)
- **ChangeRequestTracker** (`change_request/github.rs`)
- **IssueTracker** (`issue_tracker/github.rs`)
- **CloudAgentService** (`coding_agent/claude.rs`, `coding_agent/codex.rs`, `coding_agent/cursor.rs`)
- **AiUtility** (`ai_utility/claude.rs`)
- **WorkspaceManager** (`workspace/cmux.rs`, `workspace/tmux.rs`, `workspace/zellij.rs`)
- **TerminalPool** (`terminal/cleat.rs`, `terminal/shpool.rs`, `terminal/passthrough.rs`)

Every provider trait has label methods: `section_label()`, `item_noun()`, `abbreviation()`, `display_name()`. Override defaults in implementations for custom terminology.

### Discovery and Provider Construction

Providers are constructed via **factories** (`discovery/factories/`) that receive an `EnvironmentBag` — a typed collection of `EnvironmentAssertion` values populated by **detectors** (`discovery/detectors/`).

- **Host detectors** (`HostDetector`) probe the daemon's environment: available binaries, env vars, sockets. They receive an injected `CommandRunner` and `EnvVars` trait — **never use `std::env` directly** in providers or factories.
- **Repo detectors** (`RepoDetector`) probe per-repo state (VCS roots, remotes).
- The `EnvironmentBag` provides typed queries: `find_binary()`, `find_env_var()`, `find_socket()`, `find_vcs_checkout()`, `find_remote_host()`.
- Factories call `env.find_binary("tool")` to check availability, `env.find_env_var("KEY")` for env values, and receive a `ConfigStore` for user preferences.

**Why injected collaborators?** The daemon may run discovery for remote hosts or container environments where the host process's own env vars and binaries are wrong. The `EnvVars` trait and `CommandRunner` trait abstract this so tests can inject values and environments can provide their own. Never use `std::env` or `std::process::Command` directly in providers — always go through the injected `EnvVars` and `CommandRunner`.

### Correlation

Union-find over `CorrelationKey` values (`Branch`, `CheckoutPath`, `AttachableSet`, `ChangeRequestRef`, `SessionRef`). Items sharing any key merge into a single `WorkItem`. Issues link post-correlation via `AssociationKey` (don't cause merges). Tests in `crates/flotilla-core/src/providers/correlation.rs`.

## Conventions

- **Commits**: `type: lowercase description` — types: feat, fix, refactor, chore, docs. Present tense, no period.
- **Errors**: Provider methods return `Result<T, String>`. App-level uses `color_eyre::Result`.
- **Async**: `async-trait` for provider traits, `tokio::join!` for parallel refresh.
- **Enums over bools**: Prefer enum variants for state (e.g. `UiMode`, `Intent`, `WorkItemKind`).
- **Formatting**: `cargo +nightly-2026-03-12 fmt` — uses `max_width=140`, `imports_granularity="Crate"`, `group_imports="StdExternalCrate"`. See `rustfmt.toml`.
- **Inline paths**: Prefer `use` imports over long inline `crate::`/`self::`/`super::` paths (>3 segments). Enforced by a Dylint lint (`cargo dylint --all -- --all-targets`). Config in `dylint.toml`.
- **Imports**: std first, external crates, then `use crate::...`.
- **Adding dependencies is fine** when they solve a real problem — don't reinvent the wheel.
- **`expect` over `unwrap`**: Prefer `.expect("reason")` over `.unwrap()` — it avoids having to reason about whether each `unwrap` is safe.
- **Correctness first**: Always favour correct solutions over "pragmatic" shortcuts. Get the architecture right rather than patching around structural problems.
- **Tracing**: Use structured fields, not format-string interpolation. Fields go before the message: `debug!(repo = %path.display(), %since, "issue incremental")`. Use `%` for Display, `?` for Debug, and shorthand `%var` when the field name matches the variable name.

## UI Modes

`UiMode` has three variants: `Normal`, `Config`, `IssueSearch`. Most modal behaviour is driven by `BindingModeId` in the binding table:

`Shared`, `Normal`, `Overview`, `Help`, `ActionMenu`, `DeleteConfirm`, `CloseConfirm`, `BranchInput`, `IssueSearch`, `CommandPalette`, `FilePicker`, `SearchActive`

Key bindings are configurable via TOML. Defaults: `j/k` navigate, `Enter` execute, `Space` multi-select, `.` action menu, `d` delete, `p` open PR, `n` new branch, `r` refresh, `[/]` switch tabs, `{/}` reorder tabs, `:` command palette, `/` search, `q` quit, `?` help.

## Config

Stored in `~/.config/flotilla/`:
- `repos/*.toml` — one per tracked repo (`path = "..."`, plus per-provider config: `change_request`, `issue_tracker`, `cloud_agent`, `ai_utility`, `workspace_manager`, `terminal_pool`, `vcs.git`)
- `tab-order.json` — array of repo paths
- `keybindings.toml` — user key binding overrides

Workspace templates: `.flotilla/workspace.yaml` in repo root.

## Issue Types and Labels

Issues have a **type** (lifecycle stage) and **labels** (topic tags). `gh issue create` does not support `--type`, so set it after creation via the API: `gh api -X PATCH repos/flotilla-org/flotilla/issues/N -f type="TypeName"`.

| Type | Use for |
|------|---------|
| `Task` | A specific piece of work |
| `Bug` | An unexpected problem or behavior |
| `Feature` | A request, idea, or new functionality |
| `Brainstorm` | Needs design thinking before it can become a task or feature |

Use labels to tag topics. Combine as appropriate (e.g. `bug` + `ui`, or `from-review` + `refactor` + `quick-win`).

| Label | Use for |
|-------|---------|
| `bug` | Something broken or incorrect |
| `enhancement` | New feature or improvement to existing feature |
| `refactor` | Code restructuring for maintainability |
| `ui` | UI/UX and rendering changes |
| `testing` | Test coverage and testing infrastructure |
| `multi-host` | Multi-host peering and networking |
| `protocol` | Wire protocol, message format, handshake |
| `cli` | CLI commands and output formatting |
| `infrastructure` | Build, CI, and tooling |
| `integration` | New provider or tool integration |
| `provider` | Provider infrastructure or implementation |
| `vision` | Long-term direction, ambitious features |
| `documentation` | Docs improvements |
| `from-review` | Issue filed from PR review feedback |
| `quick-win` | Small, well-scoped tasks suitable for agent batches |
| `good first issue` | Good for newcomers |
| `duplicate` | Already exists |
| `wontfix` | Will not be worked on |

## Testing Providers with Record/Replay

Providers (git, GitHub, Claude, etc.) integrate with external systems. The `replay` module captures and replays interactions to enable deterministic, offline testing.

### Pattern

Tests follow a 5-step workflow:

```rust
fn fixture(name: &str) -> String {
    format!("{}/src/providers/<category>/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"), name)
}

#[tokio::test]
async fn test_my_provider() {
    let session = replay::test_session(&fixture("my_fixture.yaml"), masks);
    let runner = replay::test_runner(&session);
    let gh_api = replay::test_gh_api(&session, &runner);
    let http = replay::test_http_client(&session);

    // Inject replay implementations into your provider
    let provider = MyProvider::new(runner, gh_api, http);

    // Assert expected behavior
    let result = provider.some_method().await;
    assert!(result.is_ok());

    // Verify all fixtures were consumed
    session.finish();
}
```

In replay mode (the default), fixtures load from disk and responses are deterministic. In record mode (`REPLAY=record`), real interactions are captured and masking applies to the fixture before saving. In passthrough mode (`REPLAY=passthrough`), real commands execute without reading or writing fixtures — useful for validating that tests work against live execution.

### Fixture Format

YAML fixtures contain `interactions` — a sequence of captured requests and responses. Each interaction has a `channel` tag: `command`, `gh_api`, or `http`.

**Command channel** (shell execution):
```yaml
interactions:
  - channel: command
    cmd: git
    args: [branch, --list, --format=%(refname:short)]
    cwd: '{repo}'
    stdout: |
      main
      feature/foo
    stderr: null
    exit_code: 0
```

**GitHub API channel** (gh CLI or GhApiClient):
```yaml
interactions:
  - channel: gh_api
    method: GET
    endpoint: repos/owner/repo/pulls?state=open&per_page=100
    status: 200
    body: '[{"number": 42, "title": "Fix bug"}]'
    headers:
      etag: 'W/"abc123"'
      total_count: "1"
```

**HTTP channel** (reqwest client):
```yaml
interactions:
  - channel: http
    method: GET
    url: https://api.anthropic.com/v1/sessions
    request_headers:
      authorization: Bearer token-123
      anthropic-version: "2023-06-01"
    status: 200
    response_body: '{"data": [{"id": "s1", "title": "Work"}]}'
    response_headers:
      content-type: application/json
```

Note: `{repo}` in `cwd` fields is a **mask placeholder** (see Masks section). It is replaced with the real path during recording and restored during replay.

### Recording

Capture real interactions:

```bash
REPLAY=record cargo test -p flotilla-core test_my_provider
```

This runs your test against real systems (git, GitHub, Claude API, HTTP endpoints). The test must succeed. On exit, the fixture file is written with all interactions masked.

**Never edit replay fixture YAML files directly.** Always re-record them against the real system with `REPLAY=record`. Fixtures capture real interaction sequences including exact command arguments, response formats, and ordering — hand-editing risks subtle mismatches that pass in replay but fail against the real system. If recording requires setup that can't be automated (e.g. a running cmux instance, an active zellij session), flag it for human intervention.

### Passthrough

Validate tests against live execution without writing fixtures:

```bash
REPLAY=passthrough cargo test -p flotilla-core test_my_provider
```

This is useful on properly-equipped CI runners (e.g. macOS for cmux/claude) to verify that replay fixtures haven't drifted from real behavior.

### Masks

Masks replace sensitive or environment-dependent values (paths, tokens, IDs) with placeholders before saving fixtures. During replay, placeholders are restored.

```rust
let mut masks = Masks::new();
masks.add("/Users/alice/dev/my-repo", "{repo}");
masks.add("/Users/alice", "{home}");
masks.add("ghp_secrettoken123", "{github_token}");

let session = replay::test_session(&fixture("my.yaml"), masks);
```

**Important:** Register longer (more specific) values first. Shorter values can partially match longer ones.
