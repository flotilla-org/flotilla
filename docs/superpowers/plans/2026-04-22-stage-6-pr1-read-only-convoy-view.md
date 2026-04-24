# Stage 6 PR 1 — Read-Only Convoy View Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the read-only convoy view end-to-end: new wire protocol types + daemon projection + client/CLI replay handling + TUI widgets, producing a working `Convoys` tab that displays live convoys and task DAG status.

**Architecture:** Convoys flow on their own namespace-scoped stream (`StreamKey::Namespace`). The daemon runs a `ConvoyProjection` that watches `Convoy` and `Presentation` resources and emits `NamespaceSnapshot` / `NamespaceDelta` events. The TUI reads the resulting snapshot from its `AppModel` and renders via a new `ConvoysPage` widget using `tui-tree-widget`.

**Tech Stack:** Rust (stable, pinned nightly for fmt); tokio; ratatui 0.30; tui-tree-widget 0.24; insta for snapshot tests; `InMemoryResourceClient` for deterministic projection tests.

**Spec:** `docs/superpowers/specs/2026-04-21-tui-convoy-view-design.md`. Must stay consistent — if anything in the spec reads wrong during implementation, stop and update the spec, don't drift.

---

## File Structure

### New files

| Path | Responsibility |
|------|----------------|
| `crates/flotilla-protocol/src/namespace.rs` | Wire types for the namespace stream: `ConvoyId`, `ConvoyPhase`, `TaskPhase`, `ConvoySummary`, `TaskSummary`, `ProcessSummary`, `NamespaceSnapshot`, `NamespaceDelta`. |
| `crates/flotilla-daemon/src/convoy_projection.rs` | `ConvoyProjection` struct: watches `Convoy` + `Presentation` resources, maintains in-memory namespace view, emits snapshots/deltas. |
| `crates/flotilla-tui/src/widgets/convoys_page/mod.rs` | `ConvoysPage` widget root. Submodule. |
| `crates/flotilla-tui/src/widgets/convoys_page/list.rs` | `ConvoyList` widget. |
| `crates/flotilla-tui/src/widgets/convoys_page/detail.rs` | `ConvoyDetail` widget (header + tree + processes). |
| `crates/flotilla-tui/src/widgets/convoys_page/glyphs.rs` | Status-glyph helpers for `ConvoyPhase` / `TaskPhase`. |

### Modified files

| Path | Change |
|------|--------|
| `crates/flotilla-protocol/src/lib.rs` | Export the new namespace module; add `StreamKey::Namespace { name }`; add `DaemonEvent::NamespaceSnapshot` / `DaemonEvent::NamespaceDelta` variants. |
| `crates/flotilla-daemon/src/lib.rs` | `mod convoy_projection;` |
| `crates/flotilla-daemon/src/runtime.rs` | Spawn `ConvoyProjection` during daemon startup; route its events to the client event bus. |
| `crates/flotilla-daemon/src/server/request_dispatch.rs` | Include namespace events in `ReplaySince` response. |
| `crates/flotilla-client/src/lib.rs` | Track per-namespace seq; include namespace cursors in `ReplaySince`; dispatch `NamespaceSnapshot`/`NamespaceDelta` events. |
| `crates/flotilla-tui/Cargo.toml` | Add `tui-tree-widget` dependency. |
| `crates/flotilla-tui/src/cli.rs` | Update `watch` subcommand replay dedupe + formatting for namespace events. |
| `crates/flotilla-tui/src/binding_table.rs` | Add `BindingModeId::Convoys`; add convoy-tab bindings. |
| `crates/flotilla-tui/src/app/ui_state.rs` | Add `TabId::Convoys`. |
| `crates/flotilla-tui/src/app/mod.rs` | Add `AppModel::convoys` state (indexed by namespace); apply snapshots/deltas; route to widget. |
| `crates/flotilla-tui/src/widgets/mod.rs` | Export `convoys_page`. |
| `crates/flotilla-tui/src/widgets/screen.rs` | Render `ConvoysPage` when the active tab is `TabId::Convoys`. |
| `crates/flotilla-tui/src/widgets/tabs.rs` | Render the `Convoys` tab label alongside `Flotilla`. |

---

## Task 1: Scaffold the namespace wire-protocol module

**Files:**
- Create: `crates/flotilla-protocol/src/namespace.rs`
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Create an empty `namespace.rs`**

```rust
// crates/flotilla-protocol/src/namespace.rs
//
// Wire types for the namespace-scoped stream carrying convoy state.
// Parallel to RepoSnapshot / HostSnapshot for the per-repo / per-host streams.
// Shape deliberately mirrors ConvoyStatus fields rather than introducing a new
// vocabulary — easier to replace when the wire protocol shifts k8s-shape.
```

- [ ] **Step 2: Wire the module into the protocol crate**

Add to `crates/flotilla-protocol/src/lib.rs` near the other `pub mod` declarations:

```rust
pub mod namespace;
```

- [ ] **Step 3: Compile the empty module**

Run: `cargo build -p flotilla-protocol --locked`
Expected: builds cleanly.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-protocol/src/namespace.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat(protocol): scaffold namespace wire module"
```

---

## Task 2: Define `ConvoyId`, `ConvoyPhase`, `TaskPhase`

**Files:**
- Modify: `crates/flotilla-protocol/src/namespace.rs`
- Test: `crates/flotilla-protocol/src/namespace.rs` (inline `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test first (serde round-trip for phases + ConvoyId display/parse)**

Append to `namespace.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convoy_id_round_trips_through_string() {
        let id = ConvoyId::new("flotilla", "fix-bug-123");
        assert_eq!(id.as_str(), "flotilla/fix-bug-123");
        let parsed = ConvoyId::parse("flotilla/fix-bug-123").expect("parse");
        assert_eq!(parsed, id);
    }

    #[test]
    fn convoy_id_rejects_missing_separator() {
        assert!(ConvoyId::parse("no-slash").is_err());
    }

    #[test]
    fn convoy_phase_serde_round_trips() {
        for phase in [
            ConvoyPhase::Pending,
            ConvoyPhase::Active,
            ConvoyPhase::Completed,
            ConvoyPhase::Failed,
            ConvoyPhase::Cancelled,
        ] {
            let encoded = serde_json::to_string(&phase).unwrap();
            let decoded: ConvoyPhase = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, phase);
        }
    }

    #[test]
    fn task_phase_serde_round_trips() {
        for phase in [
            TaskPhase::Pending,
            TaskPhase::Ready,
            TaskPhase::Launching,
            TaskPhase::Running,
            TaskPhase::Completed,
            TaskPhase::Failed,
            TaskPhase::Cancelled,
        ] {
            let encoded = serde_json::to_string(&phase).unwrap();
            let decoded: TaskPhase = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, phase);
        }
    }
}
```

- [ ] **Step 2: Run tests, confirm they fail**

Run: `cargo test -p flotilla-protocol --locked namespace::tests`
Expected: FAIL — `ConvoyId`, `ConvoyPhase`, `TaskPhase` not defined.

- [ ] **Step 3: Define the types**

Prepend to `namespace.rs` (before the tests module):

```rust
use serde::{Deserialize, Serialize};

/// Stable identifier for a convoy resource: `"namespace/name"`.
/// Opaque string; parsing validates the `/` separator. No rename support.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConvoyId(String);

impl ConvoyId {
    pub fn new(namespace: impl Into<String>, name: impl Into<String>) -> Self {
        let ns = namespace.into();
        let nm = name.into();
        Self(format!("{ns}/{nm}"))
    }

    pub fn parse(s: impl AsRef<str>) -> Result<Self, String> {
        let s = s.as_ref();
        if !s.contains('/') {
            return Err(format!("convoy id missing '/' separator: {s}"));
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Namespace component (substring before `/`).
    pub fn namespace(&self) -> &str {
        self.0.split_once('/').map(|(ns, _)| ns).unwrap_or("")
    }

    /// Name component (substring after `/`).
    pub fn name(&self) -> &str {
        self.0.split_once('/').map(|(_, nm)| nm).unwrap_or("")
    }
}

/// Mirrors `ConvoyPhase` from the convoy resource design — do not simplify.
/// See docs/superpowers/specs/2026-04-14-convoy-resource-design.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConvoyPhase {
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
}

/// Mirrors `TaskPhase` from the convoy resource design — do not simplify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskPhase {
    Pending,
    Ready,
    Launching,
    Running,
    Completed,
    Failed,
    Cancelled,
}
```

- [ ] **Step 4: Run tests, confirm they pass**

Run: `cargo test -p flotilla-protocol --locked namespace::tests`
Expected: all four tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/namespace.rs
git commit -m "feat(protocol): add ConvoyId, ConvoyPhase, TaskPhase"
```

---

## Task 3: Define `ProcessSummary` and `TaskSummary`

**Files:**
- Modify: `crates/flotilla-protocol/src/namespace.rs`

- [ ] **Step 1: Write failing serde round-trip test**

Append inside the existing `tests` module:

```rust
#[test]
fn task_summary_round_trips() {
    let task = TaskSummary {
        name: "implement".into(),
        depends_on: vec!["setup".into()],
        phase: TaskPhase::Running,
        processes: vec![ProcessSummary {
            role: "coder".into(),
            command_preview: "claude".into(),
        }],
        host: None,
        checkout: None,
        workspace_ref: None,
        ready_at: None,
        started_at: None,
        finished_at: None,
        message: None,
    };
    let encoded = serde_json::to_string(&task).unwrap();
    let decoded: TaskSummary = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, task);
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-protocol --locked namespace::tests::task_summary_round_trips`
Expected: FAIL — types not defined.

- [ ] **Step 3: Define the types**

In `namespace.rs`, above the `tests` module, using the existing `chrono` pattern in the protocol crate if available; otherwise use `String` for timestamps for now. First check: `rg 'chrono' crates/flotilla-protocol/Cargo.toml` — if it's not there, add it to `Cargo.toml` under `[dependencies]`:

```toml
chrono = { version = "0.4", default-features = false, features = ["clock", "serde"] }
```

Then add to `namespace.rs`:

```rust
use chrono::{DateTime, Utc};

use crate::{host::HostName, snapshot::CheckoutRef};

pub type Timestamp = DateTime<Utc>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub role: String,
    /// Short human-readable preview of what the process runs. Derived from the
    /// frozen `ProcessDefinition` in the convoy's workflow snapshot. No process
    /// exit / live terminal status on this type — that is deferred (spec §Deferred).
    pub command_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub name: String,
    pub depends_on: Vec<String>,
    pub phase: TaskPhase,
    pub processes: Vec<ProcessSummary>,
    /// Host placement, when resolved by Stage 4. None while Pending/Ready.
    pub host: Option<HostName>,
    /// Checkout placement, when resolved by Stage 4.
    pub checkout: Option<CheckoutRef>,
    /// Attach target resolved from the task's matching Presentation.
    /// None until the per-task Presentation addendum lands
    /// (docs/superpowers/specs/2026-04-22-per-task-presentation-design.md).
    pub workspace_ref: Option<String>,
    pub ready_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub message: Option<String>,
}
```

- [ ] **Step 4: Run test, confirm pass**

Run: `cargo test -p flotilla-protocol --locked namespace::tests::task_summary_round_trips`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/namespace.rs crates/flotilla-protocol/Cargo.toml
git commit -m "feat(protocol): add ProcessSummary and TaskSummary"
```

---

## Task 4: Define `ConvoySummary` and the `initializing` flag

**Files:**
- Modify: `crates/flotilla-protocol/src/namespace.rs`

- [ ] **Step 1: Write failing test for a convoy summary with `initializing=true` (Pending, no tasks) and a convoy with tasks (Active)**

Append inside `tests`:

```rust
#[test]
fn convoy_summary_initializing_round_trips() {
    let convoy = ConvoySummary {
        id: ConvoyId::new("flotilla", "fix-bug-123"),
        namespace: "flotilla".into(),
        name: "fix-bug-123".into(),
        workflow_ref: "review-and-fix".into(),
        phase: ConvoyPhase::Pending,
        message: None,
        repo_hint: None,
        tasks: Vec::new(),
        started_at: None,
        finished_at: None,
        observed_workflow_ref: None,
        initializing: true,
    };
    let encoded = serde_json::to_string(&convoy).unwrap();
    let decoded: ConvoySummary = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, convoy);
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-protocol --locked namespace::tests::convoy_summary_initializing_round_trips`
Expected: FAIL — `ConvoySummary` not defined.

- [ ] **Step 3: Define `ConvoySummary`**

In `namespace.rs`, above `tests`, importing `RepoKey`:

```rust
use crate::snapshot::RepoKey; // may need to introduce — see note below
```

If `RepoKey` does not yet exist in `snapshot.rs`, introduce it as a newtype around the repo's display string, used only as a filter hint on the wire. Start minimal:

```rust
// In crates/flotilla-protocol/src/snapshot.rs, near RepoInfo:
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoKey(pub String);
```

Re-export it from `lib.rs` alongside the other `snapshot::` types. Then add to `namespace.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConvoySummary {
    pub id: ConvoyId,
    pub namespace: String,
    pub name: String,
    pub workflow_ref: String,
    pub phase: ConvoyPhase,
    pub message: Option<String>,
    /// Populated from a `flotilla.work/repo` label on the convoy when present.
    /// Consumed by the TUI as a filter hint; None means unclaimed.
    pub repo_hint: Option<RepoKey>,
    pub tasks: Vec<TaskSummary>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub observed_workflow_ref: Option<String>,
    /// True while the convoy's workflow_snapshot has not yet been populated by
    /// the controller. UI shows "initializing…" instead of an empty task tree.
    pub initializing: bool,
}
```

- [ ] **Step 4: Run test, confirm pass**

Run: `cargo test -p flotilla-protocol --locked namespace::tests::convoy_summary_initializing_round_trips`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/namespace.rs crates/flotilla-protocol/src/snapshot.rs crates/flotilla-protocol/src/lib.rs
git commit -m "feat(protocol): add ConvoySummary and RepoKey"
```

---

## Task 5: Define `NamespaceSnapshot` and `NamespaceDelta`

**Files:**
- Modify: `crates/flotilla-protocol/src/namespace.rs`

- [ ] **Step 1: Write failing test for namespace snapshot + delta serde round-trips**

Append inside `tests`:

```rust
#[test]
fn namespace_snapshot_round_trips() {
    let snap = NamespaceSnapshot {
        seq: 17,
        namespace: "flotilla".into(),
        convoys: vec![],
    };
    let encoded = serde_json::to_string(&snap).unwrap();
    let decoded: NamespaceSnapshot = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, snap);
}

#[test]
fn namespace_delta_round_trips() {
    let delta = NamespaceDelta {
        seq: 18,
        namespace: "flotilla".into(),
        changed: Vec::new(),
        removed: vec![ConvoyId::new("flotilla", "old-convoy")],
    };
    let encoded = serde_json::to_string(&delta).unwrap();
    let decoded: NamespaceDelta = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, delta);
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-protocol --locked namespace::tests`
Expected: the two new tests FAIL.

- [ ] **Step 3: Define the types**

Add to `namespace.rs`:

```rust
/// Full snapshot for one namespace. Sent on initial connect, after seq gaps,
/// or when a delta would be larger than the full snapshot. Mirrors the
/// RepoSnapshot idiom.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceSnapshot {
    pub seq: u64,
    pub namespace: String,
    pub convoys: Vec<ConvoySummary>,
}

/// Incremental delta for one namespace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceDelta {
    pub seq: u64,
    pub namespace: String,
    pub changed: Vec<ConvoySummary>,
    pub removed: Vec<ConvoyId>,
}
```

- [ ] **Step 4: Run tests, confirm pass**

Run: `cargo test -p flotilla-protocol --locked namespace::tests`
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-protocol/src/namespace.rs
git commit -m "feat(protocol): add NamespaceSnapshot and NamespaceDelta"
```

---

## Task 6: Extend `StreamKey` with `Namespace { name }`

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Write failing serde + exhaustiveness test**

In `crates/flotilla-protocol/src/lib.rs`, inside the existing `#[cfg(test)] mod tests` (or add one if missing) near other `StreamKey` tests:

```rust
#[test]
fn stream_key_namespace_round_trips() {
    let key = StreamKey::Namespace { name: "flotilla".into() };
    let encoded = serde_json::to_string(&key).unwrap();
    let decoded: StreamKey = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, key);
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-protocol --locked stream_key_namespace_round_trips`
Expected: FAIL — `StreamKey::Namespace` not defined.

- [ ] **Step 3: Add the variant**

In `crates/flotilla-protocol/src/lib.rs`, locate the `StreamKey` enum (around line 115) and add:

```rust
pub enum StreamKey {
    #[serde(rename = "repo")]
    Repo { identity: RepoIdentity },
    #[serde(rename = "host")]
    Host { environment_id: EnvironmentId },
    #[serde(rename = "namespace")]
    Namespace { name: String },
}
```

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-protocol --locked stream_key_namespace_round_trips`
Expected: PASS.

- [ ] **Step 5: Build the whole workspace to catch exhaustiveness errors**

Run: `cargo build --workspace --locked`
Expected: likely compilation errors wherever `StreamKey` is matched exhaustively. Find them via the compiler output. For each, add a match arm for `Namespace { .. }` that is functionally equivalent to how the other variants are handled. **Do not add stubbed behavior that silently drops the namespace case** — if a site treats repo/host identically, the namespace arm should do the same; if repo and host differ, use your judgment per-site, usually routing to a new "namespace-bound" branch that will be filled in later tasks.

For each compilation site, write a short TODO comment only if the follow-up is in a later task in this plan. Otherwise implement the behavior now.

- [ ] **Step 6: Run the protocol test suite**

Run: `cargo test -p flotilla-protocol --locked`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add -u
git commit -m "feat(protocol): add StreamKey::Namespace variant"
```

---

## Task 7: Add `DaemonEvent::NamespaceSnapshot` / `DaemonEvent::NamespaceDelta`

**Files:**
- Modify: `crates/flotilla-protocol/src/lib.rs`

- [ ] **Step 1: Write failing test**

Append to the protocol crate's test module:

```rust
#[test]
fn daemon_event_namespace_snapshot_round_trips() {
    use crate::namespace::NamespaceSnapshot;

    let event = DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![],
    }));
    let encoded = serde_json::to_string(&event).unwrap();
    let decoded: DaemonEvent = serde_json::from_str(&encoded).unwrap();
    match decoded {
        DaemonEvent::NamespaceSnapshot(snap) => {
            assert_eq!(snap.namespace, "flotilla");
            assert_eq!(snap.seq, 1);
        }
        other => panic!("expected NamespaceSnapshot, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-protocol --locked daemon_event_namespace_snapshot_round_trips`
Expected: FAIL — variants not defined.

- [ ] **Step 3: Add the variants**

In `crates/flotilla-protocol/src/lib.rs`'s `DaemonEvent` enum, add:

```rust
#[serde(rename = "namespace_snapshot")]
NamespaceSnapshot(Box<crate::namespace::NamespaceSnapshot>),
#[serde(rename = "namespace_delta")]
NamespaceDelta(Box<crate::namespace::NamespaceDelta>),
```

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-protocol --locked`
Expected: all pass.

- [ ] **Step 5: Build workspace, resolve exhaustiveness errors**

Run: `cargo build --workspace --locked`
For each match-on-`DaemonEvent` that now fails to compile, add an arm for `NamespaceSnapshot` / `NamespaceDelta`. If a site currently ignores unknown events, use `_ => {}`; if it routes by stream key, derive the stream key from the event's `namespace` field and route accordingly (matches the pattern for `RepoSnapshot` / `RepoDelta`).

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(protocol): add DaemonEvent::NamespaceSnapshot / NamespaceDelta"
```

---

## Task 8: Scaffold `ConvoyProjection` in the daemon

**Files:**
- Create: `crates/flotilla-daemon/src/convoy_projection.rs`
- Modify: `crates/flotilla-daemon/src/lib.rs`

- [ ] **Step 1: Create the projection module with skeleton struct**

```rust
// crates/flotilla-daemon/src/convoy_projection.rs
//
// Watches Convoy + Presentation resources and emits namespace-scoped
// snapshots and deltas for the TUI. Single-writer for the namespace
// stream seq counter.
//
// Spec: docs/superpowers/specs/2026-04-21-tui-convoy-view-design.md §Architecture.

use std::{collections::HashMap, sync::Arc};

use flotilla_protocol::{
    namespace::{ConvoyId, ConvoySummary, NamespaceDelta, NamespaceSnapshot},
    DaemonEvent,
};
use tokio::sync::mpsc;

/// In-memory view of one namespace's convoys, owned by the projection.
#[derive(Default)]
struct NamespaceView {
    convoys: HashMap<ConvoyId, ConvoySummary>,
    seq: u64,
}

pub struct ConvoyProjection {
    namespaces: HashMap<String, NamespaceView>,
    /// Emitter for events going to connected clients.
    event_tx: mpsc::Sender<DaemonEvent>,
}

impl ConvoyProjection {
    pub fn new(event_tx: mpsc::Sender<DaemonEvent>) -> Self {
        Self { namespaces: HashMap::new(), event_tx }
    }
}
```

- [ ] **Step 2: Wire the module into the crate**

In `crates/flotilla-daemon/src/lib.rs`, add:

```rust
mod convoy_projection;
pub use convoy_projection::ConvoyProjection;
```

- [ ] **Step 3: Build**

Run: `cargo build -p flotilla-daemon --locked`
Expected: compiles with unused-field warnings; `#[allow(dead_code)]` is fine transiently while we build up the module, but prefer adding the method that uses each field in the same commit wherever practical.

- [ ] **Step 4: Commit**

```bash
git add crates/flotilla-daemon/src/convoy_projection.rs crates/flotilla-daemon/src/lib.rs
git commit -m "feat(daemon): scaffold ConvoyProjection"
```

---

## Task 9: Translate `ConvoyStatus` into a `ConvoySummary`

**Files:**
- Modify: `crates/flotilla-daemon/src/convoy_projection.rs`

This task adds a pure function `summarize_convoy(resource: &Convoy) -> ConvoySummary` that translates a landed `Convoy` resource into the wire shape.

- [ ] **Step 1: Write failing test using an in-memory `Convoy` value**

In `convoy_projection.rs`, append a test module that constructs a `Convoy` resource directly (no I/O), calls `summarize_convoy`, and asserts the fields.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_resources::{
        convoy::{Convoy, ConvoyPhase as ResConvoyPhase, ConvoySpec, ConvoyStatus, SnapshotTask, TaskPhase as ResTaskPhase, TaskState, WorkflowSnapshot},
        resource::ObjectMeta,
    };

    fn meta(ns: &str, name: &str) -> ObjectMeta {
        ObjectMeta { name: name.into(), namespace: Some(ns.into()), ..Default::default() }
    }

    #[test]
    fn summarize_convoy_builds_full_summary_when_snapshot_present() {
        let convoy = Convoy {
            metadata: meta("flotilla", "fix-bug-123"),
            spec: ConvoySpec {
                workflow_ref: "review-and-fix".into(),
                ..Default::default()
            },
            status: ConvoyStatus {
                phase: ResConvoyPhase::Active,
                workflow_snapshot: Some(WorkflowSnapshot {
                    tasks: vec![SnapshotTask {
                        name: "implement".into(),
                        depends_on: vec![],
                        processes: vec![],
                    }],
                }),
                tasks: std::iter::once(("implement".into(), TaskState {
                    phase: ResTaskPhase::Running,
                    ..Default::default()
                })).collect(),
                observed_workflow_ref: Some("review-and-fix".into()),
                ..Default::default()
            },
        };
        let summary = summarize_convoy(&convoy);
        assert_eq!(summary.namespace, "flotilla");
        assert_eq!(summary.name, "fix-bug-123");
        assert_eq!(summary.workflow_ref, "review-and-fix");
        assert!(matches!(summary.phase, flotilla_protocol::namespace::ConvoyPhase::Active));
        assert!(!summary.initializing, "snapshot present → not initializing");
        assert_eq!(summary.tasks.len(), 1);
        assert_eq!(summary.tasks[0].name, "implement");
    }

    #[test]
    fn summarize_convoy_marks_initializing_when_snapshot_absent() {
        let convoy = Convoy {
            metadata: meta("flotilla", "new-one"),
            spec: ConvoySpec { workflow_ref: "wf".into(), ..Default::default() },
            status: ConvoyStatus {
                phase: ResConvoyPhase::Pending,
                workflow_snapshot: None,
                tasks: Default::default(),
                ..Default::default()
            },
        };
        let summary = summarize_convoy(&convoy);
        assert!(summary.initializing);
        assert!(summary.tasks.is_empty());
    }
}
```

Note: this test depends on `flotilla-daemon`'s `Cargo.toml` having `flotilla-resources` under `[dev-dependencies]` (it should already — verify). If not, add it.

- [ ] **Step 2: Run the test, confirm compile-fail (function not defined)**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests`
Expected: FAIL — `summarize_convoy` undefined.

- [ ] **Step 3: Implement `summarize_convoy`**

In `convoy_projection.rs`, add:

```rust
use flotilla_protocol::namespace::{
    ConvoyPhase as WireConvoyPhase, ConvoySummary, ProcessSummary, TaskPhase as WireTaskPhase, TaskSummary,
};
use flotilla_resources::convoy::{
    Convoy, ConvoyPhase as ResConvoyPhase, SnapshotTask, TaskPhase as ResTaskPhase, TaskState,
};

fn wire_convoy_phase(phase: ResConvoyPhase) -> WireConvoyPhase {
    match phase {
        ResConvoyPhase::Pending => WireConvoyPhase::Pending,
        ResConvoyPhase::Active => WireConvoyPhase::Active,
        ResConvoyPhase::Completed => WireConvoyPhase::Completed,
        ResConvoyPhase::Failed => WireConvoyPhase::Failed,
        ResConvoyPhase::Cancelled => WireConvoyPhase::Cancelled,
    }
}

fn wire_task_phase(phase: ResTaskPhase) -> WireTaskPhase {
    match phase {
        ResTaskPhase::Pending => WireTaskPhase::Pending,
        ResTaskPhase::Ready => WireTaskPhase::Ready,
        ResTaskPhase::Launching => WireTaskPhase::Launching,
        ResTaskPhase::Running => WireTaskPhase::Running,
        ResTaskPhase::Completed => WireTaskPhase::Completed,
        ResTaskPhase::Failed => WireTaskPhase::Failed,
        ResTaskPhase::Cancelled => WireTaskPhase::Cancelled,
    }
}

fn summarize_task(def: &SnapshotTask, state: Option<&TaskState>) -> TaskSummary {
    let phase = state.map(|s| wire_task_phase(s.phase)).unwrap_or(WireTaskPhase::Pending);
    TaskSummary {
        name: def.name.clone(),
        depends_on: def.depends_on.clone(),
        phase,
        processes: def.processes.iter().map(|p| ProcessSummary {
            role: p.role.clone(),
            // Keep command previews short — full definitions live on the resource.
            command_preview: p.command.clone().unwrap_or_default(),
        }).collect(),
        host: None,       // populated by placement in a future PR
        checkout: None,   // populated by placement in a future PR
        workspace_ref: None, // populated via Presentation index in Task 10
        ready_at: state.and_then(|s| s.ready_at),
        started_at: state.and_then(|s| s.started_at),
        finished_at: state.and_then(|s| s.finished_at),
        message: state.and_then(|s| s.message.clone()),
    }
}

pub(crate) fn summarize_convoy(convoy: &Convoy) -> ConvoySummary {
    let namespace = convoy.metadata.namespace.clone().unwrap_or_default();
    let name = convoy.metadata.name.clone();
    let id = ConvoyId::new(&namespace, &name);

    let tasks: Vec<TaskSummary> = convoy.status.workflow_snapshot
        .as_ref()
        .map(|snap| snap.tasks.iter().map(|t| summarize_task(t, convoy.status.tasks.get(&t.name))).collect())
        .unwrap_or_default();

    ConvoySummary {
        id,
        namespace: namespace.clone(),
        name,
        workflow_ref: convoy.spec.workflow_ref.clone(),
        phase: wire_convoy_phase(convoy.status.phase),
        message: convoy.status.message.clone(),
        repo_hint: None, // label lookup added in Task 11
        tasks,
        started_at: convoy.status.started_at,
        finished_at: convoy.status.finished_at,
        observed_workflow_ref: convoy.status.observed_workflow_ref.clone(),
        initializing: convoy.status.workflow_snapshot.is_none(),
    }
}
```

If `ProcessDefinition` uses a field name other than `command` for the literal command string (check `crates/flotilla-resources/src/workflow_template.rs`), adjust the `command_preview` assignment to use whichever field is present. For a `selector`-only process with no command, use the role as the preview or leave empty.

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests`
Expected: both tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/convoy_projection.rs
git commit -m "feat(daemon): summarize_convoy translates resource → wire shape"
```

---

## Task 10: Projection maintains a `(convoy, task) → workspace_ref` index

**Files:**
- Modify: `crates/flotilla-daemon/src/convoy_projection.rs`

- [ ] **Step 1: Write failing test**

Append to `tests`:

```rust
use flotilla_resources::{labels::{CONVOY_LABEL, TASK_LABEL}, presentation::{Presentation, PresentationSpec, PresentationStatus}};

fn presentation(convoy_name: &str, task_name: &str, ws_ref: Option<&str>) -> Presentation {
    let mut labels = std::collections::BTreeMap::new();
    labels.insert(CONVOY_LABEL.into(), convoy_name.into());
    labels.insert(TASK_LABEL.into(), task_name.into());
    Presentation {
        metadata: ObjectMeta {
            name: format!("{convoy_name}-{task_name}"),
            namespace: Some("flotilla".into()),
            labels,
            ..Default::default()
        },
        spec: PresentationSpec {
            convoy_ref: convoy_name.into(),
            presentation_policy_ref: "default".into(),
            name: task_name.into(),
            process_selector: Default::default(),
        },
        status: PresentationStatus {
            observed_workspace_ref: ws_ref.map(str::to_string),
            ..Default::default()
        },
    }
}

#[test]
fn presentation_index_resolves_workspace_ref_per_task() {
    let mut projection = ConvoyProjection::new(mpsc::channel(16).0);
    projection.apply_presentation(&presentation("fix-bug-123", "implement", Some("ws-1")));
    projection.apply_presentation(&presentation("fix-bug-123", "review", Some("ws-2")));

    assert_eq!(
        projection.workspace_ref_for("flotilla", "fix-bug-123", "implement"),
        Some("ws-1".to_string())
    );
    assert_eq!(
        projection.workspace_ref_for("flotilla", "fix-bug-123", "review"),
        Some("ws-2".to_string())
    );
}

#[test]
fn presentation_without_task_label_is_ignored() {
    let mut projection = ConvoyProjection::new(mpsc::channel(16).0);
    let mut p = presentation("fix-bug-123", "implement", Some("ws-1"));
    p.metadata.labels.remove(TASK_LABEL);
    projection.apply_presentation(&p);
    assert_eq!(
        projection.workspace_ref_for("flotilla", "fix-bug-123", "implement"),
        None,
        "convoy-level Presentations do not resolve per-task — addendum prerequisite"
    );
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests::presentation_index`
Expected: FAIL — methods not defined.

- [ ] **Step 3: Implement the index**

In the `ConvoyProjection` impl block:

```rust
use flotilla_resources::{labels::{CONVOY_LABEL, TASK_LABEL}, presentation::Presentation};

/// Key: (namespace, convoy_name, task_name).
type PresentationKey = (String, String, String);

// In NamespaceView or a separate field on ConvoyProjection:
#[derive(Default)]
pub struct ConvoyProjection {
    namespaces: HashMap<String, NamespaceView>,
    presentation_workspaces: HashMap<PresentationKey, String>,
    event_tx: mpsc::Sender<DaemonEvent>,
}

impl ConvoyProjection {
    pub fn apply_presentation(&mut self, p: &Presentation) {
        let namespace = p.metadata.namespace.clone().unwrap_or_default();
        let convoy = match p.metadata.labels.get(CONVOY_LABEL) {
            Some(v) => v.clone(),
            None => return,
        };
        let task = match p.metadata.labels.get(TASK_LABEL) {
            Some(v) => v.clone(),
            None => return, // convoy-level presentation; not our concern per addendum
        };
        match &p.status.observed_workspace_ref {
            Some(ws_ref) => {
                self.presentation_workspaces.insert((namespace, convoy, task), ws_ref.clone());
            }
            None => {
                self.presentation_workspaces.remove(&(namespace, convoy, task));
            }
        }
    }

    pub fn workspace_ref_for(&self, namespace: &str, convoy: &str, task: &str) -> Option<String> {
        self.presentation_workspaces
            .get(&(namespace.to_owned(), convoy.to_owned(), task.to_owned()))
            .cloned()
    }
}
```

Update the `ConvoyProjection::new` to match the new field set.

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/convoy_projection.rs
git commit -m "feat(daemon): projection indexes per-task presentation workspace_ref"
```

---

## Task 11: Projection enriches `ConvoySummary.tasks[].workspace_ref` from the index; populates `repo_hint` from labels

**Files:**
- Modify: `crates/flotilla-daemon/src/convoy_projection.rs`

- [ ] **Step 1: Write failing test**

Append:

```rust
#[test]
fn summarize_with_index_populates_workspace_ref() {
    let mut projection = ConvoyProjection::new(mpsc::channel(16).0);
    projection.apply_presentation(&presentation("fix-bug-123", "implement", Some("ws-1")));

    let convoy = Convoy {
        metadata: meta("flotilla", "fix-bug-123"),
        spec: ConvoySpec { workflow_ref: "wf".into(), ..Default::default() },
        status: ConvoyStatus {
            phase: ResConvoyPhase::Active,
            workflow_snapshot: Some(WorkflowSnapshot {
                tasks: vec![SnapshotTask { name: "implement".into(), depends_on: vec![], processes: vec![] }],
            }),
            tasks: std::iter::once(("implement".into(), TaskState { phase: ResTaskPhase::Running, ..Default::default() })).collect(),
            ..Default::default()
        },
    };
    let summary = projection.summarize(&convoy);
    assert_eq!(summary.tasks[0].workspace_ref.as_deref(), Some("ws-1"));
}

#[test]
fn summarize_populates_repo_hint_from_label() {
    use flotilla_resources::labels; // locate the repo label constant; introduce if needed (see Step 3)
    let mut projection = ConvoyProjection::new(mpsc::channel(16).0);
    let mut convoy = Convoy {
        metadata: meta("flotilla", "x"),
        spec: ConvoySpec { workflow_ref: "wf".into(), ..Default::default() },
        status: Default::default(),
    };
    convoy.metadata.labels.insert(labels::REPO_LABEL.into(), "flotilla-org/flotilla".into());
    let summary = projection.summarize(&convoy);
    assert_eq!(summary.repo_hint.as_ref().map(|r| r.0.as_str()), Some("flotilla-org/flotilla"));
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests::summarize_with_index_populates_workspace_ref convoy_projection::tests::summarize_populates_repo_hint_from_label`
Expected: FAIL.

- [ ] **Step 3: Implement**

First, ensure a `REPO_LABEL` constant exists in `crates/flotilla-resources/src/labels.rs`. If not, add:

```rust
pub const REPO_LABEL: &str = "flotilla.work/repo";
```

Then convert the free `summarize_convoy` function into an instance method `ConvoyProjection::summarize` that consults the presentation index and labels:

```rust
impl ConvoyProjection {
    pub fn summarize(&self, convoy: &Convoy) -> ConvoySummary {
        let mut summary = summarize_convoy(convoy); // internal helper stays
        // Fill workspace_ref per task from the presentation index
        for task in summary.tasks.iter_mut() {
            task.workspace_ref = self.workspace_ref_for(&summary.namespace, &summary.name, &task.name);
        }
        // Populate repo_hint from the convoy's REPO_LABEL, if present
        if let Some(repo) = convoy.metadata.labels.get(flotilla_resources::labels::REPO_LABEL) {
            summary.repo_hint = Some(flotilla_protocol::snapshot::RepoKey(repo.clone()));
        }
        summary
    }
}
```

Keep the free `summarize_convoy` private; it's the zero-context translation. Only `ConvoyProjection::summarize` is the public entry point.

- [ ] **Step 4: Update the existing `summarize_convoy_builds_full_summary_when_snapshot_present` test to use `ConvoyProjection::new(...).summarize(&convoy)` instead of the free function, so the two API surfaces don't drift**

Adjust assertions to match the new path. Confirm other existing tests still pass.

- [ ] **Step 5: Run, confirm pass**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(daemon): enrich ConvoySummary with workspace_ref and repo_hint"
```

---

## Task 12: Projection applies events (ADDED/MODIFIED/DELETED) and emits snapshots/deltas

**Files:**
- Modify: `crates/flotilla-daemon/src/convoy_projection.rs`

- [ ] **Step 1: Write failing test**

Append:

```rust
use flotilla_resources::watch::WatchEvent;
use tokio::sync::mpsc;

async fn drain(rx: &mut mpsc::Receiver<DaemonEvent>) -> Vec<DaemonEvent> {
    let mut out = Vec::new();
    while let Ok(event) = rx.try_recv() {
        out.push(event);
    }
    out
}

#[tokio::test]
async fn applying_convoy_added_emits_initial_snapshot_then_delta() {
    let (tx, mut rx) = mpsc::channel(16);
    let mut projection = ConvoyProjection::new(tx);

    // First apply emits a NamespaceSnapshot covering the new convoy.
    let convoy = Convoy {
        metadata: meta("flotilla", "x"),
        spec: ConvoySpec { workflow_ref: "wf".into(), ..Default::default() },
        status: ConvoyStatus { phase: ResConvoyPhase::Pending, ..Default::default() },
    };
    projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;

    let events = drain(&mut rx).await;
    assert_eq!(events.len(), 1, "first event per namespace emits a snapshot, got {events:?}");
    match &events[0] {
        DaemonEvent::NamespaceSnapshot(snap) => {
            assert_eq!(snap.namespace, "flotilla");
            assert_eq!(snap.convoys.len(), 1);
            assert_eq!(snap.seq, 1);
        }
        other => panic!("expected NamespaceSnapshot, got {other:?}"),
    }

    // Second apply (modification) emits a NamespaceDelta only.
    let mut modified = convoy;
    modified.status.phase = ResConvoyPhase::Active;
    projection.apply_convoy_event(WatchEvent::Modified(modified)).await;

    let events = drain(&mut rx).await;
    assert_eq!(events.len(), 1);
    match &events[0] {
        DaemonEvent::NamespaceDelta(delta) => {
            assert_eq!(delta.changed.len(), 1);
            assert!(delta.removed.is_empty());
            assert_eq!(delta.seq, 2);
        }
        other => panic!("expected NamespaceDelta, got {other:?}"),
    }
}

#[tokio::test]
async fn applying_convoy_deleted_emits_removal_delta() {
    let (tx, mut rx) = mpsc::channel(16);
    let mut projection = ConvoyProjection::new(tx);
    let convoy = Convoy {
        metadata: meta("flotilla", "x"),
        spec: ConvoySpec { workflow_ref: "wf".into(), ..Default::default() },
        status: Default::default(),
    };
    projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
    let _ = drain(&mut rx).await; // consume snapshot

    projection.apply_convoy_event(WatchEvent::Deleted(convoy)).await;
    let events = drain(&mut rx).await;
    match &events[0] {
        DaemonEvent::NamespaceDelta(delta) => {
            assert!(delta.changed.is_empty());
            assert_eq!(delta.removed.len(), 1);
        }
        other => panic!("expected NamespaceDelta, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests::applying_convoy`
Expected: FAIL.

- [ ] **Step 3: Implement `apply_convoy_event`**

```rust
impl ConvoyProjection {
    pub async fn apply_convoy_event(&mut self, event: WatchEvent<Convoy>) {
        match event {
            WatchEvent::Added(convoy) | WatchEvent::Modified(convoy) => {
                let summary = self.summarize(&convoy);
                let namespace = summary.namespace.clone();
                let id = summary.id.clone();
                let view = self.namespaces.entry(namespace.clone()).or_default();
                let is_first_event_for_namespace = view.convoys.is_empty() && view.seq == 0;
                view.convoys.insert(id, summary.clone());
                view.seq = view.seq.saturating_add(1);

                let event = if is_first_event_for_namespace {
                    DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
                        seq: view.seq,
                        namespace: namespace.clone(),
                        convoys: view.convoys.values().cloned().collect(),
                    }))
                } else {
                    DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
                        seq: view.seq,
                        namespace,
                        changed: vec![summary],
                        removed: Vec::new(),
                    }))
                };
                let _ = self.event_tx.send(event).await;
            }
            WatchEvent::Deleted(convoy) => {
                let namespace = convoy.metadata.namespace.clone().unwrap_or_default();
                let name = convoy.metadata.name.clone();
                let id = ConvoyId::new(&namespace, &name);
                if let Some(view) = self.namespaces.get_mut(&namespace) {
                    if view.convoys.remove(&id).is_some() {
                        view.seq = view.seq.saturating_add(1);
                        let event = DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
                            seq: view.seq,
                            namespace,
                            changed: Vec::new(),
                            removed: vec![id],
                        }));
                        let _ = self.event_tx.send(event).await;
                    }
                }
            }
        }
    }
}
```

The `is_first_event_for_namespace` detection is simple-minded; prefer a per-namespace `emitted_initial_snapshot: bool` flag if that field name reads more clearly. The goal is: one `NamespaceSnapshot` the first time a namespace shows up, deltas thereafter.

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/convoy_projection.rs
git commit -m "feat(daemon): ConvoyProjection applies events and emits snapshots/deltas"
```

---

## Task 13: Projection handles `Presentation` events (update task's `workspace_ref`, emit delta for affected convoy)

**Files:**
- Modify: `crates/flotilla-daemon/src/convoy_projection.rs`

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn presentation_update_refreshes_workspace_ref_on_affected_convoy() {
    let (tx, mut rx) = mpsc::channel(16);
    let mut projection = ConvoyProjection::new(tx);

    let convoy = Convoy {
        metadata: meta("flotilla", "fix-bug-123"),
        spec: ConvoySpec { workflow_ref: "wf".into(), ..Default::default() },
        status: ConvoyStatus {
            phase: ResConvoyPhase::Active,
            workflow_snapshot: Some(WorkflowSnapshot {
                tasks: vec![SnapshotTask { name: "implement".into(), depends_on: vec![], processes: vec![] }],
            }),
            tasks: std::iter::once(("implement".into(), TaskState { phase: ResTaskPhase::Running, ..Default::default() })).collect(),
            ..Default::default()
        },
    };
    projection.apply_convoy_event(WatchEvent::Added(convoy.clone())).await;
    let _ = drain(&mut rx).await;

    let p = presentation("fix-bug-123", "implement", Some("ws-1"));
    projection.apply_presentation_event(WatchEvent::Added(p)).await;

    let events = drain(&mut rx).await;
    match &events[0] {
        DaemonEvent::NamespaceDelta(delta) => {
            let task = &delta.changed[0].tasks[0];
            assert_eq!(task.workspace_ref.as_deref(), Some("ws-1"));
        }
        other => panic!("expected NamespaceDelta, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests::presentation_update_refreshes_workspace_ref_on_affected_convoy`
Expected: FAIL.

- [ ] **Step 3: Implement `apply_presentation_event`**

```rust
impl ConvoyProjection {
    pub async fn apply_presentation_event(&mut self, event: WatchEvent<Presentation>) {
        // 1. Update the presentation index
        let (namespace, convoy_name) = match &event {
            WatchEvent::Added(p) | WatchEvent::Modified(p) => {
                self.apply_presentation(p);
                (
                    p.metadata.namespace.clone().unwrap_or_default(),
                    p.metadata.labels.get(CONVOY_LABEL).cloned().unwrap_or_default(),
                )
            }
            WatchEvent::Deleted(p) => {
                // Remove from index
                let ns = p.metadata.namespace.clone().unwrap_or_default();
                let convoy = p.metadata.labels.get(CONVOY_LABEL).cloned().unwrap_or_default();
                let task = p.metadata.labels.get(TASK_LABEL).cloned().unwrap_or_default();
                if !convoy.is_empty() && !task.is_empty() {
                    self.presentation_workspaces.remove(&(ns.clone(), convoy.clone(), task));
                }
                (ns, convoy)
            }
        };

        if convoy_name.is_empty() {
            return;
        }

        // 2. Re-emit a delta for the affected convoy
        let id = ConvoyId::new(&namespace, &convoy_name);
        let Some(view) = self.namespaces.get_mut(&namespace) else { return };
        let Some(existing) = view.convoys.get(&id).cloned() else { return };

        // Rebuild the task summaries with fresh workspace_ref lookups
        let mut refreshed = existing;
        for task in refreshed.tasks.iter_mut() {
            task.workspace_ref = self.workspace_ref_for(&namespace, &convoy_name, &task.name);
        }
        view.convoys.insert(id.clone(), refreshed.clone());
        view.seq = view.seq.saturating_add(1);

        let _ = self.event_tx.send(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
            seq: view.seq,
            namespace,
            changed: vec![refreshed],
            removed: Vec::new(),
        }))).await;
    }
}
```

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests::presentation_update_refreshes_workspace_ref_on_affected_convoy`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/convoy_projection.rs
git commit -m "feat(daemon): projection re-emits affected convoy when Presentation changes"
```

---

## Task 14: Projection has a `run()` loop that consumes watch streams

**Files:**
- Modify: `crates/flotilla-daemon/src/convoy_projection.rs`

- [ ] **Step 1: Write failing test using an `InMemoryResourceClient`-backed watch stream**

This test uses the in-memory resource client from stage 1. Find its path: `rg "InMemoryResourceClient" crates/flotilla-resources/src/in_memory.rs -n | head`. Construct a client, call `projection.run(...)` in a background task, trigger a resource change, assert events arrive on the mpsc receiver.

```rust
#[tokio::test]
async fn run_loop_consumes_in_memory_client_events() {
    use flotilla_resources::in_memory::InMemoryResourceClient;
    let client = InMemoryResourceClient::new();
    let (tx, mut rx) = mpsc::channel(16);
    let projection = ConvoyProjection::new(tx);

    // Spawn run loop. Clients for convoy and presentation share the same backend.
    let convoys = client.using::<Convoy>("flotilla");
    let presentations = client.using::<Presentation>("flotilla");
    let handle = tokio::spawn(async move {
        projection.run(convoys, presentations).await
    });

    // Apply a convoy to the backend
    let convoy = Convoy {
        metadata: meta("flotilla", "x"),
        spec: ConvoySpec { workflow_ref: "wf".into(), ..Default::default() },
        status: Default::default(),
    };
    client.using::<Convoy>("flotilla").create(&convoy).await.unwrap();

    // Expect a NamespaceSnapshot event within a short timeout
    let event = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for event")
        .expect("sender dropped");
    assert!(matches!(event, DaemonEvent::NamespaceSnapshot(_)));

    handle.abort();
}
```

Exact resolver API method names may differ — consult the stage 1 prototype. Adjust accordingly; the assertion shape is the contract.

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests::run_loop_consumes_in_memory_client_events`
Expected: FAIL — `ConvoyProjection::run` not defined.

- [ ] **Step 3: Implement `run`**

```rust
impl ConvoyProjection {
    pub async fn run<CW, PW>(mut self, convoys: CW, presentations: PW)
    where
        CW: flotilla_resources::ResourceClient<Convoy>,
        PW: flotilla_resources::ResourceClient<Presentation>,
    {
        use futures::StreamExt;
        let mut convoy_stream = convoys.watch(flotilla_resources::WatchStart::Current).await.expect("start convoy watch");
        let mut presentation_stream = presentations.watch(flotilla_resources::WatchStart::Current).await.expect("start presentation watch");

        loop {
            tokio::select! {
                Some(Ok(event)) = convoy_stream.next() => {
                    self.apply_convoy_event(event).await;
                }
                Some(Ok(event)) = presentation_stream.next() => {
                    self.apply_presentation_event(event).await;
                }
                else => break,
            }
        }
    }
}
```

Adjust trait names / bounds to the actual names used by the stage-1 prototype. If `WatchStart::Current` is named differently (e.g. `WatchStart::Now`), use the real one.

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-daemon --locked convoy_projection::tests::run_loop_consumes_in_memory_client_events`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/flotilla-daemon/src/convoy_projection.rs
git commit -m "feat(daemon): ConvoyProjection::run drives event loop"
```

---

## Task 15: Wire `ConvoyProjection` into daemon runtime startup

**Files:**
- Modify: `crates/flotilla-daemon/src/runtime.rs`

- [ ] **Step 1: Read the existing runtime startup block to understand the spawn pattern**

Run: `rg -n "ConvoyReconciler|tokio::spawn" crates/flotilla-daemon/src/runtime.rs | head`. Read ~40 lines around the ConvoyReconciler spawn and note how existing long-running controllers are launched.

- [ ] **Step 2: Write a new test (or extend an existing in-process daemon integration test) that asserts: after creating a Convoy resource via the backend, the daemon's event stream delivers a `DaemonEvent::NamespaceSnapshot` to subscribed clients.**

Locate `crates/flotilla-daemon/tests/` or the test-support harness (`rg "InProcessDaemon" -l`). The integration test pattern likely exists for `RepoSnapshot`. Mirror it for namespaces.

Add a test `convoy_projection_emits_namespace_events` in the most appropriate existing test file (or a new file `crates/flotilla-daemon/tests/convoy_projection.rs`). Use the existing in-process daemon + resource client fixtures.

- [ ] **Step 3: Run test, confirm fail**

Run: `cargo test -p flotilla-daemon --locked convoy_projection_emits_namespace_events`
Expected: FAIL — projection not yet wired.

- [ ] **Step 4: Wire the projection into `runtime.rs` startup**

In the daemon startup path that already constructs the convoy backend + `ConvoyReconciler`, also spawn the projection. Use the same mpsc sender used by other event emitters:

```rust
let convoy_client = backend.clone().using::<Convoy>(&namespace_string);
let presentation_client = backend.clone().using::<Presentation>(&namespace_string);
let projection = ConvoyProjection::new(event_tx.clone());
tokio::spawn(projection.run(convoy_client, presentation_client));
```

Place it right after the existing `ConvoyReconciler` spawn so they co-live. Name `namespace_string` matches the existing variable if one exists; otherwise introduce a local binding.

- [ ] **Step 5: Run, confirm pass**

Run: `cargo test -p flotilla-daemon --locked convoy_projection_emits_namespace_events`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(daemon): spawn ConvoyProjection at runtime startup"
```

---

## Task 16: Server dispatch includes namespace events in `ReplaySince`

**Files:**
- Modify: `crates/flotilla-daemon/src/server/request_dispatch.rs`

- [ ] **Step 1: Read the existing `ReplaySince` request handling to locate where repo/host events are filtered + re-emitted**

Run: `rg -n "ReplaySince|replay_since" crates/flotilla-daemon/src/server/ | head`.

- [ ] **Step 2: Write a failing integration test**

First, find an existing `ReplaySince` test to use as a pattern. Run:

```bash
rg -l "Request::ReplaySince|ReplayCursor" crates/ | grep -E 'tests?' | head -5
```

Read one of the repo-stream replay tests and mirror its structure for the namespace case. The test must:

1. Construct an `InProcessDaemon` with the convoy projection running.
2. Connect a client (`SocketDaemon` or the test-support handle equivalent) and consume the initial `NamespaceSnapshot` for namespace `"flotilla"`, recording its `seq`.
3. Issue a `Request::ReplaySince` with a single `ReplayCursor { stream: StreamKey::Namespace { name: "flotilla".into() }, seq: recorded_seq }`.
4. Create another convoy via the backend *before* sending the replay request, so there is a delta in the server's retention buffer to replay.
5. Assert the response contains at least one `DaemonEvent::NamespaceDelta` with `namespace == "flotilla"`.

If no ReplaySince test exists to model on, read the repo-stream replay logic in `crates/flotilla-daemon/src/server/request_dispatch.rs` and build the test from scratch using the `InProcessDaemon` fixtures in `crates/flotilla-core/tests/in_process_daemon.rs`.

- [ ] **Step 3: Run, confirm fail**

Run the test.
Expected: FAIL — server likely ignores namespace cursor.

- [ ] **Step 4: Implement**

Within the server's replay-since machinery, extend the filter that selects events to include for a given cursor. The existing code likely has a match like:

```rust
match (event, cursor_key) {
    (DaemonEvent::RepoSnapshot(snap), StreamKey::Repo { identity }) if &snap.repo_identity == identity => include,
    (DaemonEvent::HostSnapshot(snap), StreamKey::Host { environment_id }) if &snap.environment_id == environment_id => include,
    ...
}
```

Add cases:

```rust
(DaemonEvent::NamespaceSnapshot(snap), StreamKey::Namespace { name }) if &snap.namespace == name => include,
(DaemonEvent::NamespaceDelta(delta), StreamKey::Namespace { name }) if &delta.namespace == name => include,
```

Follow whatever pattern the existing code uses for seq-gap handling (full snapshot on gap, delta on continuity).

- [ ] **Step 5: Run, confirm pass**

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(daemon): include namespace events in ReplaySince response"
```

---

## Task 17: Client tracks per-namespace seq + sends namespace cursors on replay

**Files:**
- Modify: `crates/flotilla-client/src/lib.rs`

- [ ] **Step 1: Read the existing repo/host seq tracking in `flotilla-client/src/lib.rs` (around lines 454, 563, 693 per the spec) to understand the pattern.**

- [ ] **Step 2: Write a failing test**

Add (or extend) a test in `crates/flotilla-client/tests/` that connects to an in-memory daemon, receives a `NamespaceSnapshot`, simulates a seq gap, calls `ReplaySince`, and asserts the namespace cursor was included in the request.

- [ ] **Step 3: Run, confirm fail**

- [ ] **Step 4: Implement**

For each of the three cited regions, extend the match with the namespace case:

```rust
// Around line 468 (event reception):
DaemonEvent::NamespaceSnapshot(snap) => {
    local_seqs.write().unwrap().insert(StreamKey::Namespace { name: snap.namespace.clone() }, snap.seq);
    // forward to consumer
}
DaemonEvent::NamespaceDelta(delta) => {
    let key = StreamKey::Namespace { name: delta.namespace.clone() };
    // forward; update seq after successful consumer delivery, mirroring repo delta path
}

// Around line 693 (replay_since response handling):
DaemonEvent::NamespaceSnapshot(snap) => (StreamKey::Namespace { name: snap.namespace.clone() }, snap.seq),
DaemonEvent::NamespaceDelta(delta) => (StreamKey::Namespace { name: delta.namespace.clone() }, delta.seq),
```

Follow the exact idiom used by the repo/host arms: same locking discipline, same removal-on-gap semantics, same logging style.

- [ ] **Step 5: Run, confirm pass**

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(client): track namespace stream seq for replay/gap recovery"
```

---

## Task 18: CLI `watch` command handles namespace events

**Files:**
- Modify: `crates/flotilla-tui/src/cli.rs`

- [ ] **Step 1: Read the watch CLI code around lines 381, 516, 539**

Note the existing dedupe helpers and the print-formatters for repo/host events.

- [ ] **Step 2: Write a failing integration test**

Use `flotilla-tui/tests/` if it exists, or `crates/flotilla-tui/src/cli.rs`'s inline test mod. The test simulates two back-to-back namespace events (one pre-gap snapshot, one replayed delta with overlapping seq) and asserts the print output contains the delta exactly once.

- [ ] **Step 3: Run, confirm fail**

- [ ] **Step 4: Implement**

In the watch command's event-processing loop, add handling for `DaemonEvent::NamespaceSnapshot` and `DaemonEvent::NamespaceDelta`:

```rust
DaemonEvent::NamespaceSnapshot(snap) => {
    if !dedupe.insert(StreamKey::Namespace { name: snap.namespace.clone() }, snap.seq) {
        return; // already seen
    }
    println!("[namespace/{}] snapshot seq={} convoys={}", snap.namespace, snap.seq, snap.convoys.len());
}
DaemonEvent::NamespaceDelta(delta) => {
    if !dedupe.insert(StreamKey::Namespace { name: delta.namespace.clone() }, delta.seq) {
        return;
    }
    println!("[namespace/{}] delta seq={} changed={} removed={}", delta.namespace, delta.seq, delta.changed.len(), delta.removed.len());
}
```

Match the existing CLI's formatting conventions exactly (quoting, separators).

- [ ] **Step 5: Run, confirm pass**

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(cli): watch handles namespace events with dedupe"
```

---

## Task 19: Add `tui-tree-widget` dependency

**Files:**
- Modify: `crates/flotilla-tui/Cargo.toml`

- [ ] **Step 1: Add dependency**

```toml
# In the [dependencies] block of crates/flotilla-tui/Cargo.toml:
tui-tree-widget = "0.24"
```

- [ ] **Step 2: Build to confirm resolution**

Run: `cargo build -p flotilla-tui --locked`
Expected: resolves and builds.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-tui/Cargo.toml Cargo.lock
git commit -m "chore(tui): add tui-tree-widget dependency"
```

---

## Task 20: Add `TabId::Convoys` and `BindingModeId::Convoys`

**Files:**
- Modify: `crates/flotilla-tui/src/app/ui_state.rs`
- Modify: `crates/flotilla-tui/src/binding_table.rs`

- [ ] **Step 1: Write failing test for tab-cycling across the new tab**

Add to `crates/flotilla-tui/src/widgets/tabs.rs` tests (if the module has them) or `app/tests.rs`:

```rust
#[test]
fn next_tab_cycles_through_convoys_tab() {
    use crate::app::ui_state::TabId;
    let tabs = [TabId::Flotilla, TabId::Convoys, TabId::Repo(0), TabId::Add];
    // Assert that the sequence cycles correctly (details depend on existing cycling helper).
}
```

If no cycling helper exists as a pure function, put the logic in one when you touch it in Step 3 and test through it.

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-tui --locked next_tab_cycles_through_convoys_tab`
Expected: FAIL — `TabId::Convoys` does not exist.

- [ ] **Step 3: Add `TabId::Convoys`**

In `crates/flotilla-tui/src/app/ui_state.rs`, extend the enum:

```rust
pub enum TabId {
    Flotilla,
    Convoys,          // NEW — global convoy view, always present
    Repo(usize),
    Add,
}
```

Then fix compilation across exhaustive matches. For most sites, `Convoys` behaves like `Flotilla` (a global-scope tab, not repo-scoped).

- [ ] **Step 4: Add `BindingModeId::Convoys`**

In `crates/flotilla-tui/src/binding_table.rs`, extend the enum:

```rust
pub enum BindingModeId {
    Shared,
    Normal,
    Overview,
    Convoys,            // NEW
    Help,
    ActionMenu,
    // ... existing variants
}
```

Add the convoy-tab bindings in the binding declarations block:

```rust
h(BindingModeId::Convoys, "j", Action::SelectNext, "Down"),
h(BindingModeId::Convoys, "k", Action::SelectPrev, "Up"),
h(BindingModeId::Convoys, "l", Action::Confirm, "Focus"),    // focus detail / expand tree node
h(BindingModeId::Convoys, "enter", Action::Confirm, "Focus"),
h(BindingModeId::Convoys, "h", Action::Dismiss, "Back"),      // focus list / collapse tree node
h(BindingModeId::Convoys, "esc", Action::Dismiss, "Back"),
h(BindingModeId::Convoys, "[", Action::PrevTab, "Prev"),
h(BindingModeId::Convoys, "]", Action::NextTab, "Next"),
h(BindingModeId::Convoys, "q", Action::Quit, "Quit"),
h(BindingModeId::Convoys, "r", Action::Refresh, "Refresh"),
h(BindingModeId::Convoys, "/", Action::OpenCommandPalette, "Filter"),
```

- [ ] **Step 5: Run, confirm pass**

Run: `cargo test -p flotilla-tui --locked`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(tui): add TabId::Convoys and BindingModeId::Convoys"
```

---

## Task 21: AppModel stores and applies namespace state

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs`

- [ ] **Step 1: Write failing test**

In `crates/flotilla-tui/src/app/tests.rs`:

```rust
#[test]
fn app_applies_namespace_snapshot() {
    use flotilla_protocol::{namespace::{ConvoyId, ConvoySummary, ConvoyPhase, NamespaceSnapshot}, DaemonEvent};
    let mut app = App::new_for_tests();
    let convoy = ConvoySummary {
        id: ConvoyId::new("flotilla", "x"),
        namespace: "flotilla".into(),
        name: "x".into(),
        workflow_ref: "wf".into(),
        phase: ConvoyPhase::Active,
        message: None,
        repo_hint: None,
        tasks: vec![],
        started_at: None,
        finished_at: None,
        observed_workflow_ref: None,
        initializing: false,
    };
    app.handle_event(DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![convoy.clone()],
    })));
    assert_eq!(app.convoys("flotilla").len(), 1);
    assert_eq!(app.convoys("flotilla")[0].name, "x");
}

#[test]
fn app_applies_namespace_delta() {
    use flotilla_protocol::{namespace::{ConvoyId, ConvoySummary, ConvoyPhase, NamespaceDelta, NamespaceSnapshot}, DaemonEvent};
    let mut app = App::new_for_tests();
    let convoy = ConvoySummary {
        id: ConvoyId::new("flotilla", "x"),
        namespace: "flotilla".into(),
        name: "x".into(),
        workflow_ref: "wf".into(),
        phase: ConvoyPhase::Pending,
        message: None,
        repo_hint: None,
        tasks: vec![],
        started_at: None,
        finished_at: None,
        observed_workflow_ref: None,
        initializing: true,
    };

    // Seed with initial snapshot
    app.handle_event(DaemonEvent::NamespaceSnapshot(Box::new(NamespaceSnapshot {
        seq: 1,
        namespace: "flotilla".into(),
        convoys: vec![convoy.clone()],
    })));

    // Apply a delta that modifies the convoy to Active
    let mut modified = convoy.clone();
    modified.phase = ConvoyPhase::Active;
    modified.initializing = false;
    app.handle_event(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
        seq: 2,
        namespace: "flotilla".into(),
        changed: vec![modified.clone()],
        removed: vec![],
    })));
    assert_eq!(app.convoys("flotilla")[0].phase, ConvoyPhase::Active);

    // Apply a delta that removes the convoy
    app.handle_event(DaemonEvent::NamespaceDelta(Box::new(NamespaceDelta {
        seq: 3,
        namespace: "flotilla".into(),
        changed: vec![],
        removed: vec![convoy.id.clone()],
    })));
    assert!(app.convoys("flotilla").is_empty());
}
```

Use the existing `App` builder/test-support pattern (`App::new_for_tests` or equivalent — search for how existing event tests construct an app).

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-tui --locked app_applies_namespace_snapshot`
Expected: FAIL — methods not defined.

- [ ] **Step 3: Implement**

Add to `App` or its associated model struct:

```rust
// On App (or wherever state lives):
pub struct NamespaceModel {
    pub convoys: indexmap::IndexMap<ConvoyId, ConvoySummary>,
    pub last_seq: u64,
}

// Field on App:
pub namespaces: std::collections::HashMap<String, NamespaceModel>,
```

`handle_event` match arms:

```rust
DaemonEvent::NamespaceSnapshot(snap) => {
    let entry = self.namespaces.entry(snap.namespace.clone()).or_default();
    entry.convoys.clear();
    for convoy in snap.convoys.iter() {
        entry.convoys.insert(convoy.id.clone(), convoy.clone());
    }
    entry.last_seq = snap.seq;
}
DaemonEvent::NamespaceDelta(delta) => {
    let entry = self.namespaces.entry(delta.namespace.clone()).or_default();
    for convoy in delta.changed.iter() {
        entry.convoys.insert(convoy.id.clone(), convoy.clone());
    }
    for id in delta.removed.iter() {
        entry.convoys.shift_remove(id);
    }
    entry.last_seq = delta.seq;
}
```

And an accessor:

```rust
pub fn convoys(&self, namespace: &str) -> Vec<&ConvoySummary> {
    self.namespaces.get(namespace).map(|m| m.convoys.values().collect()).unwrap_or_default()
}
```

- [ ] **Step 4: Run, confirm pass**

Run: `cargo test -p flotilla-tui --locked`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "feat(tui): App applies namespace snapshots/deltas"
```

---

## Task 22: Scaffold `convoys_page` widget module

**Files:**
- Create: `crates/flotilla-tui/src/widgets/convoys_page/mod.rs`
- Create: `crates/flotilla-tui/src/widgets/convoys_page/glyphs.rs`
- Modify: `crates/flotilla-tui/src/widgets/mod.rs`

- [ ] **Step 1: Create glyphs helpers**

```rust
// crates/flotilla-tui/src/widgets/convoys_page/glyphs.rs
use flotilla_protocol::namespace::{ConvoyPhase, TaskPhase};
use ratatui::style::{Color, Modifier, Style};

pub struct Glyph {
    pub symbol: &'static str,
    pub style: Style,
}

pub fn convoy_glyph(phase: ConvoyPhase) -> Glyph {
    match phase {
        ConvoyPhase::Pending   => Glyph { symbol: "○", style: Style::default().add_modifier(Modifier::DIM) },
        ConvoyPhase::Active    => Glyph { symbol: "●", style: Style::default().fg(Color::Green) },
        ConvoyPhase::Completed => Glyph { symbol: "✓", style: Style::default().fg(Color::Green).add_modifier(Modifier::BOLD) },
        ConvoyPhase::Failed    => Glyph { symbol: "✗", style: Style::default().fg(Color::Red) },
        ConvoyPhase::Cancelled => Glyph { symbol: "⊘", style: Style::default().fg(Color::Red).add_modifier(Modifier::DIM) },
    }
}

pub fn task_glyph(phase: TaskPhase) -> Glyph {
    match phase {
        TaskPhase::Pending   => Glyph { symbol: "○", style: Style::default().add_modifier(Modifier::DIM) },
        TaskPhase::Ready     => Glyph { symbol: "◐", style: Style::default().fg(Color::Yellow) },
        TaskPhase::Launching => Glyph { symbol: "◑", style: Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD) },
        TaskPhase::Running   => Glyph { symbol: "●", style: Style::default().fg(Color::Green) },
        TaskPhase::Completed => Glyph { symbol: "✓", style: Style::default().fg(Color::Green).add_modifier(Modifier::BOLD) },
        TaskPhase::Failed    => Glyph { symbol: "✗", style: Style::default().fg(Color::Red) },
        TaskPhase::Cancelled => Glyph { symbol: "⊘", style: Style::default().fg(Color::Red).add_modifier(Modifier::DIM) },
    }
}
```

- [ ] **Step 2: Create the page module skeleton**

```rust
// crates/flotilla-tui/src/widgets/convoys_page/mod.rs
mod glyphs;
mod list;
mod detail;

pub use list::ConvoyList;
pub use detail::ConvoyDetail;

use flotilla_protocol::namespace::{ConvoyId, ConvoySummary};
use ratatui::{layout::{Constraint, Direction, Layout}, Frame};

pub enum ConvoyScope {
    All,
    Repo(flotilla_protocol::snapshot::RepoKey),
}

pub struct ConvoysPage<'a> {
    pub convoys: Vec<&'a ConvoySummary>,
    pub scope: ConvoyScope,
    pub selected: Option<&'a ConvoyId>,
    pub filter: &'a str,
}

impl<'a> ConvoysPage<'a> {
    pub fn render(&self, f: &mut Frame, area: ratatui::layout::Rect) {
        if self.convoys.is_empty() {
            // Empty state rendering inline (too small for its own widget)
            use ratatui::{text::Line, widgets::{Block, Borders, Paragraph}};
            let block = Block::default().borders(Borders::ALL).title(" Convoys ");
            let text = Line::from("No convoys. Create one via 'flotilla convoy create ...' (coming soon)");
            f.render_widget(Paragraph::new(text).block(block), area);
            return;
        }
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        ConvoyList { convoys: &self.convoys, selected: self.selected }.render(f, chunks[0]);
        if let Some(id) = self.selected {
            if let Some(convoy) = self.convoys.iter().find(|c| &c.id == id) {
                ConvoyDetail { convoy }.render(f, chunks[1]);
            }
        }
    }
}
```

- [ ] **Step 3: Create empty submodules so the `mod` declarations resolve**

```rust
// crates/flotilla-tui/src/widgets/convoys_page/list.rs
use flotilla_protocol::namespace::{ConvoyId, ConvoySummary};
use ratatui::Frame;

pub struct ConvoyList<'a> {
    pub convoys: &'a [&'a ConvoySummary],
    pub selected: Option<&'a ConvoyId>,
}

impl<'a> ConvoyList<'a> {
    pub fn render(&self, _f: &mut Frame, _area: ratatui::layout::Rect) {
        // Real rendering in Task 23.
    }
}
```

```rust
// crates/flotilla-tui/src/widgets/convoys_page/detail.rs
use flotilla_protocol::namespace::ConvoySummary;
use ratatui::Frame;

pub struct ConvoyDetail<'a> {
    pub convoy: &'a ConvoySummary,
}

impl<'a> ConvoyDetail<'a> {
    pub fn render(&self, _f: &mut Frame, _area: ratatui::layout::Rect) {
        // Real rendering in Task 24.
    }
}
```

- [ ] **Step 4: Export from `widgets/mod.rs`**

```rust
// crates/flotilla-tui/src/widgets/mod.rs
pub mod convoys_page;
```

- [ ] **Step 5: Build**

Run: `cargo build -p flotilla-tui --locked`
Expected: builds cleanly.

- [ ] **Step 6: Commit**

```bash
git add crates/flotilla-tui/src/widgets/
git commit -m "feat(tui): scaffold convoys_page widget module"
```

---

## Task 23: Implement `ConvoyList` with insta snapshot

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/convoys_page/list.rs`
- Test: `crates/flotilla-tui/src/widgets/convoys_page/list.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write a failing insta snapshot test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::namespace::{ConvoyId, ConvoyPhase, ConvoySummary};
    use ratatui::{backend::TestBackend, Terminal};

    fn sample(name: &str, phase: ConvoyPhase) -> ConvoySummary {
        ConvoySummary {
            id: ConvoyId::new("flotilla", name),
            namespace: "flotilla".into(),
            name: name.into(),
            workflow_ref: "wf".into(),
            phase,
            message: None,
            repo_hint: None,
            tasks: vec![],
            started_at: None,
            finished_at: None,
            observed_workflow_ref: None,
            initializing: false,
        }
    }

    #[test]
    fn convoy_list_snapshot_three_phases() {
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let a = sample("fix-a", ConvoyPhase::Active);
        let b = sample("fix-b", ConvoyPhase::Completed);
        let c = sample("fix-c", ConvoyPhase::Failed);
        let convoys: Vec<&ConvoySummary> = vec![&a, &b, &c];
        terminal.draw(|f| {
            ConvoyList { convoys: &convoys, selected: Some(&a.id) }.render(f, f.area());
        }).unwrap();
        insta::assert_snapshot!(terminal.backend());
    }
}
```

- [ ] **Step 2: Run, confirm fail (or a TestBackend render mismatch)**

Run: `cargo test -p flotilla-tui --locked convoy_list_snapshot_three_phases`
Expected: FAIL — snapshot missing or render empty.

- [ ] **Step 3: Implement `render`**

```rust
use crate::widgets::convoys_page::glyphs::convoy_glyph;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem},
    Frame,
};

impl<'a> ConvoyList<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self.convoys.iter().map(|convoy| {
            let glyph = convoy_glyph(convoy.phase);
            let is_selected = self.selected == Some(&convoy.id);
            let mut line_style = Style::default();
            if is_selected {
                line_style = line_style.add_modifier(Modifier::REVERSED);
            }
            ListItem::new(Line::from(vec![
                Span::styled(glyph.symbol, glyph.style),
                Span::raw(" "),
                Span::styled(convoy.name.clone(), line_style),
            ]))
        }).collect();
        let block = Block::default().borders(Borders::ALL).title(" Convoys ");
        f.render_widget(List::new(items).block(block), area);
    }
}
```

- [ ] **Step 4: Run, accept snapshot if it looks right, else iterate**

Run: `cargo test -p flotilla-tui --locked convoy_list_snapshot_three_phases`
Expected: first run creates a pending snapshot. Review it:

```bash
cargo insta review
```

Only accept after verifying the output matches the intended design (3 rows, glyphs correct, selection highlighted).

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "feat(tui): ConvoyList with status glyphs and selection highlight"
```

---

## Task 24: Implement `ConvoyDetail` — header + task tree + processes

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/convoys_page/detail.rs`

- [ ] **Step 1: Write failing snapshot test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use flotilla_protocol::namespace::{ConvoyId, ConvoyPhase, ConvoySummary, ProcessSummary, TaskPhase, TaskSummary};
    use ratatui::{backend::TestBackend, Terminal};

    fn multi_task_convoy() -> ConvoySummary {
        ConvoySummary {
            id: ConvoyId::new("flotilla", "fix-bug-123"),
            namespace: "flotilla".into(),
            name: "fix-bug-123".into(),
            workflow_ref: "review-and-fix".into(),
            phase: ConvoyPhase::Active,
            message: None,
            repo_hint: None,
            tasks: vec![
                TaskSummary {
                    name: "implement".into(),
                    depends_on: vec![],
                    phase: TaskPhase::Running,
                    processes: vec![ProcessSummary { role: "coder".into(), command_preview: "claude".into() }],
                    host: None, checkout: None, workspace_ref: None,
                    ready_at: None, started_at: None, finished_at: None, message: None,
                },
                TaskSummary {
                    name: "review".into(),
                    depends_on: vec!["implement".into()],
                    phase: TaskPhase::Pending,
                    processes: vec![ProcessSummary { role: "reviewer".into(), command_preview: "claude".into() }],
                    host: None, checkout: None, workspace_ref: None,
                    ready_at: None, started_at: None, finished_at: None, message: None,
                },
            ],
            started_at: None, finished_at: None, observed_workflow_ref: None, initializing: false,
        }
    }

    #[test]
    fn convoy_detail_snapshot() {
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let convoy = multi_task_convoy();
        terminal.draw(|f| {
            ConvoyDetail { convoy: &convoy }.render(f, f.area());
        }).unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn convoy_detail_initializing_snapshot() {
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        let mut convoy = multi_task_convoy();
        convoy.initializing = true;
        convoy.tasks.clear();
        terminal.draw(|f| {
            ConvoyDetail { convoy: &convoy }.render(f, f.area());
        }).unwrap();
        insta::assert_snapshot!(terminal.backend());
    }
}
```

- [ ] **Step 2: Run, confirm fail**

Run: `cargo test -p flotilla-tui --locked convoy_detail_snapshot convoy_detail_initializing_snapshot`
Expected: FAIL — render is empty.

- [ ] **Step 3: Implement**

```rust
use crate::widgets::convoys_page::glyphs::{convoy_glyph, task_glyph};
use flotilla_protocol::namespace::ConvoySummary;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use tui_tree_widget::{Tree, TreeItem, TreeState};

impl<'a> ConvoyDetail<'a> {
    pub fn render(&self, f: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        // Header
        let glyph = convoy_glyph(self.convoy.phase);
        let header = Paragraph::new(Line::from(vec![
            Span::styled(glyph.symbol, glyph.style),
            Span::raw(format!(" {} ", self.convoy.name)),
            Span::raw(format!("[{}]", self.convoy.workflow_ref)),
        ])).block(Block::default().borders(Borders::ALL));
        f.render_widget(header, chunks[0]);

        // Body: task tree OR initializing placeholder
        let body_block = Block::default().borders(Borders::ALL).title(" Tasks ");
        let body_area = chunks[1];
        if self.convoy.initializing {
            let p = Paragraph::new("initializing…").block(body_block);
            f.render_widget(p, body_area);
            return;
        }

        let items: Vec<TreeItem<&str>> = self.convoy.tasks.iter().map(|t| {
            let glyph = task_glyph(t.phase);
            let process_count = t.processes.len();
            let label = vec![
                Span::styled(glyph.symbol, glyph.style),
                Span::raw(format!(" {} ({} proc)", t.name, process_count)),
            ];
            TreeItem::new_leaf(t.name.as_str(), Line::from(label))
        }).collect();

        let mut state = TreeState::default();
        let tree = Tree::new(&items).expect("unique keys").block(body_block);
        f.render_stateful_widget(tree, body_area, &mut state);
    }
}
```

Check the actual `tui-tree-widget` 0.24 API: method names (`new` vs `new_with_items`), identifier type, etc. Adjust to the real signatures. The semantic goal is: one leaf per task; expand/collapse state held in `TreeState`.

- [ ] **Step 4: Run, accept snapshots after review**

Run: `cargo test -p flotilla-tui --locked convoy_detail_snapshot convoy_detail_initializing_snapshot`
Expected: new pending snapshots. Review with `cargo insta review` and accept if correct.

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "feat(tui): ConvoyDetail renders header + task tree + initializing placeholder"
```

---

## Task 25: Wire `ConvoysPage` into screen.rs + tabs.rs

**Files:**
- Modify: `crates/flotilla-tui/src/widgets/screen.rs`
- Modify: `crates/flotilla-tui/src/widgets/tabs.rs`

- [ ] **Step 1: Write failing test: navigating to the Convoys tab renders `ConvoysPage`**

Extend existing tab tests or add a new one in `widgets/screen.rs`'s test module:

```rust
#[test]
fn screen_renders_convoys_page_on_convoys_tab() {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut app = App::new_for_tests();
    // Seed with one convoy, switch to Convoys tab.
    app.handle_event(... snapshot with one convoy ...);
    app.set_active_tab(TabId::Convoys);
    terminal.draw(|f| app.render(f)).unwrap();
    // Assert output contains "Convoys" title
    let expected = "Convoys";
    let rendered = terminal.backend().to_string();
    assert!(rendered.contains(expected), "expected '{expected}' in:\n{rendered}");
}
```

`TestBackend::to_string()` may not exist — use `buffer_view` or iterate rows to build a debug string. Follow the pattern already used in other tab tests.

- [ ] **Step 2: Run, confirm fail**

- [ ] **Step 3: Render the convoys tab label in tabs.rs**

In `widgets/tabs.rs`, locate where `TabId::Flotilla` renders its label and add a matching case for `TabId::Convoys`:

```rust
TabId::Convoys => Span::raw(" 🚢 convoys ").into(),
```

Match the existing emoji/whitespace convention (don't invent a new one if the `Flotilla` label uses a specific format).

- [ ] **Step 4: Dispatch to `ConvoysPage` in `screen.rs`**

Locate the match-on-active-tab:

```rust
match active_tab {
    TabId::Flotilla => render_flotilla_overview(f, area, app),
    TabId::Convoys => {
        use crate::widgets::convoys_page::{ConvoyScope, ConvoysPage};
        let convoys: Vec<&_> = app.convoys("flotilla");
        let scope = ConvoyScope::All;
        let selected = app.selected_convoy_id();
        let filter = app.convoy_filter_str();
        ConvoysPage { convoys, scope, selected, filter }.render(f, area);
    }
    TabId::Repo(idx) => ... existing ...
    TabId::Add => ... existing ...
}
```

Implement `selected_convoy_id` and `convoy_filter_str` accessors on `App` — initial stubs returning `None` / `""` are fine, selection-state wiring lands in Task 26.

- [ ] **Step 5: Run, confirm pass**

Run: `cargo test -p flotilla-tui --locked`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "feat(tui): render ConvoysPage on the Convoys tab"
```

---

## Task 26: Selection state for the convoys tab + key handler wiring

**Files:**
- Modify: `crates/flotilla-tui/src/app/mod.rs` (or `ui_state.rs`)
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn convoys_tab_select_next_advances_selection() {
    let mut app = App::new_for_tests();
    app.handle_event(... snapshot with two convoys, a and b ...);
    app.set_active_tab(TabId::Convoys);
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("a"));
    app.handle_action(Action::SelectNext);
    assert_eq!(app.selected_convoy_id().map(|id| id.name()), Some("b"));
}
```

- [ ] **Step 2: Run, confirm fail**

- [ ] **Step 3: Implement**

Add to `App`:

```rust
pub struct ConvoysUiState {
    pub selected: Option<ConvoyId>,
    pub filter: String,
}

pub convoys_ui: ConvoysUiState, // field on App
```

Wire `Action::SelectNext` and `Action::SelectPrev` in `key_handlers.rs` — when the active binding mode is `Convoys`, move the `selected` cursor through the visible convoy list (respecting current filter, which is still empty at this point). Default selection on first populated snapshot is the first convoy.

- [ ] **Step 4: Run, confirm pass**

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "feat(tui): convoys tab selection + action wiring"
```

---

## Task 27: Filter input for the convoys tab (`/`)

**Files:**
- Modify: `crates/flotilla-tui/src/app/key_handlers.rs`
- Modify: `crates/flotilla-tui/src/widgets/convoys_page/mod.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn convoy_filter_narrows_visible_convoys() {
    let mut app = App::new_for_tests();
    // snapshot with three convoys: alpha, bravo, charlie
    app.set_active_tab(TabId::Convoys);
    app.set_convoy_filter("bra");
    let visible: Vec<&ConvoySummary> = app.visible_convoys("flotilla").collect();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].name, "bravo");
}
```

- [ ] **Step 2: Run, confirm fail**

- [ ] **Step 3: Implement**

```rust
impl App {
    pub fn set_convoy_filter(&mut self, f: impl Into<String>) {
        self.convoys_ui.filter = f.into();
    }

    pub fn visible_convoys(&self, namespace: &str) -> impl Iterator<Item = &ConvoySummary> {
        let f = self.convoys_ui.filter.to_lowercase();
        self.convoys(namespace).into_iter().filter(move |c| {
            f.is_empty()
                || c.name.to_lowercase().contains(&f)
                || c.repo_hint.as_ref().map(|r| r.0.to_lowercase().contains(&f)).unwrap_or(false)
        })
    }
}
```

In `ConvoysPage::render`, use `app.visible_convoys` via the `convoys: Vec<&ConvoySummary>` already passed in — the render path reads from whatever the screen layer passes, so the filter applies in `screen.rs`'s dispatch to `ConvoysPage`.

Wire `/` (already bound to `OpenCommandPalette` in Task 20) to the filter — or replace with a dedicated `Action::OpenConvoyFilter` if the command palette doesn't fit. Decide based on how `Action::OpenCommandPalette` currently behaves; if it reuses a text input overlay, you can inject a convoy-filter mode on it. Otherwise add a new action.

- [ ] **Step 4: Run, confirm pass**

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "feat(tui): convoy tab filter narrows visible list"
```

---

## Task 28: End-to-end test through `InProcessDaemon`

**Files:**
- Create: `crates/flotilla-tui/tests/convoy_view_e2e.rs` (or extend an existing e2e file)

- [ ] **Step 1: Orient on existing patterns**

Read the existing `InProcessDaemon` end-to-end tests to see the exact setup pattern:

```bash
rg -l "InProcessDaemon" crates/flotilla-core/tests/ crates/flotilla-tui/tests/
```

Start from `crates/flotilla-core/tests/in_process_daemon.rs` for the daemon-side construction and from `crates/flotilla-tui/tests/support/high_fidelity.rs` for the TUI-harness side. The exact API names (`InProcessDaemon::start`, `App::connect_via`, `wait_for`, backend accessor) may differ from the sketch below — use whatever the existing tests use.

- [ ] **Step 2: Write the test**

```rust
// crates/flotilla-tui/tests/convoy_view_e2e.rs
use std::{collections::BTreeMap, time::Duration};

// Imports below are illustrative — replace each with the real path used by the
// nearest existing InProcessDaemon e2e test (see Step 1).
use flotilla_core::InProcessDaemon;                    // or the test-support re-export
use flotilla_resources::{convoy::{Convoy, ConvoySpec, ConvoyStatus}, resource::ObjectMeta};

use crate::support::high_fidelity::TuiHarness;          // use whatever the existing tests use

#[tokio::test]
async fn tui_shows_convoys_from_daemon() {
    let daemon = InProcessDaemon::start().await;
    let mut harness = TuiHarness::connect(&daemon).await;

    let convoy = Convoy {
        metadata: ObjectMeta {
            name: "fix-bug-123".into(),
            namespace: Some("flotilla".into()),
            labels: BTreeMap::new(),
            ..Default::default()
        },
        spec: ConvoySpec { workflow_ref: "review-and-fix".into(), ..Default::default() },
        status: ConvoyStatus::default(),
    };
    daemon.resources().using::<Convoy>("flotilla").create(&convoy).await.unwrap();

    harness.wait_for(
        |app| !app.convoys("flotilla").is_empty(),
        Duration::from_secs(5),
    ).await.expect("convoy delivered to TUI");

    let visible = harness.app().convoys("flotilla");
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].name, "fix-bug-123");
}
```

If the existing TUI harness does not have a `wait_for` helper, use a small polling loop with `tokio::time::sleep` capped at the 5-second deadline — the pattern is:

```rust
let deadline = std::time::Instant::now() + Duration::from_secs(5);
while std::time::Instant::now() < deadline {
    harness.poll().await; // whatever method drives event delivery in the harness
    if !harness.app().convoys("flotilla").is_empty() {
        break;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
}
assert!(!harness.app().convoys("flotilla").is_empty(), "timed out waiting for convoy");
```

- [ ] **Step 2: Run, confirm it passes (or iterate until it does)**

Run: `cargo test -p flotilla-tui --locked --test convoy_view_e2e`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/flotilla-tui/tests/convoy_view_e2e.rs
git commit -m "test(tui): end-to-end convoy view through InProcessDaemon"
```

---

## Task 29: Run the full CI gate locally

- [ ] **Step 1: Apply formatting**

Run: `cargo +nightly-2026-03-12 fmt`

- [ ] **Step 2: Check formatting (CI gate)**

Run: `cargo +nightly-2026-03-12 fmt --check`
Expected: clean.

- [ ] **Step 3: Run clippy (CI gate)**

Run: `cargo clippy --workspace --all-targets --locked -- -D warnings`
Expected: no warnings. Fix any that appear.

- [ ] **Step 4: Run the full test suite (CI gate, with sandbox-safe flags per CLAUDE.md)**

Run:

```bash
mkdir -p .codex-tmp && TMPDIR="$PWD/.codex-tmp" cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests
```

Expected: all tests pass.

- [ ] **Step 5: Commit any formatting / lint fixes**

```bash
git add -u
git commit -m "chore: fmt + clippy cleanup for stage 6 PR 1"
```

---

## Task 30: Push and open the PR

- [ ] **Step 1: Push the branch**

```bash
git push -u origin feat/tui-convoy-view
```

- [ ] **Step 2: Open the PR**

```bash
gh pr create -R flotilla-org/flotilla --title "feat: stage 6 PR 1 — read-only convoy TUI view" --body "$(cat <<'EOF'
## Summary
- New namespace-scoped stream (`StreamKey::Namespace`) carries convoy summaries from daemon to TUI.
- `ConvoyProjection` in the daemon watches `Convoy` + `Presentation` resources and emits `NamespaceSnapshot` / `NamespaceDelta` events.
- New `Convoys` tab in the TUI renders the list + detail (task DAG via `tui-tree-widget`) with status glyphs.
- `flotilla-client` + CLI `watch` replay / gap-recovery extended for the new stream type.

Spec: `docs/superpowers/specs/2026-04-21-tui-convoy-view-design.md` (PR 1 slice).

## Test plan
- [ ] `cargo +nightly-2026-03-12 fmt --check`
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings`
- [ ] `cargo test --workspace --locked --features flotilla-daemon/skip-no-sandbox-tests`
- [ ] Verify Convoys tab renders in a live TUI session with an in-process backend.
EOF
)"
```

Keep the title under 70 chars. Body contains summary + the CI gate test plan.

---

## Self-Review Notes

Spec coverage check — each spec section maps to these tasks:

| Spec section | Tasks |
|---|---|
| Source of Truth (read model) | 9, 11 |
| Architecture (projection + stream) | 8, 14, 15, 16 |
| Protocol (new types) | 1–7 |
| UI (scope enum, widget hierarchy, filter) | 22–27 |
| UI (status glyphs, empty state, initializing) | 22 (glyphs), 22 (empty state in page mod), 24 (initializing) |
| Keybindings table (nav subset) | 20 |
| PR 1 slicing — all bullets | 1–18 (protocol+daemon+client+cli), 19–28 (TUI + e2e) |
| Testing strategy | Tests within each task + 28 (e2e) + 29 (CI gate) |

Completion / attach keybindings (`x`, `.`, `a`) are **not** in this plan — they are PR 2 / PR 3 and get separate plans.
