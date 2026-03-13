# CLI Output Formatting Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `--json` flags to `status` and `watch` subcommands with human-friendly default output and structured JSON for scripting.

**Architecture:** `OutputFormat` enum and JSON helpers live in `flotilla-protocol/src/output.rs`. Each command in `flotilla-tui/src/cli.rs` branches on the format to produce either human text or JSON. The clap `--json` bool in `src/main.rs` converts to `OutputFormat` before calling handlers.

**Tech Stack:** Rust, clap (derive), serde_json, tokio

**Spec:** `docs/superpowers/specs/2026-03-13-cli-output-formatting-design.md`

---

## Chunk 1: Shared infrastructure and status command

### Task 1: Add `OutputFormat` enum and JSON helpers to `flotilla-protocol`

**Files:**
- Create: `crates/flotilla-protocol/src/output.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs:1-8`

- [ ] **Step 1: Write failing tests for `json_line` and `json_pretty`**

Create `crates/flotilla-protocol/src/output.rs` with tests only:

```rust
use std::fmt;

use serde::Serialize;

/// Selects between human-readable and machine-readable output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
}

/// Serialize `data` as compact single-line JSON. Falls back to Debug on error.
pub fn json_line<T: Serialize + fmt::Debug>(data: &T) -> String {
    todo!()
}

/// Serialize `data` as pretty-printed JSON. Falls back to Debug on error.
pub fn json_pretty<T: Serialize + fmt::Debug>(data: &T) -> String {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Serialize)]
    struct Sample {
        name: String,
        count: u32,
    }

    #[test]
    fn json_line_produces_compact_json() {
        let s = Sample { name: "test".into(), count: 42 };
        let result = json_line(&s);
        assert_eq!(result, r#"{"name":"test","count":42}"#);
        // Must be a single line
        assert!(!result.contains('\n'));
    }

    #[test]
    fn json_pretty_produces_indented_json() {
        let s = Sample { name: "test".into(), count: 42 };
        let result = json_pretty(&s);
        assert!(result.contains('\n'), "pretty JSON should contain newlines");
        assert!(result.contains("  \"name\""), "pretty JSON should be indented");
    }

    #[test]
    fn json_line_fallback_on_serialize_error() {
        // A type that fails to serialize but has Debug
        #[derive(Debug)]
        struct Bad;
        impl Serialize for Bad {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional"))
            }
        }
        let result = json_line(&Bad);
        assert_eq!(result, "Bad");
    }

    #[test]
    fn json_pretty_fallback_on_serialize_error() {
        #[derive(Debug)]
        struct Bad;
        impl Serialize for Bad {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional"))
            }
        }
        let result = json_pretty(&Bad);
        assert_eq!(result, "Bad");
    }

    #[test]
    fn from_json_flag_conversion() {
        assert_eq!(OutputFormat::from_json_flag(true), OutputFormat::Json);
        assert_eq!(OutputFormat::from_json_flag(false), OutputFormat::Human);
    }
}
```

- [ ] **Step 2: Wire up the module in `lib.rs`**

In `crates/flotilla-protocol/src/lib.rs`, add after line 6 (`pub mod snapshot;`):

```rust
pub mod output;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p flotilla-protocol output::tests -- --nocapture`

Expected: All five tests FAIL with `not yet implemented`.

- [ ] **Step 4: Implement `json_line` and `json_pretty`**

Replace the `todo!()` bodies in `crates/flotilla-protocol/src/output.rs`:

```rust
/// Serialize `data` as compact single-line JSON. Falls back to Debug on error.
pub fn json_line<T: Serialize + fmt::Debug>(data: &T) -> String {
    serde_json::to_string(data).unwrap_or_else(|_| format!("{data:?}"))
}

/// Serialize `data` as pretty-printed JSON. Falls back to Debug on error.
pub fn json_pretty<T: Serialize + fmt::Debug>(data: &T) -> String {
    serde_json::to_string_pretty(data).unwrap_or_else(|_| format!("{data:?}"))
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p flotilla-protocol output::tests -- --nocapture`

Expected: All five tests PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-protocol/src/output.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat: add OutputFormat enum and JSON helpers in flotilla-protocol"
```

---

### Task 2: Add `--json` flag to clap subcommands

**Files:**
- Modify: `src/main.rs:33-45` (SubCommand enum)
- Modify: `src/main.rs:62-67` (match arms)
- Modify: `src/main.rs:157-163` (run_status, run_watch wrappers)

- [ ] **Step 1: Add `json: bool` to `Status` and `Watch` subcommand variants**

In `src/main.rs`, change the `SubCommand` enum (lines 33-45):

```rust
#[derive(clap::Subcommand)]
enum SubCommand {
    /// Run the daemon server
    Daemon {
        /// Idle timeout in seconds (0 = no timeout)
        #[arg(long, default_value = "300")]
        timeout: u64,
    },
    /// Print repo list and state
    Status {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Stream daemon events to stdout
    Watch {
        /// Output as JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
}
```

- [ ] **Step 2: Update match arms and wrapper functions to pass `OutputFormat`**

In the root `Cargo.toml`, add `flotilla-protocol` to `[dependencies]`:

```toml
flotilla-protocol = { path = "crates/flotilla-protocol" }
```

In `src/main.rs`, add `use flotilla_protocol::output::OutputFormat;` to the imports.

Update the match in `main()` (lines 62-67):

```rust
match &cli.command {
    Some(SubCommand::Daemon { timeout }) => run_daemon(&cli, *timeout).await,
    Some(SubCommand::Status { json }) => run_status(&cli, OutputFormat::from_json_flag(*json)).await,
    Some(SubCommand::Watch { json }) => run_watch(&cli, OutputFormat::from_json_flag(*json)).await,
    None => run_tui(cli).await,
}
```

Update the wrapper functions (lines 157-163):

```rust
async fn run_status(cli: &Cli, format: OutputFormat) -> Result<()> {
    flotilla_tui::cli::run_status(&cli.socket_path(), format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}

async fn run_watch(cli: &Cli, format: OutputFormat) -> Result<()> {
    flotilla_tui::cli::run_watch(&cli.socket_path(), format).await.map_err(|e| color_eyre::eyre::eyre!(e))
}
```

- [ ] **Step 3: Add `from_json_flag` helper to `OutputFormat`**

In `crates/flotilla-protocol/src/output.rs`, add an impl block after the enum definition:

```rust
impl OutputFormat {
    pub fn from_json_flag(json: bool) -> Self {
        if json { Self::Json } else { Self::Human }
    }
}
```

- [ ] **Step 4: Update `cli.rs` signatures to accept the format parameter**

In `crates/flotilla-tui/src/cli.rs`, change imports and function signatures:

```rust
use std::path::Path;

use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::output::OutputFormat;

use crate::socket::SocketDaemon;

pub async fn run_status(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
```

And `run_watch` (line 37):

```rust
pub async fn run_watch(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
```

Add `let _ = format;` as the first line of each function body to suppress unused warnings temporarily.

- [ ] **Step 5: Verify it compiles and existing tests pass**

Run: `cargo build && cargo test --locked`

Expected: Compiles and all tests pass.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs crates/flotilla-protocol/src/output.rs crates/flotilla-tui/src/cli.rs
git commit -m "feat: add --json flag to status and watch subcommands"
```

---

### Task 3: Implement `status` command human and JSON output

**Files:**
- Modify: `crates/flotilla-tui/src/cli.rs:7-35` (run_status function)

- [ ] **Step 1: Write tests for status human formatting**

Add a `#[cfg(test)]` module at the bottom of `crates/flotilla-tui/src/cli.rs`:

```rust
#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    use flotilla_protocol::snapshot::{CategoryLabels, RepoInfo, RepoLabels};

    fn make_repo(name: &str, path: &str, loading: bool, health: HashMap<String, HashMap<String, bool>>) -> RepoInfo {
        RepoInfo {
            name: name.to_string(),
            path: PathBuf::from(path),
            labels: RepoLabels::default(),
            provider_names: HashMap::new(),
            provider_health: health,
            loading,
        }
    }

    fn health(entries: &[(&str, &str, bool)]) -> HashMap<String, HashMap<String, bool>> {
        let mut map: HashMap<String, HashMap<String, bool>> = HashMap::new();
        for (cat, name, ok) in entries {
            map.entry(cat.to_string()).or_default().insert(name.to_string(), *ok);
        }
        map
    }

    mod status_human {
        use super::*;
        use crate::cli::format_status_human;

        #[test]
        fn empty_repos() {
            assert_eq!(format_status_human(&[]), "No repos tracked.\n");
        }

        #[test]
        fn single_repo_healthy() {
            let repos = vec![make_repo("my-repo", "/tmp/my-repo", false, health(&[("vcs", "Git", true)]))];
            let output = format_status_human(&repos);
            assert!(output.contains("my-repo"), "should contain repo name");
            assert!(output.contains("/tmp/my-repo"), "should contain repo path");
            assert!(output.contains("vcs/Git: ok"), "should show health");
            assert!(!output.contains("loading"), "should not show loading");
        }

        #[test]
        fn repo_loading() {
            let repos = vec![make_repo("my-repo", "/tmp/my-repo", true, HashMap::new())];
            let output = format_status_human(&repos);
            assert!(output.contains("(loading)"), "should show loading indicator");
        }

        #[test]
        fn repo_with_error_health() {
            let repos = vec![make_repo("r", "/tmp/r", false, health(&[("code_review", "GitHub", false)]))];
            let output = format_status_human(&repos);
            assert!(output.contains("code_review/GitHub: error"), "should show error health");
        }
    }

    mod status_json {
        use super::*;
        use crate::cli::format_status_json;

        #[test]
        fn empty_repos_json() {
            let output = format_status_json(&[]);
            let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
            assert_eq!(parsed["repos"], serde_json::json!([]));
        }

        #[test]
        fn repos_wrapped_in_object() {
            let repos = vec![make_repo("my-repo", "/tmp/my-repo", false, HashMap::new())];
            let output = format_status_json(&repos);
            let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
            assert!(parsed["repos"].is_array(), "should have repos array");
            assert_eq!(parsed["repos"][0]["name"], "my-repo");
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui cli::tests -- --nocapture`

Expected: FAIL — `format_status_human` and `format_status_json` don't exist yet.

- [ ] **Step 3: Implement the formatting functions and update `run_status`**

In `crates/flotilla-tui/src/cli.rs`, add these imports at the top:

```rust
use std::fmt::Write;
```

Add the formatting functions (before the `run_status` function):

```rust
pub(crate) fn format_status_human(repos: &[flotilla_protocol::snapshot::RepoInfo]) -> String {
    if repos.is_empty() {
        return "No repos tracked.\n".to_string();
    }
    let mut out = String::new();
    for (i, repo) in repos.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let loading = if repo.loading { "  (loading)" } else { "" };
        writeln!(out, "{}  {}{}", repo.name, repo.path.display(), loading).expect("write to string");
        let health: Vec<String> = repo
            .provider_health
            .iter()
            .flat_map(|(category, providers)| {
                providers.iter().map(move |(name, v)| format!("{category}/{name}: {}", if *v { "ok" } else { "error" }))
            })
            .collect();
        if !health.is_empty() {
            writeln!(out, "  {}", health.join("  ")).expect("write to string");
        }
    }
    out
}

pub(crate) fn format_status_json(repos: &[flotilla_protocol::snapshot::RepoInfo]) -> String {
    #[derive(serde::Serialize)]
    struct StatusResponse<'a> {
        repos: &'a [flotilla_protocol::snapshot::RepoInfo],
    }
    flotilla_protocol::output::json_pretty(&StatusResponse { repos })
}
```

Update `run_status` to branch on format:

```rust
pub async fn run_status(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path).await.map_err(|e| format!("cannot connect to daemon: {e}"))?;
    let repos = daemon.list_repos().await.map_err(|e| e.to_string())?;

    let output = match format {
        OutputFormat::Human => format_status_human(&repos),
        OutputFormat::Json => format_status_json(&repos),
    };
    print!("{output}");
    Ok(())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui cli::tests -- --nocapture`

Expected: All status tests PASS.

- [ ] **Step 5: Run full test suite**

Run: `cargo test --locked`

Expected: All tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/cli.rs
git commit -m "feat: implement status command human and JSON output"
```

---

## Chunk 2: Watch command formatting

### Task 4: Implement `watch` command human formatting

**Files:**
- Modify: `crates/flotilla-tui/src/cli.rs` (add `format_event_human`, update `run_watch`)

- [ ] **Step 1: Write tests for human event formatting**

Add to the `tests` module in `crates/flotilla-tui/src/cli.rs`:

```rust
mod watch_human {
    use std::path::PathBuf;

    use flotilla_protocol::{
        commands::CommandResult, snapshot::Snapshot, DaemonEvent, HostName, PeerConnectionState, SnapshotDelta,
    };

    use crate::cli::format_event_human;

    fn dummy_snapshot(seq: u64, repo: &str, work_item_count: usize) -> Snapshot {
        use flotilla_protocol::snapshot::{WorkItem, WorkItemIdentity, WorkItemKind};
        use std::collections::HashMap;

        Snapshot {
            seq,
            repo: PathBuf::from(repo),
            host_name: HostName::new("test"),
            work_items: (0..work_item_count)
                .map(|i| WorkItem {
                    kind: WorkItemKind::Checkout,
                    identity: WorkItemIdentity::Checkout(flotilla_protocol::HostPath::new(
                        HostName::new("test"),
                        PathBuf::from(format!("/tmp/wt{i}")),
                    )),
                    host: HostName::new("test"),
                    branch: None,
                    description: String::new(),
                    checkout: None,
                    change_request_key: None,
                    session_key: None,
                    issue_keys: vec![],
                    workspace_refs: vec![],
                    is_main_checkout: false,
                    debug_group: vec![],
                    source: None,
                    terminal_keys: vec![],
                })
                .collect(),
            providers: Default::default(),
            provider_health: HashMap::new(),
            errors: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        }
    }

    #[test]
    fn snapshot_full() {
        let event = DaemonEvent::SnapshotFull(Box::new(dummy_snapshot(42, "/tmp/my-repo", 5)));
        let line = format_event_human(&event);
        assert!(line.contains("[snapshot]"), "should have snapshot tag");
        assert!(line.contains("my-repo"), "should extract repo name from path");
        assert!(line.contains("seq 42"), "should show seq");
        assert!(line.contains("5 work items"), "should show work item count");
    }

    #[test]
    fn snapshot_delta() {
        let event = DaemonEvent::SnapshotDelta(Box::new(SnapshotDelta {
            seq: 42,
            prev_seq: 41,
            repo: PathBuf::from("/tmp/my-repo"),
            changes: vec![],
            work_items: vec![],
            issue_total: None,
            issue_has_more: false,
            issue_search_results: None,
        }));
        let line = format_event_human(&event);
        assert!(line.contains("[delta]"), "should have delta tag");
        assert!(line.contains("41→42") || line.contains("41->42"), "should show prev→seq");
    }

    #[test]
    fn repo_added() {
        let event = DaemonEvent::RepoAdded(Box::new(flotilla_protocol::snapshot::RepoInfo {
            name: "added-repo".into(),
            path: PathBuf::from("/tmp/added-repo"),
            labels: Default::default(),
            provider_names: Default::default(),
            provider_health: Default::default(),
            loading: false,
        }));
        let line = format_event_human(&event);
        assert!(line.contains("[repo]"), "should have repo tag");
        assert!(line.contains("added-repo"), "should show repo name");
        assert!(line.contains("added"), "should say added");
    }

    #[test]
    fn repo_removed() {
        let event = DaemonEvent::RepoRemoved { path: PathBuf::from("/tmp/old-repo") };
        let line = format_event_human(&event);
        assert!(line.contains("[repo]"), "should have repo tag");
        assert!(line.contains("old-repo"), "should extract name");
        assert!(line.contains("removed"), "should say removed");
    }

    #[test]
    fn command_started() {
        let event = DaemonEvent::CommandStarted {
            command_id: 1,
            repo: PathBuf::from("/tmp/my-repo"),
            description: "Refreshing...".into(),
        };
        let line = format_event_human(&event);
        assert!(line.contains("[command]"), "should have command tag");
        assert!(line.contains("started"), "should say started");
        assert!(line.contains("Refreshing..."), "should include description");
    }

    #[test]
    fn command_finished_ok() {
        let event = DaemonEvent::CommandFinished {
            command_id: 1,
            repo: PathBuf::from("/tmp/my-repo"),
            result: CommandResult::Ok,
        };
        let line = format_event_human(&event);
        assert!(line.contains("[command]"), "should have command tag");
        assert!(line.contains("finished"), "should say finished");
        assert!(line.contains("ok"), "should show ok result");
    }

    #[test]
    fn command_finished_error() {
        let event = DaemonEvent::CommandFinished {
            command_id: 1,
            repo: PathBuf::from("/tmp/my-repo"),
            result: CommandResult::Error { message: "boom".into() },
        };
        let line = format_event_human(&event);
        assert!(line.contains("error: boom"), "should show error message");
    }

    #[test]
    fn peer_all_states() {
        for (state, expected) in [
            (PeerConnectionState::Connected, "connected"),
            (PeerConnectionState::Disconnected, "disconnected"),
            (PeerConnectionState::Connecting, "connecting"),
            (PeerConnectionState::Reconnecting, "reconnecting"),
        ] {
            let event = DaemonEvent::PeerStatusChanged { host: HostName::new("host-2"), status: state };
            let line = format_event_human(&event);
            assert!(line.contains("[peer]"), "should have peer tag for {expected}");
            assert!(line.contains("host-2"), "should show host name for {expected}");
            assert!(line.contains(expected), "should contain '{expected}'");
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p flotilla-tui cli::tests::watch_human -- --nocapture`

Expected: FAIL — `format_event_human` doesn't exist.

- [ ] **Step 3: Implement `format_event_human`**

Add to `crates/flotilla-tui/src/cli.rs` (after the status formatting functions).

**Note:** The spec example shows the command description in `CommandFinished` output (`finished "refresh" → ok`), but `DaemonEvent::CommandFinished` doesn't carry the `description` field — only `command_id`, `repo`, and `result`. We format what's available.

```rust
/// Extract a short display name from a repo path (last path component).
fn repo_name(path: &std::path::Path) -> &str {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("unknown")
}

/// Format a `CommandResult` as a short human-readable string.
fn format_command_result(result: &flotilla_protocol::commands::CommandResult) -> String {
    use flotilla_protocol::commands::CommandResult;
    match result {
        CommandResult::Ok => "ok".to_string(),
        CommandResult::CheckoutCreated { branch } => format!("checkout created: {branch}"),
        CommandResult::BranchNameGenerated { name, .. } => format!("branch name: {name}"),
        CommandResult::CheckoutStatus(_) => "checkout status received".to_string(),
        CommandResult::Error { message } => format!("error: {message}"),
    }
}

pub(crate) fn format_event_human(event: &flotilla_protocol::DaemonEvent) -> String {
    use flotilla_protocol::{DaemonEvent, PeerConnectionState};
    match event {
        DaemonEvent::SnapshotFull(snap) => {
            format!(
                "[snapshot] {}: full snapshot (seq {}, {} work items)",
                repo_name(&snap.repo),
                snap.seq,
                snap.work_items.len()
            )
        }
        DaemonEvent::SnapshotDelta(delta) => {
            format!(
                "[delta]    {}: delta seq {}→{} ({} changes)",
                repo_name(&delta.repo),
                delta.prev_seq,
                delta.seq,
                delta.changes.len()
            )
        }
        DaemonEvent::RepoAdded(info) => {
            format!("[repo]     {}: added", info.name)
        }
        DaemonEvent::RepoRemoved { path } => {
            format!("[repo]     {}: removed", repo_name(path))
        }
        DaemonEvent::CommandStarted { repo, description, .. } => {
            format!("[command]  {}: started \"{}\"", repo_name(repo), description)
        }
        DaemonEvent::CommandFinished { repo, result, .. } => {
            format!("[command]  {}: finished → {}", repo_name(repo), format_command_result(result))
        }
        DaemonEvent::PeerStatusChanged { host, status } => {
            let state = match status {
                PeerConnectionState::Connected => "connected",
                PeerConnectionState::Disconnected => "disconnected",
                PeerConnectionState::Connecting => "connecting",
                PeerConnectionState::Reconnecting => "reconnecting",
            };
            format!("[peer]     {host}: {state}")
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p flotilla-tui cli::tests::watch_human -- --nocapture`

Expected: All watch human tests PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-tui/src/cli.rs
git commit -m "feat: add human-readable event formatting for watch command"
```

---

### Task 5: Wire up `run_watch` with format branching

**Files:**
- Modify: `crates/flotilla-tui/src/cli.rs` (`run_watch` function)

- [ ] **Step 1: Update `run_watch` to branch on format**

Replace the `run_watch` function body in `crates/flotilla-tui/src/cli.rs`:

```rust
pub async fn run_watch(socket_path: &Path, format: OutputFormat) -> Result<(), String> {
    let daemon = SocketDaemon::connect(socket_path).await.map_err(|e| format!("cannot connect to daemon: {e}"))?;

    let mut rx = daemon.subscribe();

    if matches!(format, OutputFormat::Human) {
        eprintln!("watching events (Ctrl-C to stop)...");
    }

    loop {
        match rx.recv().await {
            Ok(event) => {
                let line = match format {
                    OutputFormat::Human => format_event_human(&event),
                    OutputFormat::Json => flotilla_protocol::output::json_line(&event),
                };
                println!("{line}");
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                eprintln!("warning: skipped {n} events");
            }
            Err(_) => {
                eprintln!("daemon disconnected");
                break;
            }
        }
    }

    Ok(())
}
```

Key changes from existing code:
- Human mode prints one-line event summaries instead of pretty JSON.
- JSON mode uses `json_line` (compact, single line) instead of `to_string_pretty`.
- The "watching events" banner goes to stderr and only appears in human mode.

- [ ] **Step 2: Verify it compiles and all tests pass**

Run: `cargo build && cargo test --locked`

Expected: Compiles and all tests pass.

- [ ] **Step 3: Run clippy and format**

Run: `cargo +nightly fmt && cargo clippy --all-targets --locked -- -D warnings`

Expected: No warnings or errors.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-tui/src/cli.rs
git commit -m "feat: implement watch command human and JSON output modes"
```

---

### Task 6: Handle broken pipe (SIGPIPE)

**Files:**
- Modify: `src/main.rs` (add SIGPIPE reset at program start)

Rust overrides the default SIGPIPE handler, so `println!` panics when piped to a command that closes early (e.g., `flotilla watch | head -1`). CLI tools should exit silently on broken pipe.

- [ ] **Step 1: Add SIGPIPE reset at the top of `main()`**

In `src/main.rs`, add this before `color_eyre::install()`:

```rust
// Reset SIGPIPE to default behavior so piped CLI commands (e.g. `watch | head`)
// exit cleanly instead of panicking on broken pipe.
#[cfg(unix)]
{
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}
```

- [ ] **Step 2: Add `libc` dependency to the root crate**

In the root `Cargo.toml`, add under `[dependencies]`:

```toml
libc = "0.2"
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build`

Expected: Compiles without errors.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs Cargo.toml Cargo.lock
git commit -m "fix: reset SIGPIPE for clean broken pipe behavior in CLI"
```

---

### Task 7: Final verification

- [ ] **Step 1: Run the full test suite**

Run: `cargo test --locked`

Expected: All tests pass.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --all-targets --locked -- -D warnings`

Expected: No warnings.

- [ ] **Step 3: Run formatter**

Run: `cargo +nightly fmt`

Expected: No changes (already formatted).

- [ ] **Step 4: Verify `--json` flag appears in help**

Run: `cargo run -- status --help`

Expected output includes:
```
  --json  Output as JSON instead of human-readable text
```

Run: `cargo run -- watch --help`

Expected output includes:
```
  --json  Output as JSON instead of human-readable text
```
