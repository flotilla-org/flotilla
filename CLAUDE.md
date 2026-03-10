# Flotilla

TUI dashboard for managing development workspaces across git worktrees, code review (GitHub PRs), issue trackers, cloud agent sessions (Claude), and terminal multiplexers (cmux).

## Quick Reference

```bash
cargo build                          # build
cargo test                           # all tests
cargo clippy                         # lint
cargo run -- --repo-root /path       # run with explicit repo
cargo run                            # run, auto-detect repo from cwd
```

Before pushing, always run `cargo fmt`, `cargo clippy --all-targets --locked -- -D warnings`, and `cargo test --locked`.

In the Codex sandbox, prefer `mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests` so native dependencies can create temp files and socket-bind tests stay skipped.

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
- **CodeReview** (`code_review/github.rs`)
- **IssueTracker** (`issue_tracker/github.rs`)
- **CodingAgent** (`coding_agent/claude.rs`)
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
- **Imports**: std first, external crates, then `use crate::...`.
- **Adding dependencies is fine** when they solve a real problem — don't reinvent the wheel.
- **Correctness first**: Always favour correct solutions over "pragmatic" shortcuts. Get the architecture right rather than patching around structural problems.

## UI Modes

`Normal` → `Help` → `Config` → `ActionMenu` → `BranchInput` → `FilePicker` → `DeleteConfirm`

Key bindings: `j/k` navigate, `Enter` execute, `Space` multi-select, `.` action menu, `d` delete, `p` open PR, `n` new branch, `r` refresh, `[/]` switch tabs, `{/}` reorder tabs, `q` quit, `?` help.

## Config

Stored in `~/.config/flotilla/`:
- `repos/*.toml` — one per tracked repo (`path = "..."`)
- `tab-order.json` — array of repo paths

Workspace templates: `.flotilla/workspace.yaml` in repo root.
