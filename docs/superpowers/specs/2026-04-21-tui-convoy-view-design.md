# Stage 6 — TUI Convoy View

Design spec for stage 6 of the convoy implementation plan. See `2026-04-13-convoy-and-control-plane-design.md` for the larger convoy/flotilla-cp design and `2026-04-13-convoy-brainstorm-prompts.md` for the full stage sequence.

## Context

Stages 1–5 have landed: ResourceClient trait, WorkflowTemplate + Convoy resources, task provisioning runtime, and the first cut of the presentation controller runtime. Convoys can be created and progressed via resource APIs today, but they are invisible in the flotilla TUI. Stage 6 makes them visible and interactable.

The convoy TUI view is also the driver for two adjacent concerns:

1. The mutation path (TUI → daemon → resource client PATCH) becomes load-bearing for the first time outside tests. Completing a task from the TUI proves the full round-trip.
2. The stage-5 pane-mode concept (`flotilla tui --convoy <name>` running inside a convoy's own presentation workspace) needs a widget to run. Stage 6 builds that widget in a shape that supports the pane-mode re-use.

## Goals

- Make convoys visible and interactable in the flotilla TUI.
- Global `Convoys` tab showing a list of convoys + detail view with task DAG, alongside the existing `Overview` tab.
- Task-level completion from the TUI (wires the mutation path end-to-end).
- Task-level attach reuses the existing terminal-attach flow.
- Same widget reused (with scoping) for the `flotilla tui --convoy <id>` pane mode from stage 5.
- DAG visualization via `tui-tree-widget` — linear chains and fan-outs render as indented trees with status glyphs. Multi-parent DAGs are rendered as trees with duplicated nodes for now; a proper Sugiyama-layered renderer (`ascii-dag`) is a future upgrade and does not block this stage.

## Non-Goals

- Convoy creation flow. No UI to make a convoy from scratch yet — users create them by applying resources or via CLI (which does not exist yet either).
- Process-level attach. Whole-task attach only; process granularity waits on presentation-manager work in a later stage.
- Cross-linking with the work item table. Convoy-provisioned items duplicate into both views; no pointers either way. Integration into the work item table (option Z from brainstorm) is a future possibility, made easier by the existing heterogeneous-table machinery (WorkItem / Issue), but out of scope here.
- Correlation integration. Convoys do not flow into `ProviderData` for stage 6. They travel a parallel path: daemon reads resource watches, produces snapshot fields, TUI reads the new fields. Correlation stays UI-side and view-oriented per the larger design direction.
- Convoy editing UI. `DeleteConvoy` is specified for completeness but may be deferred to stage 7.

## Architecture

```
flotilla-cp (k8s REST) or InProcess resource store
                    │
                    │  watch(convoys, workflowtemplates, tasks)
                    ▼
         ┌──────────────────────────────┐
         │       flotilla-daemon        │
         │  ┌────────────────────────┐  │
         │  │   ConvoyProjection     │  │   Subscribes to resource watches,
         │  │                        │  │   maintains in-memory view,
         │  └───────────┬────────────┘  │   emits snapshot fields + deltas.
         │              │               │
         │   Snapshot { ..existing, convoys: Vec<ConvoySummary> }
         │   DaemonEvent::{ConvoyChanged, ConvoyRemoved}
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

- **Daemon is the adaptor for the TUI path.** Other daemon components (convoy controller, presentation reconciler) already read resources for their own reconciliation loops. `ConvoyProjection` is the single component on the TUI read path that translates resource state into `Snapshot` fields and `DaemonEvent` deltas. If/when the wire protocol shifts to k8s-shape end-to-end, the projection is the piece that gets rewritten. The TUI sees stable `Snapshot` / `DaemonEvent` shapes across that transition.
- **Delta-driven refresh.** No polling on the TUI side. The projection emits deltas on every meaningful resource event.
- **Mutations reuse the existing command envelope.** TUI dispatches commands → daemon routes → projection calls the resource client.
- **Per-repo scoping is a filter, not a structural split.** The tab is global; per-repo views come from scope filters, not from separate widgets.

## Protocol

All types live in `flotilla-protocol`. Deliberately minimal — easy to extend, easier to replace when the wire protocol shifts.

```rust
pub struct Snapshot {
    // ...existing fields
    pub convoys: Vec<ConvoySummary>,
}

pub struct ConvoySummary {
    pub id: ConvoyId,              // opaque "namespace/name"; stable for a resource's lifetime (no rename support)
    pub repo: Option<RepoKey>,     // from a label on the resource; None => unclaimed
    pub name: String,              // user-visible
    pub template_ref: String,      // workflow template name
    pub phase: ConvoyPhase,        // Pending | Running | Completed | Failed | Halted
    pub tasks: Vec<TaskSummary>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

pub struct TaskSummary {
    pub name: String,
    pub depends_on: Vec<String>,
    pub phase: TaskPhase,          // Pending | Ready | Running | Completed | Failed
    pub processes: Vec<ProcessSummary>,
    pub checkout: Option<CheckoutRef>,
    pub host: Option<HostName>,
}

pub struct ProcessSummary {
    pub role: String,
    pub command: String,
    pub terminal_ref: Option<TerminalRef>,
    pub status: ProcessStatus,     // NotStarted | Running | Exited { code } — display only
}

pub enum DaemonEvent {
    // ...existing variants
    ConvoyChanged { convoy: ConvoySummary },
    ConvoyRemoved { id: ConvoyId },
}

pub enum CommandAction {
    // ...existing variants
    CompleteTask { convoy_id: ConvoyId, task_name: String },
    DeleteConvoy { convoy_id: ConvoyId },
}
```

**YAGNI cuts:**

- No `TaskChanged` delta. Start with convoy-level deltas only; add task-granular deltas later only if bandwidth becomes a real problem.
- No `generation` / `observedGeneration` on snapshot types — the resource client handles conflict resolution internally.
- No `resourceVersion` exposed to the TUI — daemon maintains consistency; TUI trusts the ordering of its event stream.
- No process-exit → phase transitions. Task completion is always explicit, per the core convoy design.

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
    └── TaskProcesses           — processes of the selected task: role, command, terminal status
```

**Scope enum:**

```rust
pub enum ConvoyScope {
    All,
    Repo(RepoKey),
    Single(ConvoyId),
}
```

- `All` — global default; every convoy in the snapshot.
- `Repo(RepoKey)` — filter state within the global tab; also the scope applied automatically when entering the tab from a repo context.
- `Single(ConvoyId)` — pane-mode invocation (`flotilla tui --convoy <id>`); auto-focuses `ConvoyDetail`, hides the list, skips tab chrome.

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
| `d` | Delete convoy — opens existing `DeleteConfirm` mode |

Completion with `x` opens a lightweight inline confirmation (typing `y` confirms, anything else cancels) — same affordance as other destructive actions without adding a new confirm mode.

**Status glyphs:**

| Phase | Glyph | Color |
|-------|-------|-------|
| Pending | ○ | dim |
| Ready | ◐ | yellow |
| Running | ● | green |
| Completed | ✓ | green (bold) |
| Failed | ✗ | red |
| Halted | ⊘ | red (dim) |

**Empty state:** when `ConvoyList` has no entries for the active scope, show a centered message: `No convoys. Create one via 'flotilla convoy create ...' (coming soon)`. The CLI hint is aspirational — stage 6 does not ship a creation command.

## Slicing — PR Sequence

Four PRs off `feat/tui-convoy-view`, each independently mergeable.

### PR 1 — Read-only convoy view (core of stage 6)

- `flotilla-protocol`: all new types (`ConvoySummary`, `TaskSummary`, `ProcessSummary`, `ConvoyPhase`, `TaskPhase`, `ProcessStatus`, `ConvoyId`, `RepoKey` if not present); `Snapshot.convoys`; `DaemonEvent::{ConvoyChanged, ConvoyRemoved}`.
- `flotilla-daemon`: `ConvoyProjection` subscribing to convoy / workflowtemplate / task resources, producing snapshot state + deltas. Unit tested against the in-memory resource client.
- `flotilla-tui`: global `Convoys` tab, `ConvoysPage`, `ConvoyList`, `ConvoyDetail`, `TaskTree` (tui-tree-widget), `TaskProcesses`; `BindingModeId::Convoys` with navigation-only keys; `/` filter.
- Empty state, status glyphs, tab reachable via `[` / `]`.
- No mutations. Integration tests with scripted convoy fixtures validate display.

### PR 2 — Task completion

- `CommandAction::CompleteTask { convoy_id, task_name }`.
- Daemon dispatches to resource client PATCH on task status.
- TUI: `x` opens confirm prompt, `.` action menu adds "Complete task" entry.
- Tests: resource-client mock asserts PATCH; snapshot-driven rerender proves delta round-trips.

### PR 3 — Task attach

- `a` keybinding on a task: resolves the task's workspace identity and invokes existing terminal-attach flow.
- No new command — reuse existing attach machinery.
- Tests: integration test covering select-task → `a` → existing attach path invoked with correct identity.

### PR 4 — `flotilla tui --convoy <id>` pane mode

- New CLI flag on `flotilla tui` that sets initial `ConvoyScope::Single(id)`.
- TUI skips tab chrome and auto-focuses `ConvoyDetail`.
- Small; mostly CLI plumbing and a "single-convoy mode" branch in the app state.

**Dependencies:** PR 2 and PR 3 depend on PR 1; PR 4 depends on PR 1 only. PR 2 and PR 3 can overlap.

## Testing Strategy

- **Protocol.** Round-trip serde tests for all new types.
- **ConvoyProjection.** Unit tests against `InMemoryResourceClient` (from the stage-1 prototype). Feed resource events, assert snapshot state + deltas match expectations. Cover: add, modify, delete, out-of-order events, resync after disconnect.
- **TUI widgets.** Existing insta snapshot harness. Feed `ConvoysPage` a fixed `Snapshot.convoys` and assert render. One snapshot per scope variant, one for empty state, one covering each status glyph. Per the repo's testing philosophy, snapshot changes are signals — any diff must be investigated, not accepted reflexively.
- **Keybindings.** Existing `App` integration-test pattern: dispatch key events, assert commands dispatched + state transitions. Cover `x` confirm flow, `.` action menu, `a` attach dispatch, `/` filter, scope changes.
- **End-to-end.** `InProcessDaemon` integration test. Submit a convoy via resource client, assert TUI's `AppModel.convoys` populates and widget renders. Mark task complete from TUI, assert resource state changes.
- No live k8s / minikube in CI. All tests run against the in-memory resource client.

## Dependencies and Deferred Questions

PR 1 (read-only view) depends only on stages 1–3. Task state will render whether or not provisioning is producing meaningful processes — empty convoys are a valid display case.

PR 2 (task completion) depends on the same foundation — marking a task complete is a pure resource PATCH and does not need provisioning or presentation to be working.

PR 3 (task attach) depends on stage 5 being functional enough that tasks have associated terminals for the attach flow to reach.

PR 4 (pane mode) is CLI + widget scoping only; no new runtime dependencies beyond PR 1.

**Deferred to later stages:**

- Convoy creation UI (no decision yet — likely a dedicated brainstorm).
- Cross-linking with the work item table (option Z from the brainstorm; blocked on correlation evolution).
- Process-level attach (blocked on presentation-manager work).
- Multi-parent DAG rendering (`ascii-dag`) — the tree widget handles it with duplicated nodes for now.
- `TaskChanged` deltas, if convoy-level delta churn becomes a bottleneck.
- Global convoy aggregation across daemon peers — in multi-host setups today the snapshot already merges per-host state, and convoys come along for the ride; no new work needed here, but worth revisiting once real multi-host convoys exist.
- **Generalised arbitrary-tab model ([#589](https://github.com/flotilla-org/flotilla/issues/589)).** The current tab model is hard-coded to `Overview` + one tab per repo. #589 proposes arbitrary widget tabs — convoys, single convoy/task, approvals, projects, workflows, persistent agents, onboarding — each with a stable "address" for deep-linking into another TUI instance (and eventually the web). The `ConvoyScope` enum here is effectively such an address for convoy-flavored tabs: keep the scope shape serialisable and URL-friendly so the eventual migration is cheap. Stage 6 does not block on #589 and does not need to solve it, but the widget and its scoping should be designed as if an arbitrary-tab host is the eventual home.
