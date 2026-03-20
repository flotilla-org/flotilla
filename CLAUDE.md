# Flotilla

TUI dashboard for managing development workspaces across git worktrees, code review (GitHub PRs), issue trackers, cloud agent sessions (Claude), and terminal multiplexers (cmux).

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
cargo run -- --repo-root /path                 # run with explicit repo
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
Providers (git, wt, github, claude, cmux)
  → RepoModel (providers, health, correlation groups)
    → Correlation engine (union-find groups items by shared keys)
      → WorkItems (correlated work units)
        → UI (ratatui table + tabs)
```

User actions flow: **Intent → Command → Executor → Provider call → Refresh**

### Crate Structure

| Crate | Role |
|-------|------|
| `flotilla-core` | Providers, correlation, refresh, executor, config, `DaemonHandle` trait, `InProcessDaemon` |
| `flotilla-protocol` | Serde-only types: commands, results, snapshots, events, envelope |
| `flotilla-client` | Socket client: `SocketDaemon`, `connect_or_spawn`, gap recovery |
| `flotilla-tui` | UI rendering, input handling, event loop, thin executor wrapper |
| `flotilla-daemon` | Socket server (Step 2, placeholder) |
| `flotilla` (root) | Thin `src/main.rs` entry point |

### Key Modules

| Path | Role |
|------|------|
| `src/main.rs` | Entry point, event loop, mouse handling |
| `crates/flotilla-core/src/daemon.rs` | `DaemonHandle` trait |
| `crates/flotilla-core/src/in_process.rs` | `InProcessDaemon` implementation |
| `crates/flotilla-core/src/executor.rs` | Executes commands against providers, returns `CommandResult` |
| `crates/flotilla-core/src/model.rs` | `AppModel` (repos, labels), `RepoModel` (per-repo data) |
| `crates/flotilla-core/src/data.rs` | `WorkItem`, `TableEntry`, correlation + table building |
| `crates/flotilla-core/src/convert.rs` | Core-to-protocol type conversion |
| `crates/flotilla-core/src/providers/` | Provider traits, implementations, registry, discovery, correlation |
| `crates/flotilla-core/src/config.rs` | Persistence to `~/.config/flotilla/` |
| `crates/flotilla-core/src/template.rs` | `.flotilla/workspace.yaml` pane templates |
| `crates/flotilla-protocol/src/lib.rs` | `Message` envelope, `DaemonEvent` |
| `crates/flotilla-protocol/src/commands.rs` | `ProtoCommand`, `CommandResult` |
| `crates/flotilla-protocol/src/snapshot.rs` | `Snapshot`, `ProtoWorkItem`, `RepoInfo` |
| `crates/flotilla-tui/src/app/mod.rs` | `App` struct, key/mouse dispatch, mode transitions |
| `crates/flotilla-tui/src/app/intent.rs` | `Intent` enum, resolves to `ProtoCommand` |
| `crates/flotilla-tui/src/app/executor.rs` | Thin executor: routes to core, interprets results into UI state |
| `crates/flotilla-tui/src/app/ui_state.rs` | `UiState`, `TabId`, `UiMode`, per-repo UI state |
| `crates/flotilla-tui/src/ui.rs` | All ratatui rendering |
| `crates/flotilla-tui/src/event.rs` | Terminal event stream (key, mouse, tick) |
| `crates/flotilla-tui/src/event_log.rs` | In-app tracing log with level filtering |

### Provider Traits

Each trait lives in `crates/flotilla-core/src/providers/<category>/mod.rs` with implementations alongside:

- **Vcs** + **CheckoutManager** (`vcs/git.rs`, `vcs/wt.rs`)
- **ChangeRequestTracker** (`change_request/github.rs`)
- **IssueTracker** (`issue_tracker/github.rs`)
- **CloudAgentService** (`coding_agent/claude.rs`)
- **AiUtility** (`ai_utility/claude.rs`)
- **WorkspaceManager** (`workspace/cmux.rs`)

Every provider trait has label methods: `section_label()`, `item_noun()`, `abbreviation()`, `display_name()`. Override defaults in implementations for custom terminology.

### Correlation

Union-find over `CorrelationKey` values (`Branch`, `CheckoutPath`, `ChangeRequestRef`, `SessionRef`). Items sharing any key merge into a single `WorkItem`. Issues link post-correlation via `AssociationKey` (don't cause merges). Tests in `crates/flotilla-core/src/providers/correlation.rs`.

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

`Normal` → `Help` → `Config` → `ActionMenu` → `BranchInput` → `FilePicker` → `DeleteConfirm`

Key bindings: `j/k` navigate, `Enter` execute, `Space` multi-select, `.` action menu, `d` delete, `p` open PR, `n` new branch, `r` refresh, `[/]` switch tabs, `{/}` reorder tabs, `q` quit, `?` help.

## Config

Stored in `~/.config/flotilla/`:
- `repos/*.toml` — one per tracked repo (`path = "..."`)
- `tab-order.json` — array of repo paths

Workspace templates: `.flotilla/workspace.yaml` in repo root.

## Issue Labels

Use these labels when creating or triaging issues. Combine as appropriate (e.g. `bug` + `ui`, or `from-review` + `refactor` + `quick-win`).

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
