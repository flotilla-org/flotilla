# CLI Output Formatting Infrastructure

**Issue:** [#281](https://github.com/rjwittams/flotilla/issues/281)

## Goal

Add a `--json` flag and shared output layer so every CLI command can render its data as human-friendly text (default) or structured JSON (for scripting and tests). Retrofit the existing `status` and `watch` subcommands with this pattern.

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Flag scope | Per-subcommand | Matches `gh`/`kubectl` conventions; each command owns its output contract |
| Output crate | `flotilla-protocol` | Protocol types already live here; avoids naming mismatch with `flotilla-tui` |
| Abstraction level | `OutputFormat` enum + per-command formatting | Human output differs enough per command that a shared trait adds abstraction without reducing code |

## Shared Infrastructure

New module: `crates/flotilla-protocol/src/output.rs`

```rust
/// Selects between human-readable and machine-readable output.
pub enum OutputFormat {
    Human,
    Json,
}

/// Serialize `data` as compact single-line JSON. Falls back to Debug on error.
pub fn json_line<T: Serialize + std::fmt::Debug>(data: &T) -> String { ... }

/// Serialize `data` as pretty-printed JSON. Falls back to Debug on error.
pub fn json_pretty<T: Serialize + std::fmt::Debug>(data: &T) -> String { ... }
```

Re-exported from `crates/flotilla-protocol/src/lib.rs` via `pub mod output`.

Each command handles its own human-friendly formatting inline. JSON formatting delegates to the helpers above.

## Clap Changes

`src/main.rs` — add `json: bool` to subcommand variants, convert to `OutputFormat` before calling handlers:

```rust
enum SubCommand {
    Daemon { timeout: u64 },
    Status { #[arg(long)] json: bool },
    Watch  { #[arg(long)] json: bool },
}
```

Handlers receive `format: OutputFormat`.

## `status` Command

### Human mode (default)

Table-like output — one block per repo. This is a new format replacing the existing ad-hoc output:

```
my-repo  /path/to/repo
  vcs/Git: ok  code_review/GitHub: ok

other-repo  /path/to/other  (loading)
  vcs/Git: ok  issue_tracker/GitHub: error
```

If no repos are tracked, print `No repos tracked.` and exit 0.

### JSON mode

Pretty-printed JSON wrapper object via `json_pretty`:

```json
{
  "repos": [ ... RepoInfo objects ... ]
}
```

The wrapper object allows adding top-level fields (e.g., daemon version) without breaking consumers.

## `watch` Command

**Breaking change:** The existing `watch` command outputs pretty-printed JSON by default. This changes the default to human-readable summaries. Use `--json` for machine-readable output.

### Human mode (default)

One-line event summaries. Repo names are derived from the last component of the `repo: PathBuf` field:

```
[snapshot] my-repo: full snapshot (seq 42, 5 work items)
[delta]    my-repo: delta seq 41→42 (3 changes)
[repo]     my-repo: added
[repo]     old-repo: removed
[command]  my-repo: started "refresh"
[command]  my-repo: finished "refresh" → ok
[peer]     host-2: connected
[peer]     host-2: disconnected
[peer]     host-2: connecting
[peer]     host-2: reconnecting
```

All four `PeerConnectionState` variants (`Connected`, `Disconnected`, `Connecting`, `Reconnecting`) are rendered lowercase.

On daemon disconnect, print `daemon disconnected` to stderr and exit 0.

### JSON mode

Compact JSONL — one `DaemonEvent` per line via `json_line`. Not pretty-printed.

On daemon disconnect, stop writing and exit 0.

### Both modes

On broken pipe, exit silently (exit 0) — standard behavior for piped CLI tools (`watch | head`). If the daemon is not running or the connection fails, print the error to stderr and exit non-zero (the default `color_eyre` behavior).

## File Changes

| File | Change |
|------|--------|
| `crates/flotilla-protocol/src/output.rs` | New: `OutputFormat`, `json_line`, `json_pretty` |
| `crates/flotilla-protocol/src/lib.rs` | Add `pub mod output` |
| `src/main.rs` | Add `json: bool` to `Status`/`Watch`, convert to `OutputFormat`, pass to handlers |
| `crates/flotilla-tui/src/cli.rs` | `run_status` and `run_watch` accept `format: OutputFormat`, branch on it |

## Testing

- Unit tests for `json_line` and `json_pretty` (valid serialization, fallback on error)
- Unit tests for human formatting of `status` output (construct `RepoInfo` values, assert formatted strings)
- Unit tests for human formatting of `watch` events (construct `DaemonEvent` variants, assert summary lines)

## Future

- A `flotilla-cli` crate may split out if CLI-specific logic grows beyond what belongs in `flotilla-protocol` or `flotilla-tui`.
- Additional output formats (e.g., `--format table|csv`) could extend `OutputFormat` later.
