# Per-Task Presentation — Stage 5 Addendum

A narrow contract change to the stage-5 presentation resource model: Presentations key on **task**, not convoy. This addendum is a prerequisite for stage-6 task-level attach (see `2026-04-21-tui-convoy-view-design.md`).

Base spec: `2026-04-15-presentation-manager-design.md`. This document supersedes the "Presentation creation/deletion on phase transitions" section there and related bits.

## Motivation

Stage 5 shipped one `Presentation` per convoy, with `spec.process_selector = { CONVOY_LABEL: convoy.metadata.name }` matching every process from every task. That contract works today only because every convoy currently has exactly one task — the workspace managers (cmux, tmux, zellij) are themselves task/vessel oriented, so a single-task convoy maps cleanly to a single workspace.

Multi-task convoys break this. Workspace managers cannot usefully host "all processes from all tasks" in one static workspace when tasks have different checkouts, hosts, or lifecycles. The natural unit for a workspace is a task, matching `TaskWorkspace` which is already per-task.

A separate future need — a dynamic "convoy overview" presentation aggregating active tasks — is **not** part of this addendum. It's a larger presentation-side design that can layer on top of per-task Presentations later.

## Contract Change

Before (stage 5 as landed):

```rust
// Convoy transitions to Active:
CreatePresentation {
    meta.name = "<convoy-name>-presentation",
    meta.labels = { CONVOY_LABEL: <convoy-name> },
    spec.convoy_ref = <convoy-name>,
    spec.name = <convoy-name>,
    spec.process_selector = { CONVOY_LABEL: <convoy-name> },
}
// Convoy transitions to Completed/Failed/Cancelled:
DeletePresentation { name: "<convoy-name>-presentation" }
```

After:

```rust
// Task transitions to Ready (per-task, emitted once per task):
CreatePresentation {
    meta.name = "<convoy-name>-<task-name>",
    meta.labels = { CONVOY_LABEL: <convoy-name>, TASK_LABEL: <task-name> },
    spec.convoy_ref = <convoy-name>,
    spec.name = <task-name>,
    spec.process_selector = { CONVOY_LABEL: <convoy-name>, TASK_LABEL: <task-name> },
}
// Task transitions to Completed/Failed/Cancelled (per-task):
DeletePresentation { name: "<convoy-name>-<task-name>" }
```

Owner reference continues to point at the Convoy, so the existing Convoy finalizer (which lists Presentations by `CONVOY_LABEL` and deletes them) still cleans up all per-task Presentations on convoy deletion without modification.

## Creation Site

The create/delete actuations stay in the **convoy reconciler** — it already watches task phase transitions and is the documented "single swap point" for presentation triggers. The change is the phase it reacts to:

- Before: convoy phase transitions (`Active` → create; terminal → delete).
- After: task phase transitions (`Ready` → create; terminal → delete) emitted per task.

Rationale: the convoy reconciler already loops over tasks for TaskWorkspace management, so emitting per-task Presentation actuations alongside is a small extension rather than a restructure. Keeping it in the convoy reconciler preserves the "single swap point for future explicit-trigger policies" property.

## Selector Resolution

The presentation reconciler's `fetch_dependencies` already resolves terminal sessions via `process_selector`. With the new selector shape `{ CONVOY_LABEL, TASK_LABEL }`, it will only match sessions tagged with both labels. The stage-5 session-label code in the task-workspace reconciler already stamps `flotilla.work/convoy` and `flotilla.work/task` on terminal sessions (per `presentation-manager-design.md:60–62`), so this works out of the box — no changes needed in the presentation reconciler or session-label code.

## Behavior on Single-Task Convoys

Identical user-visible behavior. A single-task convoy produces one Presentation per task = one Presentation total. Name changes from `<convoy>-presentation` to `<convoy>-<task>`, which is internal. Attach from the TUI resolves to the one Presentation regardless of keying.

## Migration

No runtime data to migrate — we're in the pre-stable phase. Any existing Presentations created under the old contract get deleted when their convoys next transition to a terminal phase or are explicitly deleted. Tests using the old name format need updating.

## Test Coverage

- Convoy reconciler tests: assert one `CreatePresentation` per task on task-ready; assert `DeletePresentation` per task on terminal transitions; multi-task convoy produces multiple Presentations.
- Presentation reconciler tests: unchanged — selector semantics were already per-label-set, just tested against `{ CONVOY_LABEL }`. Add a new fixture with a `{ CONVOY_LABEL, TASK_LABEL }` selector and mixed sessions to prove only matching sessions come through.
- End-to-end: two-task convoy, both tasks Ready, assert two Presentations with distinct `observed_workspace_ref` values.

## Out of Scope

- Convoy-overview Presentation (dynamic, aggregates active tasks). Future presentation-side design; no effect on per-task Presentations.
- Changes to the presentation reconciler's own resource, spec, or status. Unchanged.
- `PresentationPolicy` per-task variation. `spec.presentation_policy_ref = "default"` still comes from convoy-level default for now; per-task policy can come later.
- TaskWorkspace changes. Already per-task; no changes required.
