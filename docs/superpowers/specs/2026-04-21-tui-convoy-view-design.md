# Stage 6 — TUI Convoy View

Design spec for stage 6 of the convoy implementation plan. See `2026-04-13-convoy-and-control-plane-design.md` for the larger convoy/flotilla-cp design and `2026-04-13-convoy-brainstorm-prompts.md` for the full stage sequence.

## Context

Stages 1–5 have landed: ResourceClient trait, WorkflowTemplate + Convoy resources, task provisioning runtime, and the first cut of the presentation controller runtime. Convoys can be created and progressed via resource APIs today, but they are invisible in the flotilla TUI. Stage 6 makes them visible and interactable.

The convoy TUI view is also the driver for two adjacent concerns:

1. The mutation path (TUI → daemon → resource client PATCH) becomes load-bearing for the first time outside tests. Completing a task from the TUI proves the full round-trip.
2. The stage-5 pane-mode concept (`flotilla tui --convoy <namespace>/<name>` running inside a convoy's own presentation workspace) needs a widget to run. Stage 6 builds that widget in a shape that supports the pane-mode re-use.

## Source of Truth

The read model is grounded in convoy **status**, not live WorkflowTemplates:

- **Task DAG structure** comes from `ConvoyStatus.workflow_snapshot.tasks` — frozen at init (see `2026-04-14-convoy-resource-design.md`). Live templates are never read by the projection; if a template is edited after a convoy bootstraps, the snapshot still describes what is actually executing.
- **Per-task runtime state** comes from `ConvoyStatus.tasks[name]` (the `TaskState` map).
- **Attach target per task** comes from the `Presentation` resource owned by the convoy/task: specifically `PresentationStatus.observed_workspace_ref`. See `2026-04-15-presentation-manager-design.md`.

A convoy in `Pending` phase may have no `workflow_snapshot` yet; the projection surfaces such convoys with an `initializing` placeholder rather than an empty task list so the user sees the convoy came into existence.

## Goals

- Make convoys visible and interactable in the flotilla TUI.
- Global `Convoys` tab showing a list of convoys + detail view with task DAG, alongside the existing `Overview` tab.
- Task-level completion from the TUI (wires the mutation path end-to-end).
- Task-level attach reuses the existing terminal-attach flow.
- Same widget reused (with scoping) for the `flotilla tui --convoy <namespace>/<name>` pane mode from stage 5.
- DAG visualization via `tui-tree-widget` — linear chains and fan-outs render as indented trees with status glyphs. Multi-parent DAGs are rendered as trees with duplicated nodes for now; a proper Sugiyama-layered renderer (`ascii-dag`) is a future upgrade and does not block this stage.

## Non-Goals

- Convoy creation flow. No UI to make a convoy from scratch yet — users create them by applying resources or via CLI (which does not exist yet either).
- Process-level attach. Whole-task attach only; process granularity waits on presentation-manager work in a later stage.
- Cross-linking with the work item table. Convoy-provisioned items duplicate into both views; no pointers either way. Integration into the work item table (option Z from brainstorm) is a future possibility, made easier by the existing heterogeneous-table machinery (WorkItem / Issue), but out of scope here.
- Correlation integration. Convoys do not flow into `ProviderData` for stage 6. They travel a parallel path: daemon reads resource watches, produces snapshot fields, TUI reads the new fields. Correlation stays UI-side and view-oriented per the larger design direction.
- Convoy editing UI. Delete / cancel of a running convoy is a later concern; no delete command or keybinding ships in stage 6.

## Architecture

```
flotilla-cp (k8s REST) or InProcess resource store
                    │
                    │  watch(Convoy, Presentation)
                    ▼
         ┌──────────────────────────────┐
         │       flotilla-daemon        │
         │  ┌────────────────────────┐  │
         │  │   ConvoyProjection     │  │   Subscribes to resource watches,
         │  │                        │  │   maintains in-memory view,
         │  └───────────┬────────────┘  │   emits namespace snapshots + deltas.
         │              │               │
         │   NamespaceSnapshot { convoys: Vec<ConvoySummary> }
         │   DaemonEvent::{NamespaceSnapshot, NamespaceDelta}
         │   (replay keyed by StreamKey::Namespace { name })
         │              │               │
         └──────────────┼───────────────┘
                        │  existing socket protocol
                        ▼
                   flotilla-tui
              ┌──────────────────┐
              │  ConvoysPage     │  reads convoys from AppModel,
              │  (scoped widget) │  renders list + tree detail
              └──────────────────┘
```

Key properties:

- **Convoys live on their own stream.** Today the wire splits into per-repo `RepoSnapshot` and per-host `HostSnapshot` streams (`crates/flotilla-protocol/src/snapshot.rs` and `lib.rs`), with replay cursors keyed via `StreamKey`. Convoys are namespace-scoped resources that may span repos and hosts, so we introduce a third stream keyed by `StreamKey::Namespace { name }`. One stream per active namespace; for MVP the default namespace (`flotilla`) carries everything.
- **Projection reads convoy + presentation status.** `ConvoyProjection` watches `Convoy` resources (authoritative for DAG structure via `status.workflow_snapshot` and per-task runtime state via `status.tasks`) and `Presentation` resources (for `observed_workspace_ref` per task). It does not read live `WorkflowTemplate` resources; the frozen snapshot on the convoy is the source of truth once a convoy is past `Pending`. `TerminalSession` process-level status is deferred (see Deferred section).
- **Daemon is the adaptor for the TUI path.** Other daemon components (convoy controller, presentation reconciler) already read resources for their own reconciliation loops. `ConvoyProjection` is the single component on the TUI read path that translates resource state into `NamespaceSnapshot` / `DaemonEvent::NamespaceDelta`. If/when the wire protocol shifts to k8s-shape end-to-end, the projection is the piece that gets rewritten. The TUI sees stable wire shapes across that transition.
- **Delta-driven refresh.** No polling on the TUI side. The projection emits deltas on every meaningful resource event.
- **Mutations reuse existing commands.** `CommandAction::ConvoyTaskComplete { convoy, task, message }` already exists in `crates/flotilla-protocol/src/commands.rs` and is wired through `in_process.rs` / daemon runtime. Stage 6 does not add a new completion command — the TUI just plumbs existing binding actions into this command.
- **Per-repo scoping is a filter, not a structural split.** The tab is global; per-repo views come from scope filters on top of the namespace stream, not from separate widgets or streams.

## Protocol

All new types live in `flotilla-protocol`. Shape mirrors the resource status fields rather than introducing a parallel vocabulary — easier to reason about, easier to replace when the wire protocol shifts k8s-shape.

**Stream key extension:**

```rust
pub enum StreamKey {
    Repo { identity: RepoIdentity },
    Host { environment_id: EnvironmentId },
    Namespace { name: String },   // NEW — one stream per namespace
}
```

`ReplaySince` continues to take a `Vec<ReplayCursor>`; clients that care about convoys include a namespace cursor alongside their repo/host cursors.

**Namespace snapshot + deltas:**

```rust
/// Full snapshot for one namespace. Sent on initial connect, after seq gaps,
/// or when delta would be larger than the full snapshot. Mirrors the
/// RepoSnapshot idiom.
pub struct NamespaceSnapshot {
    pub seq: u64,
    pub namespace: String,
    pub convoys: Vec<ConvoySummary>,
}

/// Incremental delta for one namespace.
pub struct NamespaceDelta {
    pub seq: u64,
    pub namespace: String,
    pub changed: Vec<ConvoySummary>,
    pub removed: Vec<ConvoyId>,
}

pub enum DaemonEvent {
    // ...existing variants
    NamespaceSnapshot(Box<NamespaceSnapshot>),
    NamespaceDelta(Box<NamespaceDelta>),
}
```

**Convoy summary (wire shape for the TUI):**

```rust
pub struct ConvoyId(pub String);   // "namespace/name"

pub struct ConvoySummary {
    pub id: ConvoyId,
    pub namespace: String,
    pub name: String,
    pub workflow_ref: String,                 // matches ConvoySpec.workflow_ref
    pub phase: ConvoyPhase,                   // mirrors resource-design enum
    pub message: Option<String>,
    pub repo_hint: Option<RepoKey>,           // from flotilla.work/repo label if present
    pub tasks: Vec<TaskSummary>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub observed_workflow_ref: Option<String>,
    pub initializing: bool,                   // true when workflow_snapshot not yet populated
}

/// Mirrors ConvoyPhase from the convoy resource design — do not simplify.
pub enum ConvoyPhase { Pending, Active, Completed, Failed, Cancelled }

pub struct TaskSummary {
    pub name: String,
    pub depends_on: Vec<String>,
    pub phase: TaskPhase,                     // mirrors resource-design enum
    pub processes: Vec<ProcessSummary>,
    pub host: Option<HostName>,               // from placement status when available
    pub checkout: Option<CheckoutRef>,        // when known via placement
    pub workspace_ref: Option<String>,        // from matching Presentation.status.observed_workspace_ref
    pub ready_at: Option<Timestamp>,
    pub started_at: Option<Timestamp>,
    pub finished_at: Option<Timestamp>,
    pub message: Option<String>,
}

/// Mirrors TaskPhase from the convoy resource design — do not simplify.
pub enum TaskPhase { Pending, Ready, Launching, Running, Completed, Failed, Cancelled }

pub struct ProcessSummary {
    pub role: String,
    pub command_preview: String,              // short human-readable; derived from ProcessDefinition
    // Process-level terminal status is deferred to a future PR; see Deferred section.
}
```

**Commands:**

No new commands are added. Task completion uses the existing `CommandAction::ConvoyTaskComplete { convoy, task, message }`. Convoy deletion is deferred to a later stage along with the rest of convoy editing.

**Resolving the attach target.** `TaskSummary.workspace_ref` is populated by the projection by watching `Presentation` resources: `PresentationSpec.convoy_ref` identifies the convoy, and `flotilla.work/task` labels (per `presentation-manager-design.md`) identify the task. The projection keeps a `convoy/task → ws_ref` index from the latest `PresentationStatus.observed_workspace_ref`. PR 3's `a` keybinding reads this field and dispatches the existing `CommandAction::SelectWorkspace { ws_ref }`.

**YAGNI cuts:**

- No task-level delta variant. Convoy-level `changed`/`removed` replays the whole convoy, which is tractable for the scales we expect. Add task-granular deltas only if bandwidth becomes a real problem.
- No `generation` / `observedGeneration` on wire types — the resource client handles conflict resolution internally.
- No `resourceVersion` exposed to the TUI — daemon maintains consistency; TUI trusts event ordering.
- Process exit does not feed task phase. Task completion is always explicit per the core convoy design.
- Process-level terminal status (NotStarted / Running / Exited) deferred; needs a `TerminalSession` watch that isn't wired yet.

## UI

**Tab placement:** a new global `Convoys` tab alongside `Overview`, reachable via `[` / `]`. Default scope is `All`.

**Widget hierarchy:**

```
ConvoysPage { scope: ConvoyScope }
├── ConvoyList      (left pane)  — convoys matching scope
└── ConvoyDetail    (right pane) — selected convoy
    ├── ConvoyHeader            — name, template, phase, timestamps
    ├── TaskTree                — tui-tree-widget showing task DAG
    │   └── Tree nodes          — task name + phase glyph + process count
    └── TaskProcesses           — processes of the selected task: role + command preview (terminal status deferred)
```

**Scope enum:**

```rust
pub enum ConvoyScope {
    All,
    Repo(RepoKey),
    Single(ConvoyId),
}
```

- `All` — global default; every convoy across all namespace streams the client is subscribed to.
- `Repo(RepoKey)` — filter state within the global tab, matching against `ConvoySummary.repo_hint`; also the scope applied automatically when entering the tab from a repo context.
- `Single(ConvoyId)` — pane-mode invocation (`flotilla tui --convoy <namespace>/<name>`); `<namespace>/<name>` is the stable pane-mode contract and matches `ConvoyId`. Auto-focuses `ConvoyDetail`, hides the list, skips tab chrome.

**Filtering:** the existing `/` search binding mode is reused to filter the list by repo name, convoy name, or phase substring. Filter state is held on the widget, not in `UiState` globally.

**Convoys binding mode (new `BindingModeId::Convoys`):**

| Key | Action |
|-----|--------|
| `j` / `k` | Move selection (list or tree, depending on focus) |
| `l` / `Enter` | Focus detail / expand tree node |
| `h` / `Esc` | Focus list / collapse tree node |
| `x` | Mark selected task completed (confirm with `y`) |
| `.` | Action menu for selected task (complete, attach) |
| `a` | Attach to selected task's workspace (reuses terminal-attach path) |
| `r` | Refresh request (no-op on server; kept for consistency with other modes) |

Completion with `x` opens a lightweight inline confirmation (typing `y` confirms, anything else cancels) — same affordance as other destructive actions without adding a new confirm mode.

**Status glyphs:**

ConvoyPhase:

| Phase | Glyph | Color |
|-------|-------|-------|
| Pending | ○ | dim |
| Active | ● | green |
| Completed | ✓ | green (bold) |
| Failed | ✗ | red |
| Cancelled | ⊘ | red (dim) |

TaskPhase:

| Phase | Glyph | Color |
|-------|-------|-------|
| Pending | ○ | dim |
| Ready | ◐ | yellow |
| Launching | ◑ | yellow (bold) |
| Running | ● | green |
| Completed | ✓ | green (bold) |
| Failed | ✗ | red |
| Cancelled | ⊘ | red (dim) |

When a convoy's `initializing` flag is true (no `workflow_snapshot` yet), the DAG area shows "initializing…" instead of an empty tree.

**Empty state:** when `ConvoyList` has no entries for the active scope, show a centered message: `No convoys. Create one via 'flotilla convoy create ...' (coming soon)`. The CLI hint is aspirational — stage 6 does not ship a creation command.

## Slicing — PR Sequence

Four PRs off `feat/tui-convoy-view`, each independently mergeable.

### PR 1 — Read-only convoy view (core of stage 6)

- `flotilla-protocol`: `ConvoyId`, `ConvoySummary`, `TaskSummary`, `ProcessSummary`, `ConvoyPhase`, `TaskPhase`, `NamespaceSnapshot`, `NamespaceDelta`; `StreamKey::Namespace { name }`; `DaemonEvent::{NamespaceSnapshot, NamespaceDelta}`.
- `flotilla-daemon`: `ConvoyProjection` watching `Convoy` and `Presentation` resources, producing per-namespace snapshots + deltas. Maintains `convoy/task → workspace_ref` index from Presentation status. Initial-sync and gap-recovery paths mirror the existing `RepoSnapshot` machinery. Unit tested against the in-memory resource client.
- `flotilla-tui`: global `Convoys` tab, `ConvoysPage`, `ConvoyList`, `ConvoyDetail`, `TaskTree` (tui-tree-widget), `TaskProcesses`; `BindingModeId::Convoys` with navigation-only keys; `/` filter; `initializing…` placeholder for pre-snapshot convoys.
- Empty state, status glyphs for both phase enums, tab reachable via `[` / `]`.
- No mutations. Integration tests with scripted convoy resource fixtures validate display.

### PR 2 — Task completion

- No new command. TUI `x` keybinding and `.` action menu "Complete task" dispatch the existing `CommandAction::ConvoyTaskComplete { convoy, task, message }`.
- `x` opens the inline confirm prompt; on `y` it dispatches with `message = None`.
- Tests: keybinding dispatch test asserts the existing command is sent; end-to-end `InProcessDaemon` test observes `TaskState.phase` transition and delta arriving back at the TUI.

### PR 3 — Task attach

- `a` keybinding on a task reads `TaskSummary.workspace_ref`. When `Some(ws_ref)` it dispatches `CommandAction::SelectWorkspace { ws_ref }`. When `None` it shows a transient status line ("no workspace yet") rather than erroring.
- No new command or attach machinery.
- Tests: integration test covering select-task → `a` → existing `SelectWorkspace` dispatched with the correct `ws_ref`; test for the no-workspace case.

### PR 4 — `flotilla tui --convoy <namespace>/<name>` pane mode

- New CLI flag on `flotilla tui` parsing `<namespace>/<name>` into `ConvoyId` and setting initial `ConvoyScope::Single(id)`.
- TUI skips tab chrome and auto-focuses `ConvoyDetail`.
- Small; mostly CLI plumbing and a "single-convoy mode" branch in the app state.

**Dependencies:** PR 2 and PR 3 depend on PR 1; PR 4 depends on PR 1 only. PR 2 and PR 3 can overlap.

## Testing Strategy

- **Protocol.** Round-trip serde tests for all new types and the `StreamKey::Namespace` variant.
- **ConvoyProjection.** Unit tests against `InMemoryResourceClient` (from the stage-1 prototype). Feed `Convoy` and `Presentation` resource events, assert `NamespaceSnapshot` + `NamespaceDelta` match expectations. Cover: convoy add/modify/delete, Presentation workspace_ref arrival / change / clear, `workflow_snapshot` absent (`initializing=true`) then populated, out-of-order events, resync after disconnect (full snapshot re-sent on gap).
- **TUI widgets.** Existing insta snapshot harness. Feed `ConvoysPage` fixed `NamespaceSnapshot` fixtures and assert render. One snapshot per scope variant, one for empty state, one per ConvoyPhase, one per TaskPhase, one for the initializing placeholder. Per the repo's testing philosophy, snapshot changes are signals — any diff must be investigated, not accepted reflexively.
- **Keybindings.** Existing `App` integration-test pattern: dispatch key events, assert commands dispatched + state transitions. Cover `x` confirm flow → `ConvoyTaskComplete`, `.` action menu, `a` attach dispatch (both `Some(ws_ref)` and `None`), `/` filter, scope changes.
- **End-to-end.** `InProcessDaemon` integration test. Create a convoy via the resource client, assert the TUI receives a `NamespaceSnapshot` with the convoy and the widget renders. Mark task complete from TUI, assert `TaskState.phase` transitions and delta arrives back. Create a Presentation for a task, assert `workspace_ref` appears on the summary.
- No live k8s / minikube in CI. All tests run against the in-memory resource client.

## Dependencies and Deferred Questions

PR 1 (read-only view) depends only on stages 1–3. Task state will render whether or not provisioning is producing meaningful processes — empty convoys are a valid display case.

PR 2 (task completion) depends on the same foundation — marking a task complete is a pure resource PATCH and does not need provisioning or presentation to be working.

PR 3 (task attach) depends on stage 5 being functional enough that tasks have associated terminals for the attach flow to reach.

PR 4 (pane mode) is CLI + widget scoping only; no new runtime dependencies beyond PR 1.

**Deferred to later stages:**

- Convoy creation UI (no decision yet — likely a dedicated brainstorm).
- Convoy deletion / cancellation UI.
- Cross-linking with the work item table (option Z from the brainstorm; blocked on correlation evolution).
- Process-level attach (blocked on presentation-manager work that exposes per-process terminal identity).
- Process-level terminal status on `ProcessSummary` (NotStarted/Running/Exited) — requires a `TerminalSession` watch in the projection.
- Multi-parent DAG rendering (`ascii-dag`) — the tree widget handles it with duplicated nodes for now.
- Task-level deltas, if convoy-level delta churn becomes a bottleneck.
- Multi-namespace UX — today everything is in the default `flotilla` namespace. A namespace picker / per-namespace scope in `ConvoyScope` is straightforward to add once we actually use more than one.
- Multi-host convoy aggregation across daemon peers — per-host snapshot merging is already handled by the existing peer layer; convoys will benefit when the namespace stream propagates via the same mechanism.
- **Generalised arbitrary-tab model ([#589](https://github.com/flotilla-org/flotilla/issues/589)).** The current tab model is hard-coded to `Overview` + one tab per repo. #589 proposes arbitrary widget tabs — convoys, single convoy/task, approvals, projects, workflows, persistent agents, onboarding — each with a stable "address" for deep-linking into another TUI instance (and eventually the web). The `ConvoyScope` enum here is effectively such an address for convoy-flavored tabs: keep the scope shape serialisable and URL-friendly so the eventual migration is cheap. Stage 6 does not block on #589 and does not need to solve it, but the widget and its scoping should be designed as if an arbitrary-tab host is the eventual home.
