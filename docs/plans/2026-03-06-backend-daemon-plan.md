# Backend Daemon Architecture Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Extract flotilla's core into a daemon behind a `DaemonHandle` trait, with the TUI as a client — Step 1 of the design (in-process only, compile-time boundary enforcement).

**Architecture:** Convert single crate to Cargo workspace with 4 library crates (`flotilla-core`, `flotilla-protocol`, `flotilla-daemon`, `flotilla-tui`) and one aggregator binary. Define `DaemonHandle` trait as the boundary. Implement `InProcessDaemon` so behavior is identical to today. Socket transport (Step 2) comes later.

**Tech Stack:** Rust, Cargo workspaces, tokio, async-trait, serde, ratatui

---

## Scope Note

This plan covers **Step 1 only** from the design doc: workspace restructure, trait definition, in-process implementation, TUI as client. The socket server (Step 2), delta snapshots (Step 3), and multi-host (Step 4) are separate future plans.

The executor currently takes `&mut App` — it reads `app.model` for provider registries and writes `app.ui.mode` for UI feedback (e.g. `DeleteConfirm`, `BranchInput`). The key challenge is splitting this: the daemon executes commands against providers and returns results; the TUI interprets results into UI state changes.

---

### Task 1: Create Cargo workspace structure

**Files:**
- Move: `Cargo.toml` → workspace root
- Create: `crates/flotilla-core/Cargo.toml`
- Create: `crates/flotilla-core/src/lib.rs`
- Create: `crates/flotilla-protocol/Cargo.toml`
- Create: `crates/flotilla-protocol/src/lib.rs`
- Create: `crates/flotilla-daemon/Cargo.toml`
- Create: `crates/flotilla-daemon/src/lib.rs`
- Create: `crates/flotilla-tui/Cargo.toml`
- Create: `crates/flotilla-tui/src/lib.rs`
- Modify: `src/main.rs` (aggregator binary)

**Step 1: Create workspace Cargo.toml**

Replace the root `Cargo.toml` with a workspace definition. The aggregator binary stays at the root.

```toml
[workspace]
members = [
    "crates/flotilla-core",
    "crates/flotilla-protocol",
    "crates/flotilla-daemon",
    "crates/flotilla-tui",
]
resolver = "2"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
async-trait = "0.1"
tracing = "0.1.44"
indexmap = { version = "2", features = ["serde"] }
color-eyre = "0.6"

[package]
name = "flotilla"
version = "0.1.0"
edition = "2021"

[dependencies]
flotilla-core = { path = "crates/flotilla-core" }
flotilla-protocol = { path = "crates/flotilla-protocol" }
flotilla-daemon = { path = "crates/flotilla-daemon" }
flotilla-tui = { path = "crates/flotilla-tui" }
tokio = { workspace = true }
clap = { version = "4", features = ["derive"] }
color-eyre = { workspace = true }
tracing = { workspace = true }
```

**Step 2: Create flotilla-protocol crate**

```toml
# crates/flotilla-protocol/Cargo.toml
[package]
name = "flotilla-protocol"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { workspace = true }
serde_json = { workspace = true }
```

```rust
// crates/flotilla-protocol/src/lib.rs
// Placeholder — protocol types will be added in Task 4
```

**Step 3: Create flotilla-core crate**

```toml
# crates/flotilla-core/Cargo.toml
[package]
name = "flotilla-core"
version = "0.1.0"
edition = "2021"

[dependencies]
flotilla-protocol = { path = "../flotilla-protocol" }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
async-trait = { workspace = true }
tracing = { workspace = true }
indexmap = { workspace = true }
color-eyre = { workspace = true }
dirs = "6"
toml = "0.8"
reqwest = { version = "0.13.2", features = ["json"] }
```

```rust
// crates/flotilla-core/src/lib.rs
// Placeholder — modules moved in Task 2
```

**Step 4: Create flotilla-daemon crate**

```toml
# crates/flotilla-daemon/Cargo.toml
[package]
name = "flotilla-daemon"
version = "0.1.0"
edition = "2021"

[dependencies]
flotilla-core = { path = "../flotilla-core" }
flotilla-protocol = { path = "../flotilla-protocol" }
tokio = { workspace = true }
```

```rust
// crates/flotilla-daemon/src/lib.rs
// Placeholder — socket server comes in Step 2 of the design
```

**Step 5: Create flotilla-tui crate**

```toml
# crates/flotilla-tui/Cargo.toml
[package]
name = "flotilla-tui"
version = "0.1.0"
edition = "2021"

[dependencies]
flotilla-core = { path = "../flotilla-core" }
flotilla-protocol = { path = "../flotilla-protocol" }
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tracing = { workspace = true }
color-eyre = { workspace = true }
ratatui = "0.30"
crossterm = { version = "0.28", features = ["event-stream"] }
tui-input = "0.10"
ratatui-image = { version = "10.0.6", default-features = false, features = ["crossterm", "image-defaults"] }
image = "0.25.9"
unicode-width = "0.2.2"
tracing-subscriber = "0.3.22"
tracing-appender = "0.2.4"
time = { version = "0.3.47", features = ["local-offset"] }
```

```rust
// crates/flotilla-tui/src/lib.rs
// Placeholder — modules moved in Task 3
```

**Step 6: Verify workspace compiles**

Run: `cargo check 2>&1 | head -5`
Expected: Compiles (existing src/main.rs still works, new crates are empty placeholders)

**Step 7: Commit**

```bash
git add -A
git commit -m "chore: create cargo workspace with 4 crates"
```

---

### Task 2: Move core modules into flotilla-core

Move provider, data, config, and refresh code into `flotilla-core`. The goal is to get `flotilla-core` compiling with all the domain logic. The aggregator binary's `src/lib.rs` re-exports from `flotilla-core` temporarily so nothing else breaks.

**Files:**
- Move: `src/providers/` → `crates/flotilla-core/src/providers/`
- Move: `src/provider_data.rs` → `crates/flotilla-core/src/provider_data.rs`
- Move: `src/data.rs` → `crates/flotilla-core/src/data.rs`
- Move: `src/refresh.rs` → `crates/flotilla-core/src/refresh.rs`
- Move: `src/config.rs` → `crates/flotilla-core/src/config.rs`
- Move: `src/app/command.rs` → `crates/flotilla-core/src/command.rs`
- Move: `src/app/executor.rs` → `crates/flotilla-core/src/executor.rs` (temporarily, will be refactored)
- Move: `src/app/model.rs` → `crates/flotilla-core/src/model.rs`
- Modify: `crates/flotilla-core/src/lib.rs`
- Modify: `src/lib.rs` (re-export from flotilla-core)

**Step 1: Move files**

```bash
# Create directory structure
mkdir -p crates/flotilla-core/src

# Move modules
cp -r src/providers crates/flotilla-core/src/
cp src/provider_data.rs crates/flotilla-core/src/
cp src/data.rs crates/flotilla-core/src/
cp src/refresh.rs crates/flotilla-core/src/
cp src/config.rs crates/flotilla-core/src/
cp src/app/command.rs crates/flotilla-core/src/command.rs
cp src/app/model.rs crates/flotilla-core/src/model.rs
```

**Step 2: Write flotilla-core/src/lib.rs**

```rust
pub mod command;
pub mod config;
pub mod data;
pub mod model;
pub mod provider_data;
pub mod providers;
pub mod refresh;
```

**Step 3: Fix `crate::` references in moved files**

All moved files use `crate::` paths that now refer to `flotilla_core`. These should just work since they're now in the `flotilla-core` crate. But some files reference `crate::providers::run_cmd` with `pub(crate)` visibility — change these to `pub` so they're accessible within the crate. Specifically in `crates/flotilla-core/src/providers/mod.rs`:

- Change `pub(crate) async fn run_cmd` → `pub async fn run_cmd`
- Change `pub(crate) fn command_exists` → `pub fn command_exists`
- Change `pub(crate) fn resolve_claude_path` → `pub fn resolve_claude_path`

**Step 4: Handle the executor**

The executor currently depends on `App` (which includes UI state). For now, copy `src/app/executor.rs` to `crates/flotilla-core/src/executor.rs` but **do not wire it into the lib.rs yet** — it will be refactored in Task 5 when we define `CommandResult` and split UI concerns out. Just move it so it's in the right place.

**Step 5: Remove model.rs dependency on UI types**

`model.rs` (now in flotilla-core) currently lives at `src/app/model.rs` and imports from `crate::data`, `crate::providers`, `crate::refresh` — these all exist in flotilla-core now, so the paths work. Verify there are no imports from `app::ui_state` or other UI modules. Looking at the current code, `model.rs` is clean — it only imports `DataStore`, `ProviderRegistry`, `discovery`, `RepoCriteria`, and `RepoRefreshHandle`.

**Step 6: Update root src/lib.rs to re-export**

Replace `src/lib.rs` with re-exports so existing `src/main.rs` and `src/app/` code can still use `crate::data`, `crate::config`, etc:

```rust
pub use flotilla_core::config;
pub use flotilla_core::data;
pub use flotilla_core::provider_data;
pub use flotilla_core::providers;
pub use flotilla_core::refresh;
pub use flotilla_core::command;
pub use flotilla_core::model;
```

Also keep the modules that stay in the root crate (TUI-side, moved in Task 3):

```rust
pub mod event_log;
pub mod template;
```

**Step 7: Fix src/app/ imports**

Update `src/app/mod.rs` to import from `crate::` (which now re-exports from flotilla-core):
- `use crate::data::{TableEntry, WorkItem}` — still works via re-export
- `use crate::command::{Command, CommandQueue}` — update from `super::command`
- `use crate::model::{AppModel, ProviderStatus}` — update from `super::model`

Remove `pub mod command;` and `pub mod model;` from `src/app/mod.rs` since they've moved.

Update `src/app/executor.rs` — this file stays in `src/app/` for now (the copy in flotilla-core is dormant). Update its imports to use `crate::` re-exports.

Update `src/app/intent.rs` — uses `crate::data` (works via re-export) and `super::command::Command` → `crate::command::Command`.

Update `src/app/ui_state.rs` — uses `crate::data` (works via re-export).

**Step 8: Verify it compiles**

Run: `cargo check`
Expected: Compiles. All `crate::` paths resolve through re-exports.

**Step 9: Run tests**

Run: `cargo test`
Expected: All 37 tests pass. Tests in `data.rs` and `providers/` now run under `flotilla-core`.

**Step 10: Commit**

```bash
git add -A
git commit -m "refactor: move core modules into flotilla-core crate"
```

---

### Task 3: Move TUI modules into flotilla-tui

Move rendering, input handling, and UI state into `flotilla-tui`. After this, the root binary just wires things together.

**Files:**
- Move: `src/ui.rs` → `crates/flotilla-tui/src/ui.rs`
- Move: `src/app/mod.rs` → `crates/flotilla-tui/src/app/mod.rs`
- Move: `src/app/ui_state.rs` → `crates/flotilla-tui/src/app/ui_state.rs`
- Move: `src/app/intent.rs` → `crates/flotilla-tui/src/app/intent.rs`
- Move: `src/app/executor.rs` → `crates/flotilla-tui/src/app/executor.rs`
- Move: `src/event.rs` → `crates/flotilla-tui/src/event.rs`
- Move: `src/event_log.rs` → `crates/flotilla-tui/src/event_log.rs`
- Move: `src/template.rs` → `crates/flotilla-tui/src/template.rs`
- Move: `assets/` → `crates/flotilla-tui/assets/`
- Modify: `crates/flotilla-tui/src/lib.rs`
- Modify: `src/main.rs` (thin aggregator)
- Delete: `src/lib.rs`, `src/app/`

**Step 1: Move files**

```bash
mkdir -p crates/flotilla-tui/src/app
cp src/ui.rs crates/flotilla-tui/src/
cp src/app/mod.rs crates/flotilla-tui/src/app/
cp src/app/ui_state.rs crates/flotilla-tui/src/app/
cp src/app/intent.rs crates/flotilla-tui/src/app/
cp src/app/executor.rs crates/flotilla-tui/src/app/
cp src/event.rs crates/flotilla-tui/src/
cp src/event_log.rs crates/flotilla-tui/src/
cp src/template.rs crates/flotilla-tui/src/
cp -r assets crates/flotilla-tui/
```

**Step 2: Write flotilla-tui/src/lib.rs**

```rust
pub mod app;
pub mod event;
pub mod event_log;
pub mod template;
pub mod ui;
```

**Step 3: Fix imports in moved TUI files**

All `crate::` references in TUI files that point to core modules need to become `flotilla_core::`:

In `app/mod.rs`:
- `use crate::data::{TableEntry, WorkItem}` → `use flotilla_core::data::{TableEntry, WorkItem}`
- `use crate::command::...` → `use flotilla_core::command::...`
- `use crate::model::...` → `use flotilla_core::model::...`
- `use crate::config` → `use flotilla_core::config`

In `app/executor.rs`:
- `use crate::data` → `use flotilla_core::data`
- `use crate::config` → `use flotilla_core::config`
- `use crate::providers` → `use flotilla_core::providers`
- `use super::command::Command` → `use flotilla_core::command::Command`

In `app/intent.rs`:
- `use crate::data::{WorkItem, WorkItemKind}` → `use flotilla_core::data::{WorkItem, WorkItemKind}`
- `use super::command::Command` → `use flotilla_core::command::Command`

In `app/ui_state.rs`:
- `use crate::data::{DeleteConfirmInfo, TableView, WorkItemIdentity}` → `use flotilla_core::data::{DeleteConfirmInfo, TableView, WorkItemIdentity}`

In `ui.rs`:
- All `crate::` refs to data/providers/model → `flotilla_core::`
- `crate::event_log` → `crate::event_log` (stays, it's in flotilla-tui)

In `template.rs`:
- Uses `crate::providers::types` → `flotilla_core::providers::types`

**Step 4: Fix assets path**

In `crates/flotilla-tui/src/ui.rs` or wherever `include_bytes!("../assets/splash.png")` appears — this is actually in `src/main.rs`. The splash screen logic will need the assets path updated. Move splash display into `flotilla-tui` as a public function, or keep it in `main.rs` with the correct relative path.

Simplest: keep splash in `main.rs` with `include_bytes!("../crates/flotilla-tui/assets/splash.png")`, or move the splash function into flotilla-tui and export it.

**Step 5: Update root src/main.rs**

Replace the root `main.rs` to be a thin entry point that delegates to flotilla-tui:

```rust
use std::io::stdout;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use clap::Parser;
use color_eyre::Result;
use crossterm::{execute, event::{EnableMouseCapture, DisableMouseCapture}};
use tracing::info;

use flotilla_tui::event_log::LevelExt;
use flotilla_tui::app;
use flotilla_tui::event;
use flotilla_tui::ui;
use flotilla_core::{config, data, providers};
```

Then the `run()`, `drain_snapshots()`, `resolve_repo_roots()`, and `show_splash()` functions work with imports from the two crates.

**Step 6: Delete old source files**

```bash
rm src/lib.rs
rm -r src/app/
rm src/ui.rs src/event.rs src/event_log.rs src/template.rs
rm src/data.rs src/config.rs src/provider_data.rs src/refresh.rs
rm -r src/providers/
```

Only `src/main.rs` remains in the root crate.

**Step 7: Verify it compiles**

Run: `cargo check`
Expected: Compiles with imports split across flotilla-core and flotilla-tui.

**Step 8: Run tests**

Run: `cargo test --workspace`
Expected: All 37 tests pass (they live in flotilla-core now).

**Step 9: Commit**

```bash
git add -A
git commit -m "refactor: move TUI modules into flotilla-tui crate"
```

---

### Task 4: Define protocol types in flotilla-protocol

Define the serializable types that form the contract between daemon and client. These are serde-derived mirrors of core types, not the core types themselves. This enforces that nothing non-serializable crosses the boundary.

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`
- Create: `crates/flotilla-protocol/src/snapshot.rs`
- Create: `crates/flotilla-protocol/src/commands.rs`
- Test: `crates/flotilla-protocol/src/lib.rs` (serde round-trip tests)

**Step 1: Write protocol snapshot types**

```rust
// crates/flotilla-protocol/src/snapshot.rs
use std::collections::HashMap;
use std::path::PathBuf;
use serde::{Serialize, Deserialize};

/// Repo info for list_repos response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoInfo {
    pub path: PathBuf,
    pub name: String,
    pub provider_health: HashMap<String, bool>,
    pub loading: bool,
}

/// A complete snapshot for one repo — sent on subscribe and on each refresh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub repo: PathBuf,
    pub work_items: Vec<ProtoWorkItem>,
    pub provider_health: HashMap<String, bool>,
    pub errors: Vec<ProtoError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoError {
    pub category: String,
    pub message: String,
}

/// Serializable work item — flattened from the core WorkItem enum.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoWorkItem {
    pub kind: ProtoWorkItemKind,
    pub identity: ProtoWorkItemIdentity,
    pub branch: Option<String>,
    pub description: String,
    pub checkout: Option<ProtoCheckoutRef>,
    pub pr_key: Option<String>,
    pub session_key: Option<String>,
    pub issue_keys: Vec<String>,
    pub workspace_refs: Vec<String>,
    pub is_main_worktree: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ProtoWorkItemKind {
    Checkout,
    Session,
    Pr,
    RemoteBranch,
    Issue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ProtoWorkItemIdentity {
    Checkout(PathBuf),
    ChangeRequest(String),
    Session(String),
    Issue(String),
    RemoteBranch(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoCheckoutRef {
    pub key: PathBuf,
    pub is_main_worktree: bool,
}
```

**Step 2: Write protocol command types**

```rust
// crates/flotilla-protocol/src/commands.rs
use std::path::PathBuf;
use serde::{Serialize, Deserialize};

/// Commands the client can send to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProtoCommand {
    SwitchWorktree { path: PathBuf },
    SelectWorkspace { ws_ref: String },
    CreateWorktree { branch: String, create_branch: bool, issue_ids: Vec<(String, String)> },
    RemoveCheckout { branch: String },
    OpenPr { id: String },
    OpenIssueBrowser { id: String },
    LinkIssuesToPr { pr_id: String, issue_ids: Vec<String> },
    ArchiveSession { session_id: String },
    GenerateBranchName { issue_keys: Vec<String> },
    TeleportSession { session_id: String, branch: Option<String>, checkout_key: Option<PathBuf> },
    AddRepo { path: PathBuf },
    RemoveRepo { path: PathBuf },
    Refresh,
}

/// Result returned from command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandResult {
    Ok,
    /// Worktree created — provides the branch name for UI feedback.
    WorktreeCreated { branch: String },
    /// Branch name generated by AI.
    BranchNameGenerated { name: String, issue_ids: Vec<(String, String)> },
    /// Delete confirmation info fetched.
    DeleteInfo(ProtoDeleteInfo),
    /// Error executing the command.
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtoDeleteInfo {
    pub branch: String,
    pub pr_status: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub unpushed_commits: Vec<String>,
    pub has_uncommitted: bool,
}
```

**Step 3: Write protocol envelope types**

```rust
// crates/flotilla-protocol/src/lib.rs
pub mod commands;
pub mod snapshot;

use serde::{Serialize, Deserialize};

pub use commands::{ProtoCommand, CommandResult, ProtoDeleteInfo};
pub use snapshot::{Snapshot, ProtoWorkItem, ProtoWorkItemKind, ProtoWorkItemIdentity, ProtoCheckoutRef, ProtoError, RepoInfo};

/// Top-level message envelope for the JSON protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "request")]
    Request {
        id: u64,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },

    #[serde(rename = "response")]
    Response {
        id: u64,
        result: CommandResult,
    },

    #[serde(rename = "event")]
    Event {
        event: DaemonEvent,
    },
}

/// Events pushed from daemon to subscribed clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum DaemonEvent {
    #[serde(rename = "snapshot")]
    Snapshot(Snapshot),

    #[serde(rename = "repo_added")]
    RepoAdded(RepoInfo),

    #[serde(rename = "repo_removed")]
    RepoRemoved { path: std::path::PathBuf },

    #[serde(rename = "command_result")]
    CommandResult {
        repo: std::path::PathBuf,
        result: CommandResult,
    },
}
```

**Step 4: Write serde round-trip tests**

```rust
// Add to crates/flotilla-protocol/src/lib.rs
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn message_request_roundtrip() {
        let msg = Message::Request {
            id: 1,
            method: "subscribe".into(),
            params: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Message::Request { id: 1, .. }));
    }

    #[test]
    fn message_event_snapshot_roundtrip() {
        let snapshot = Snapshot {
            seq: 42,
            repo: PathBuf::from("/tmp/repo"),
            work_items: vec![],
            provider_health: Default::default(),
            errors: vec![],
        };
        let msg = Message::Event {
            event: DaemonEvent::Snapshot(snapshot),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: Message = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, Message::Event { event: DaemonEvent::Snapshot(s) } if s.seq == 42));
    }

    #[test]
    fn command_result_variants_roundtrip() {
        let results = vec![
            CommandResult::Ok,
            CommandResult::WorktreeCreated { branch: "feat-x".into() },
            CommandResult::BranchNameGenerated { name: "feat-y".into(), issue_ids: vec![("github".into(), "42".into())] },
            CommandResult::Error { message: "boom".into() },
        ];
        for result in results {
            let json = serde_json::to_string(&result).unwrap();
            let _: CommandResult = serde_json::from_str(&json).unwrap();
        }
    }

    #[test]
    fn proto_work_item_roundtrip() {
        let item = ProtoWorkItem {
            kind: ProtoWorkItemKind::Checkout,
            identity: ProtoWorkItemIdentity::Checkout(PathBuf::from("/tmp/wt")),
            branch: Some("main".into()),
            description: "Main branch".into(),
            checkout: Some(ProtoCheckoutRef { key: PathBuf::from("/tmp/wt"), is_main_worktree: true }),
            pr_key: None,
            session_key: None,
            issue_keys: vec![],
            workspace_refs: vec![],
            is_main_worktree: true,
        };
        let json = serde_json::to_string(&item).unwrap();
        let parsed: ProtoWorkItem = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kind, ProtoWorkItemKind::Checkout);
        assert_eq!(parsed.branch, Some("main".into()));
    }
}
```

**Step 5: Verify tests pass**

Run: `cargo test -p flotilla-protocol`
Expected: All 4 tests pass.

**Step 6: Commit**

```bash
git add -A
git commit -m "feat: define protocol types in flotilla-protocol"
```

---

### Task 5: Define DaemonHandle trait and conversion layer

Define the trait in `flotilla-core` and implement conversions between core types and protocol types. This is the key boundary — everything the TUI needs from the daemon goes through this trait.

**Files:**
- Create: `crates/flotilla-core/src/daemon.rs`
- Create: `crates/flotilla-core/src/convert.rs`
- Modify: `crates/flotilla-core/src/lib.rs`
- Test: conversion round-trips in `crates/flotilla-core/src/convert.rs`

**Step 1: Write the DaemonHandle trait**

```rust
// crates/flotilla-core/src/daemon.rs
use std::path::Path;
use async_trait::async_trait;
use tokio::sync::broadcast;

use flotilla_protocol::{
    CommandResult, DaemonEvent, ProtoCommand, RepoInfo, Snapshot,
};

/// The boundary between daemon and client.
/// Both InProcessDaemon and SocketDaemon implement this.
#[async_trait]
pub trait DaemonHandle: Send + Sync {
    /// Subscribe to daemon events (snapshots, repo changes).
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent>;

    /// Get full current state for a repo.
    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String>;

    /// List all tracked repos.
    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String>;

    /// Execute a command.
    async fn execute(&self, repo: &Path, command: ProtoCommand) -> Result<CommandResult, String>;

    /// Trigger an immediate refresh for a repo.
    async fn refresh(&self, repo: &Path) -> Result<(), String>;

    /// Add a repo.
    async fn add_repo(&self, path: &Path) -> Result<(), String>;

    /// Remove a repo.
    async fn remove_repo(&self, path: &Path) -> Result<(), String>;
}
```

**Step 2: Write core→protocol conversion functions**

```rust
// crates/flotilla-core/src/convert.rs
use std::collections::HashMap;
use std::path::PathBuf;

use crate::data::{
    CheckoutRef, CorrelatedAnchor, WorkItem, WorkItemKind, WorkItemIdentity,
    ProviderError, StandaloneWorkItem,
};
use crate::refresh::RefreshSnapshot;

use flotilla_protocol::{
    ProtoCheckoutRef, ProtoError, ProtoWorkItem, ProtoWorkItemIdentity,
    ProtoWorkItemKind, Snapshot,
};

pub fn work_item_kind_to_proto(kind: WorkItemKind) -> ProtoWorkItemKind {
    match kind {
        WorkItemKind::Checkout => ProtoWorkItemKind::Checkout,
        WorkItemKind::Session => ProtoWorkItemKind::Session,
        WorkItemKind::Pr => ProtoWorkItemKind::Pr,
        WorkItemKind::RemoteBranch => ProtoWorkItemKind::RemoteBranch,
        WorkItemKind::Issue => ProtoWorkItemKind::Issue,
    }
}

pub fn work_item_identity_to_proto(identity: &WorkItemIdentity) -> ProtoWorkItemIdentity {
    match identity {
        WorkItemIdentity::Checkout(p) => ProtoWorkItemIdentity::Checkout(p.clone()),
        WorkItemIdentity::ChangeRequest(id) => ProtoWorkItemIdentity::ChangeRequest(id.clone()),
        WorkItemIdentity::Session(id) => ProtoWorkItemIdentity::Session(id.clone()),
        WorkItemIdentity::Issue(id) => ProtoWorkItemIdentity::Issue(id.clone()),
        WorkItemIdentity::RemoteBranch(b) => ProtoWorkItemIdentity::RemoteBranch(b.clone()),
    }
}

pub fn work_item_to_proto(item: &WorkItem) -> ProtoWorkItem {
    let identity = item.identity()
        .map(|id| work_item_identity_to_proto(&id))
        .unwrap_or_else(|| ProtoWorkItemIdentity::RemoteBranch(String::new()));

    ProtoWorkItem {
        kind: work_item_kind_to_proto(item.kind()),
        identity,
        branch: item.branch().map(|s| s.to_string()),
        description: item.description().to_string(),
        checkout: item.checkout().map(|co| ProtoCheckoutRef {
            key: co.key.clone(),
            is_main_worktree: co.is_main_worktree,
        }),
        pr_key: item.pr_key().map(|s| s.to_string()),
        session_key: item.session_key().map(|s| s.to_string()),
        issue_keys: item.issue_keys().to_vec(),
        workspace_refs: item.workspace_refs().to_vec(),
        is_main_worktree: item.is_main_worktree(),
    }
}

pub fn snapshot_to_proto(repo: &PathBuf, seq: u64, refresh: &RefreshSnapshot) -> Snapshot {
    Snapshot {
        seq,
        repo: repo.clone(),
        work_items: refresh.work_items.iter().map(work_item_to_proto).collect(),
        provider_health: refresh.provider_health.iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect(),
        errors: refresh.errors.iter()
            .map(|e| ProtoError {
                category: e.category.to_string(),
                message: e.message.clone(),
            })
            .collect(),
    }
}

pub fn error_to_proto(error: &ProviderError) -> ProtoError {
    ProtoError {
        category: error.category.to_string(),
        message: error.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::*;

    #[test]
    fn convert_correlated_checkout() {
        let wi = WorkItem::Correlated(CorrelatedWorkItem {
            anchor: CorrelatedAnchor::Checkout(CheckoutRef {
                key: PathBuf::from("/tmp/wt"),
                is_main_worktree: false,
            }),
            branch: Some("feat-x".into()),
            description: "Feature X".into(),
            linked_pr: Some("42".into()),
            linked_session: None,
            linked_issues: vec!["7".into()],
            workspace_refs: vec!["ws-1".into()],
            correlation_group_idx: 0,
        });
        let proto = work_item_to_proto(&wi);
        assert_eq!(proto.kind, ProtoWorkItemKind::Checkout);
        assert_eq!(proto.branch, Some("feat-x".into()));
        assert_eq!(proto.pr_key, Some("42".into()));
        assert_eq!(proto.issue_keys, vec!["7"]);
        assert!(proto.checkout.is_some());
        assert!(!proto.is_main_worktree);
    }

    #[test]
    fn convert_standalone_issue() {
        let wi = WorkItem::Standalone(StandaloneWorkItem::Issue {
            key: "99".into(),
            description: "Bug report".into(),
        });
        let proto = work_item_to_proto(&wi);
        assert_eq!(proto.kind, ProtoWorkItemKind::Issue);
        assert_eq!(proto.identity, ProtoWorkItemIdentity::Issue("99".into()));
        assert_eq!(proto.description, "Bug report");
    }
}
```

**Step 3: Update flotilla-core lib.rs**

Add the new modules:

```rust
pub mod command;
pub mod config;
pub mod convert;
pub mod daemon;
pub mod data;
pub mod model;
pub mod provider_data;
pub mod providers;
pub mod refresh;
```

**Step 4: Verify tests pass**

Run: `cargo test -p flotilla-core`
Expected: All existing tests + 2 new conversion tests pass.

**Step 5: Commit**

```bash
git add -A
git commit -m "feat: define DaemonHandle trait and core-to-protocol conversion"
```

---

### Task 6: Implement InProcessDaemon

The in-process implementation wraps the existing `AppModel` + `RepoRefreshHandle` machinery. It owns the repos, runs refresh loops, executes commands, and broadcasts events.

**Files:**
- Create: `crates/flotilla-core/src/in_process.rs`
- Modify: `crates/flotilla-core/src/lib.rs`
- Modify: `crates/flotilla-core/src/command.rs` (make Command derive Clone for internal use)
- Test: `crates/flotilla-core/src/in_process.rs`

**Step 1: Refactor executor out of App dependency**

The current `executor.rs` takes `&mut App`. We need a daemon-side executor that takes the registry + repo state and returns a `CommandResult`. Create a new function in `crates/flotilla-core/src/executor.rs`:

```rust
// crates/flotilla-core/src/executor.rs
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, debug, error};

use crate::config;
use crate::data;
use crate::providers;
use crate::providers::registry::ProviderRegistry;
use crate::providers::types::WorkspaceConfig;
use crate::provider_data::ProviderData;

use flotilla_protocol::{CommandResult, ProtoCommand, ProtoDeleteInfo};

/// Execute a command against providers, returning a result.
/// This is the daemon-side executor — no UI state involved.
pub async fn execute(
    cmd: ProtoCommand,
    repo_root: &Path,
    registry: &ProviderRegistry,
    providers_data: &ProviderData,
) -> CommandResult {
    match cmd {
        ProtoCommand::SwitchWorktree { path } => {
            if let Some(co) = providers_data.checkouts.get(&path) {
                info!("entering workspace for {}", co.branch);
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let config = workspace_config(repo_root, &co.branch, &co.path, "claude");
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        return CommandResult::Error { message: e };
                    }
                }
                CommandResult::Ok
            } else {
                CommandResult::Error { message: format!("checkout not found: {}", path.display()) }
            }
        }
        ProtoCommand::SelectWorkspace { ws_ref } => {
            info!("switching to workspace {ws_ref}");
            if let Some((_, ws_mgr)) = &registry.workspace_manager {
                if let Err(e) = ws_mgr.select_workspace(&ws_ref).await {
                    return CommandResult::Error { message: e };
                }
            }
            CommandResult::Ok
        }
        ProtoCommand::CreateWorktree { branch, create_branch, issue_ids } => {
            info!("creating worktree {branch}");
            let cm = match registry.checkout_managers.values().next() {
                Some(cm) => cm,
                None => return CommandResult::Error { message: "No checkout manager available".into() },
            };
            match cm.create_checkout(repo_root, &branch, create_branch).await {
                Ok(checkout) => {
                    if !issue_ids.is_empty() {
                        write_branch_issue_links(repo_root, &branch, &issue_ids).await;
                    }
                    info!("created worktree at {}", checkout.path.display());
                    if let Some((_, ws_mgr)) = &registry.workspace_manager {
                        let config = workspace_config(repo_root, &branch, &checkout.path, "claude");
                        if let Err(e) = ws_mgr.create_workspace(&config).await {
                            return CommandResult::Error { message: e };
                        }
                    }
                    CommandResult::WorktreeCreated { branch }
                }
                Err(e) => {
                    error!("create worktree failed: {e}");
                    CommandResult::Error { message: e }
                }
            }
        }
        ProtoCommand::RemoveCheckout { branch } => {
            let cm = match registry.checkout_managers.values().next() {
                Some(cm) => cm,
                None => return CommandResult::Error { message: "No checkout manager available".into() },
            };
            match cm.remove_checkout(repo_root, &branch).await {
                Ok(()) => CommandResult::Ok,
                Err(e) => CommandResult::Error { message: e },
            }
        }
        ProtoCommand::OpenPr { id } => {
            debug!("opening PR {id} in browser");
            if let Some(cr) = registry.code_review.values().next() {
                let _ = cr.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }
        ProtoCommand::OpenIssueBrowser { id } => {
            debug!("opening issue {id} in browser");
            if let Some(it) = registry.issue_trackers.values().next() {
                let _ = it.open_in_browser(repo_root, &id).await;
            }
            CommandResult::Ok
        }
        ProtoCommand::LinkIssuesToPr { pr_id, issue_ids } => {
            info!("linking issues {:?} to PR #{pr_id}", issue_ids);
            let body_result = providers::run_cmd(
                "gh",
                &["pr", "view", &pr_id, "--json", "body", "--jq", ".body"],
                repo_root,
            ).await;
            match body_result {
                Ok(current_body) => {
                    let fixes_lines: Vec<String> = issue_ids.iter()
                        .map(|id| format!("Fixes #{id}"))
                        .collect();
                    let new_body = if current_body.trim().is_empty() {
                        fixes_lines.join("\n")
                    } else {
                        format!("{}\n\n{}", current_body.trim(), fixes_lines.join("\n"))
                    };
                    match providers::run_cmd(
                        "gh",
                        &["pr", "edit", &pr_id, "--body", &new_body],
                        repo_root,
                    ).await {
                        Ok(_) => { info!("linked issues to PR #{pr_id}"); CommandResult::Ok }
                        Err(e) => { error!("failed to edit PR: {e}"); CommandResult::Error { message: e } }
                    }
                }
                Err(e) => {
                    error!("failed to read PR body: {e}");
                    CommandResult::Error { message: e }
                }
            }
        }
        ProtoCommand::ArchiveSession { session_id } => {
            if let Some(ca) = registry.coding_agents.values().next() {
                match ca.archive_session(&session_id).await {
                    Ok(()) => CommandResult::Ok,
                    Err(e) => CommandResult::Error { message: e },
                }
            } else {
                CommandResult::Error { message: "No coding agent available".into() }
            }
        }
        ProtoCommand::GenerateBranchName { issue_keys } => {
            let issues: Vec<(String, String)> = issue_keys.iter()
                .filter_map(|k| providers_data.issues.get(k.as_str()))
                .map(|issue| (issue.id.clone(), issue.title.clone()))
                .collect();
            let issue_id_pairs: Vec<(String, String)> = {
                let provider = registry.issue_trackers
                    .keys().next().cloned().unwrap_or_else(|| "github".to_string());
                issues.iter()
                    .map(|(id, _)| (provider.clone(), id.clone()))
                    .collect()
            };
            if let Some(ai) = registry.ai_utilities.values().next() {
                let context: Vec<String> = issues.iter()
                    .map(|(id, title)| format!("{} #{}", title, id))
                    .collect();
                let prompt = if context.len() == 1 { context[0].clone() } else { context.join("; ") };
                match ai.generate_branch_name(&prompt).await {
                    Ok(name) => {
                        info!("AI suggested: {name}");
                        CommandResult::BranchNameGenerated { name, issue_ids: issue_id_pairs }
                    }
                    Err(_) => {
                        let fallback: Vec<String> = issues.iter()
                            .map(|(id, _)| format!("issue-{id}"))
                            .collect();
                        CommandResult::BranchNameGenerated { name: fallback.join("-"), issue_ids: issue_id_pairs }
                    }
                }
            } else {
                let fallback: Vec<String> = issues.iter()
                    .map(|(id, _)| format!("issue-{id}"))
                    .collect();
                CommandResult::BranchNameGenerated { name: fallback.join("-"), issue_ids: issue_id_pairs }
            }
        }
        ProtoCommand::TeleportSession { session_id, branch, checkout_key } => {
            info!("teleporting to session {session_id}");
            let claude_bin = providers::resolve_claude_path().unwrap_or_else(|| "claude".into());
            let teleport_cmd = format!("{} --teleport {}", claude_bin, session_id);
            let wt_path = if let Some(ref key) = checkout_key {
                providers_data.checkouts.get(key).map(|co| co.path.clone())
            } else if let Some(ref branch_name) = branch {
                if let Some(cm) = registry.checkout_managers.values().next() {
                    cm.create_checkout(repo_root, branch_name, false).await.ok().map(|c| c.path)
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(path) = wt_path {
                let name = branch.as_deref().unwrap_or("session");
                if let Some((_, ws_mgr)) = &registry.workspace_manager {
                    let config = workspace_config(repo_root, name, &path, &teleport_cmd);
                    if let Err(e) = ws_mgr.create_workspace(&config).await {
                        return CommandResult::Error { message: e };
                    }
                }
            }
            CommandResult::Ok
        }
        ProtoCommand::AddRepo { .. } | ProtoCommand::RemoveRepo { .. } | ProtoCommand::Refresh => {
            // These are handled at the daemon level, not per-repo executor
            CommandResult::Ok
        }
    }
}

pub fn workspace_config(
    repo_root: &Path,
    name: &str,
    working_dir: &Path,
    main_command: &str,
) -> WorkspaceConfig {
    let tmpl_path = repo_root.join(".flotilla/workspace.yaml");
    let template_yaml = std::fs::read_to_string(&tmpl_path).ok().or_else(|| {
        let global_path = dirs::home_dir()?.join(".config/flotilla/workspace.yaml");
        std::fs::read_to_string(global_path).ok()
    });
    let mut template_vars = std::collections::HashMap::new();
    template_vars.insert("main_command".to_string(), main_command.to_string());
    WorkspaceConfig {
        name: name.to_string(),
        working_directory: working_dir.to_path_buf(),
        template_vars,
        template_yaml,
    }
}

async fn write_branch_issue_links(repo_root: &Path, branch: &str, issue_ids: &[(String, String)]) {
    use std::collections::HashMap;
    let mut by_provider: HashMap<&str, Vec<&str>> = HashMap::new();
    for (provider, id) in issue_ids {
        by_provider.entry(provider.as_str()).or_default().push(id.as_str());
    }
    for (provider, ids) in by_provider {
        let key = format!("branch.{branch}.flotilla.issues.{provider}");
        let value = ids.join(",");
        if let Err(e) = providers::run_cmd("git", &["config", &key, &value], repo_root).await {
            tracing::warn!("failed to write issue link: {e}");
        }
    }
}
```

**Step 2: Implement InProcessDaemon**

```rust
// crates/flotilla-core/src/in_process.rs
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use async_trait::async_trait;
use tokio::sync::{broadcast, RwLock};

use crate::config;
use crate::convert;
use crate::daemon::DaemonHandle;
use crate::executor;
use crate::model::RepoModel;
use crate::providers::discovery;
use crate::providers::types::RepoCriteria;
use crate::refresh::RefreshSnapshot;

use flotilla_protocol::{
    CommandResult, DaemonEvent, ProtoCommand, RepoInfo, Snapshot,
};

struct RepoState {
    model: RepoModel,
    seq: u64,
    last_snapshot: Arc<RefreshSnapshot>,
}

pub struct InProcessDaemon {
    repos: RwLock<HashMap<PathBuf, RepoState>>,
    repo_order: RwLock<Vec<PathBuf>>,
    event_tx: broadcast::Sender<DaemonEvent>,
}

impl InProcessDaemon {
    pub fn new(repo_paths: Vec<PathBuf>) -> Self {
        let (event_tx, _) = broadcast::channel(256);
        let mut repos = HashMap::new();
        let mut order = Vec::new();

        for path in repo_paths {
            if repos.contains_key(&path) {
                continue;
            }
            let registry = discovery::detect_providers(&path);
            let model = RepoModel::new(path.clone(), registry);
            repos.insert(path.clone(), RepoState {
                model,
                seq: 0,
                last_snapshot: Arc::new(RefreshSnapshot::default()),
            });
            order.push(path);
        }

        Self {
            repos: RwLock::new(repos),
            repo_order: RwLock::new(order),
            event_tx,
        }
    }

    /// Poll for new snapshots from background refresh tasks and broadcast them.
    /// Called by the TUI event loop (replaces drain_snapshots).
    pub async fn poll_snapshots(&self) {
        let mut repos = self.repos.write().await;
        for (path, state) in repos.iter_mut() {
            let Some(ref mut handle) = state.model.refresh_handle else { continue };
            if !handle.snapshot_rx.has_changed().unwrap_or(false) {
                continue;
            }

            let snapshot = handle.snapshot_rx.borrow_and_update().clone();

            // Handle issues_disabled
            let issues_disabled = snapshot.errors.iter().any(|e|
                e.category == "issues" && e.message.contains("has disabled issues")
            );
            if issues_disabled {
                handle.skip_issues.store(true, std::sync::atomic::Ordering::Relaxed);
            }

            state.seq += 1;
            state.last_snapshot = snapshot.clone();

            // Update model data
            state.model.data.providers = Arc::clone(&snapshot.providers);
            state.model.data.correlation_groups = snapshot.correlation_groups.clone();
            state.model.data.provider_health = snapshot.provider_health.clone();
            state.model.data.loading = false;

            let proto_snapshot = convert::snapshot_to_proto(path, state.seq, &snapshot);
            let _ = self.event_tx.send(DaemonEvent::Snapshot(proto_snapshot));
        }
    }
}

#[async_trait]
impl DaemonHandle for InProcessDaemon {
    fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.event_tx.subscribe()
    }

    async fn get_state(&self, repo: &Path) -> Result<Snapshot, String> {
        let repos = self.repos.read().await;
        let state = repos.get(repo).ok_or_else(|| format!("repo not found: {}", repo.display()))?;
        Ok(convert::snapshot_to_proto(&repo.to_path_buf(), state.seq, &state.last_snapshot))
    }

    async fn list_repos(&self) -> Result<Vec<RepoInfo>, String> {
        let repos = self.repos.read().await;
        let order = self.repo_order.read().await;
        let mut infos = Vec::new();
        for path in order.iter() {
            if let Some(state) = repos.get(path) {
                infos.push(RepoInfo {
                    path: path.clone(),
                    name: path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default(),
                    provider_health: state.model.data.provider_health.iter()
                        .map(|(k, v)| (k.to_string(), *v))
                        .collect(),
                    loading: state.model.data.loading,
                });
            }
        }
        Ok(infos)
    }

    async fn execute(&self, repo: &Path, command: ProtoCommand) -> Result<CommandResult, String> {
        let repos = self.repos.read().await;
        let state = repos.get(repo).ok_or_else(|| format!("repo not found: {}", repo.display()))?;
        let result = executor::execute(
            command,
            repo,
            &state.model.registry,
            &state.model.data.providers,
        ).await;

        // Trigger refresh after mutating commands
        if let Some(ref handle) = state.model.refresh_handle {
            handle.trigger_refresh();
        }

        Ok(result)
    }

    async fn refresh(&self, repo: &Path) -> Result<(), String> {
        let repos = self.repos.read().await;
        let state = repos.get(repo).ok_or_else(|| format!("repo not found: {}", repo.display()))?;
        if let Some(ref handle) = state.model.refresh_handle {
            handle.trigger_refresh();
        }
        Ok(())
    }

    async fn add_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();
        config::save_repo(&path);

        let mut repos = self.repos.write().await;
        if !repos.contains_key(&path) {
            let registry = discovery::detect_providers(&path);
            let model = RepoModel::new(path.clone(), registry);
            repos.insert(path.clone(), RepoState {
                model,
                seq: 0,
                last_snapshot: Arc::new(RefreshSnapshot::default()),
            });
            let mut order = self.repo_order.write().await;
            order.push(path.clone());
            config::save_tab_order(&order);

            let info = RepoInfo {
                path: path.clone(),
                name: path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                provider_health: Default::default(),
                loading: true,
            };
            let _ = self.event_tx.send(DaemonEvent::RepoAdded(info));
        }
        Ok(())
    }

    async fn remove_repo(&self, path: &Path) -> Result<(), String> {
        let path = path.to_path_buf();
        let mut repos = self.repos.write().await;
        repos.remove(&path);
        let mut order = self.repo_order.write().await;
        order.retain(|p| p != &path);
        config::save_tab_order(&order);
        let _ = self.event_tx.send(DaemonEvent::RepoRemoved { path });
        Ok(())
    }
}
```

**Step 3: Add FetchDeleteInfo handling**

The current `FetchDeleteInfo` command reads UI state (selectable index) to find the work item, then fetches info. In the daemon model, the client should resolve the work item identity first and send a command with the branch name. Add a new `ProtoCommand` variant if needed, or handle it as a `get_state` + client-side lookup + `RemoveCheckout`.

Actually, looking at the flow: the client knows the branch and PR from the work item. It can send a `FetchDeleteInfo { branch, worktree_path, pr_number }` command. Add this to `ProtoCommand`:

Update `crates/flotilla-protocol/src/commands.rs` to add:

```rust
FetchDeleteInfo { branch: String, worktree_path: Option<PathBuf>, pr_number: Option<String> },
```

And handle it in the executor by calling `data::fetch_delete_confirm_info()` and returning `CommandResult::DeleteInfo(...)`.

**Step 4: Update flotilla-core lib.rs**

```rust
pub mod command;
pub mod config;
pub mod convert;
pub mod daemon;
pub mod data;
pub mod executor;
pub mod in_process;
pub mod model;
pub mod provider_data;
pub mod providers;
pub mod refresh;
```

**Step 5: Verify it compiles**

Run: `cargo check -p flotilla-core`
Expected: Compiles. The `InProcessDaemon` may need minor adjustments for borrow checker issues with the `RwLock` — work through those.

**Step 6: Commit**

```bash
git add -A
git commit -m "feat: implement InProcessDaemon with DaemonHandle trait"
```

---

### Task 7: Rewire TUI to use DaemonHandle

Update the TUI to consume data through the `DaemonHandle` trait instead of directly accessing `AppModel`. This is the final step — after this, the TUI is a proper client.

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs` — replace `AppModel` with `DaemonHandle`
- Modify: `crates/flotilla-tui/src/app/executor.rs` — TUI-side command result handling
- Modify: `crates/flotilla-tui/src/app/intent.rs` — resolve to `ProtoCommand`
- Modify: `crates/flotilla-tui/src/app/ui_state.rs` — use protocol types for table view
- Modify: `crates/flotilla-tui/src/ui.rs` — render from protocol snapshot data
- Modify: `src/main.rs` — create `InProcessDaemon`, pass to TUI

**Step 1: Define TUI-side App struct using DaemonHandle**

The TUI `App` struct changes from owning `AppModel` to holding a `DaemonHandle` reference and local protocol-level state:

```rust
// In crates/flotilla-tui/src/app/mod.rs
use std::sync::Arc;
use flotilla_core::daemon::DaemonHandle;
use flotilla_protocol::{Snapshot, ProtoWorkItem, RepoInfo};

pub struct App {
    pub daemon: Arc<dyn DaemonHandle>,
    pub repos: Vec<RepoInfo>,
    pub active_repo: usize,
    pub ui: UiState,
    pub commands: CommandQueue,  // Now queues ProtoCommands
    pub should_quit: bool,
    pub snapshots: HashMap<PathBuf, Snapshot>,  // Latest snapshot per repo
    pub status_message: Option<String>,
}
```

The TUI now:
1. Calls `daemon.list_repos()` at startup to populate repo list
2. Calls `daemon.get_state(repo)` for initial hydration
3. Reads `daemon.subscribe()` for ongoing updates
4. Sends `daemon.execute(repo, command)` for user actions
5. Interprets `CommandResult` locally to update UI state (e.g. `BranchNameGenerated` → `BranchInput` mode)

**Step 2: Update TUI executor to handle CommandResults**

Instead of the executor directly modifying `app.ui.mode`, it now receives `CommandResult` and updates UI state accordingly:

```rust
// crates/flotilla-tui/src/app/executor.rs
use flotilla_protocol::CommandResult;

pub fn handle_command_result(result: &CommandResult, app: &mut App) {
    match result {
        CommandResult::Ok => {}
        CommandResult::WorktreeCreated { branch } => {
            tracing::info!("created worktree {branch}");
        }
        CommandResult::BranchNameGenerated { name, issue_ids } => {
            app.prefill_branch_input(name, issue_ids.clone());
        }
        CommandResult::DeleteInfo(info) => {
            // Convert to UI delete confirm state
            app.ui.mode = UiMode::DeleteConfirm {
                info: Some(/* convert ProtoDeleteInfo to DeleteConfirmInfo */),
                loading: false,
            };
        }
        CommandResult::Error { message } => {
            app.status_message = Some(message.clone());
        }
    }
}
```

**Step 3: Update intent.rs to resolve to ProtoCommand**

Change `Intent::resolve()` to return `Option<ProtoCommand>` instead of `Option<Command>`. The work item data it needs is available from the protocol `ProtoWorkItem` — no need to access `ProviderData` directly. The one exception is `LinkIssuesToPr` which currently reads `ProviderData` to diff issue lists — this needs to work from the snapshot data or move the diffing to the daemon side.

Simplest: move issue diff logic to the daemon. The client sends `LinkIssuesToPr { pr_id }` and the daemon figures out which issues are missing. Add the issue diff to the daemon executor.

**Step 4: Update table building**

The current `build_table_view` in `data.rs` takes `&[WorkItem]` and `&ProviderData`. In the daemon model, the client receives `Vec<ProtoWorkItem>` which has all the info needed. Create a TUI-side `build_table_view` that works with `ProtoWorkItem` instead.

This is the largest change — `ui.rs` renders from `ProtoWorkItem` data instead of `WorkItem`. The fields map 1:1 (that's why we designed `ProtoWorkItem` to be flat), so this is mostly mechanical import changes.

**Step 5: Update main.rs event loop**

```rust
// src/main.rs
async fn run(terminal: &mut ratatui::DefaultTerminal, repo_roots: Vec<PathBuf>) -> Result<()> {
    let daemon = Arc::new(flotilla_core::in_process::InProcessDaemon::new(repo_roots));
    let mut app = flotilla_tui::app::App::new(daemon.clone()).await;
    let mut events = flotilla_tui::event::EventHandler::new(Duration::from_millis(250));
    let mut event_rx = daemon.subscribe();

    loop {
        // Poll for daemon snapshots (in-process)
        daemon.poll_snapshots().await;

        // Drain daemon events
        while let Ok(event) = event_rx.try_recv() {
            app.handle_daemon_event(event);
        }

        terminal.draw(|f| flotilla_tui::ui::render(&app, f))?;

        if let Some(evt) = events.next().await {
            app.handle_terminal_event(evt);
        }

        // Process command queue — send to daemon
        while let Some(cmd) = app.commands.take_next() {
            let repo = app.active_repo_path().clone();
            let result = daemon.execute(&repo, cmd).await;
            if let Ok(result) = result {
                app.handle_command_result(result);
            }
        }

        if app.should_quit {
            break;
        }
    }
    Ok(())
}
```

**Step 6: Verify it compiles and runs**

Run: `cargo check`
Run: `cargo test --workspace`
Run: `cargo run -- --repo-root .` (manual smoke test)

Expected: Same behavior as before — TUI shows work items, navigation works, commands execute.

**Step 7: Commit**

```bash
git add -A
git commit -m "refactor: rewire TUI to consume DaemonHandle trait"
```

---

### Task 8: Clean up and delete old code

Remove the temporary re-exports, old `src/app/` executor, and any dead code from the migration.

**Files:**
- Delete: `src/lib.rs` re-exports (if any remain)
- Delete: old `src/app/` directory (should already be gone from Task 3)
- Clean up: `crate/flotilla-core/src/command.rs` — the old `Command` enum and `CommandQueue` can be removed if fully replaced by `ProtoCommand`
- Clean up: `crates/flotilla-core/src/model.rs` — remove `AppModel` if fully replaced by `InProcessDaemon`

**Step 1: Audit imports**

Run: `cargo clippy --workspace`
Expected: No warnings about unused imports or dead code.

**Step 2: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 3: Commit**

```bash
git add -A
git commit -m "chore: remove legacy code from daemon migration"
```

---

## Summary

| Task | What | Commit message |
|------|------|---------------|
| 1 | Create Cargo workspace | `chore: create cargo workspace with 4 crates` |
| 2 | Move core modules to flotilla-core | `refactor: move core modules into flotilla-core crate` |
| 3 | Move TUI modules to flotilla-tui | `refactor: move TUI modules into flotilla-tui crate` |
| 4 | Define protocol types | `feat: define protocol types in flotilla-protocol` |
| 5 | DaemonHandle trait + conversions | `feat: define DaemonHandle trait and core-to-protocol conversion` |
| 6 | InProcessDaemon implementation | `feat: implement InProcessDaemon with DaemonHandle trait` |
| 7 | Rewire TUI as client | `refactor: rewire TUI to consume DaemonHandle trait` |
| 8 | Clean up dead code | `chore: remove legacy code from daemon migration` |

Tasks 1-3 are mechanical moves. Task 4 is new code with tests. Tasks 5-6 are the architectural core. Task 7 is the integration. Task 8 is cleanup.
