# Work Item Delta Design

## Summary

`RepoDelta` and the per-repo `DeltaEntry` log already send incremental provider changes, but they still carry a full `Vec<WorkItem>` on every delta. That means a small change to provider state can still serialize and transmit the entire correlated work-item list on every tick.

This design changes work items to use the same keyed delta mechanism as the rest of the protocol. Full snapshots remain full-state and continue to include `work_items`, but `RepoDelta` and the replay log carry only `Change::WorkItem` operations keyed by `WorkItemIdentity`.

## Goals

- Stop sending the full work-item vector on each `RepoDelta`.
- Keep correlation daemon-side; clients should continue to consume pre-correlated work items.
- Make live deltas and replay use the same incremental semantics.
- Preserve full `RepoSnapshot` behavior as the resync/fallback path.

## Non-Goals

- Recompute work-item correlation on the client.
- Introduce wire-format compatibility shims; the project is currently free to change protocol types.
- Optimize issue search metadata or other snapshot-level fields in this change.

## Current State

`Change::WorkItem` and `diff_work_items()` already exist, but they are not part of the production delta flow.

Current behavior:

- `RepoState::record_delta()` diffs provider data, provider health, and errors.
- `DeltaEntry` stores those `changes` plus a full `work_items` vector.
- `choose_event()` builds `RepoDelta` with the full `work_items` vector.
- `replay_since()` replays `DeltaEntry` entries and again emits the full `work_items` vector.
- The TUI applies provider changes incrementally but replaces its work-item vector wholesale on each delta.

## Proposed Design

### Protocol Model

- Keep `RepoSnapshot.work_items: Vec<WorkItem>` unchanged.
- Remove `work_items: Vec<WorkItem>` from `RepoDelta`.
- Remove `work_items: Vec<WorkItem>` from `DeltaEntry`.
- Treat `Change::WorkItem { identity, op }` as part of the normal per-repo delta stream.

This makes snapshots the full-state transport and deltas the incremental transport consistently across all repo-scoped state.

### Delta Generation

`RepoState` will track the last broadcast work-item list alongside the existing last-broadcast provider state. On refresh:

1. Build the new correlated work-item vector from the refreshed provider data.
2. Diff the previous and current vectors using `diff_work_items()`.
3. Append those `Change::WorkItem` entries into the same `changes` vector as provider updates.
4. Record only the combined `changes` vector in `DeltaEntry`.
5. Update the cached last-broadcast work-item state after recording.

`WorkItemIdentity` is already stable enough to key add/update/remove operations for the existing item kinds.

### Replay

Replay should emit the recorded `DeltaEntry.changes` exactly as stored. No full work-item vector should be reconstructed or included in replayed `RepoDelta` events.

Clients that miss too much history will still receive a full `RepoSnapshot`, which fully rebuilds provider data and work items.

### Client/TUI Materialization

The TUI should maintain work items incrementally on repo-scoped delta application:

- On `RepoSnapshot`, replace the full work-item vector as today.
- On `RepoDelta`, apply `Change::WorkItem` ops into the current work-item state.

The existing UI already keys selection and pending state by `WorkItemIdentity`, so item identity is the correct update boundary. The display order should continue to come from the rendering/grouping layer rather than the insertion order of incoming deltas.

## Ordering and UI Behavior

Moving to keyed work-item deltas means the incoming order of `Change::WorkItem` operations is not important. The rendered table order should remain derived from `group_work_items_split()` and its sorting rules.

That keeps ordering behavior deterministic across:

- full snapshots
- live deltas
- replayed deltas after reconnect

## Risks

### Correlation Churn

If small provider changes cause many correlated fields on a work item to change, the wire payload may still contain many `Updated` entries. That is acceptable for this change; the goal is to avoid unconditional full-vector replacement, not to invent a field-level patch format.

### Equality Sensitivity

`diff_work_items()` uses full `WorkItem` equality for update detection. Any field that changes in the flattened proto item produces an `Updated` op. This is the correct behavior as long as the client is meant to mirror the daemon’s rendered work-item state exactly.

### Empty Delta Behavior

`choose_event()` currently treats `delta.changes.is_empty()` as a reason to avoid sending a delta. That continues to work unchanged once work-item changes are folded into `changes`.

## Testing Strategy

- Extend core delta tests to cover production use of work-item changes.
- Update repo-state/in-process tests to expect `Change::WorkItem` entries rather than `work_items` vectors on deltas.
- Add TUI tests showing snapshot replacement plus incremental add/update/remove work-item application.
- Run the exact repo-safe test command under the sandbox and targeted crate tests where useful.

## Implementation Outline

1. Remove full work-item vectors from `RepoDelta` and `DeltaEntry`.
2. Fold `diff_work_items()` into `RepoState::record_delta()`.
3. Update daemon delta emission and replay to use only `changes`.
4. Update TUI delta application to materialize work-item changes incrementally.
5. Refresh affected tests and JSON roundtrip expectations.
