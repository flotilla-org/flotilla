# cmux Cross-Window Workspace Discovery Design

**Date:** 2026-03-11

**Issue:** [#214](https://github.com/rjwittams/flotilla/issues/214)

## Summary

Flotilla's cmux workspace manager currently treats `cmux --json list-workspaces` as a complete workspace inventory. That assumption is wrong when cmux workspaces are spread across multiple macOS windows. The command is window-scoped, so a workspace moved into another window disappears from the provider data seen by the current Flotilla process. Refresh then fails to correlate that checkout with its existing workspace, and later actions may try to create a duplicate workspace for the same checkout.

This design fixes the bug inside the cmux adapter by enumerating all cmux windows and aggregating their workspaces into one logical inventory before returning data to the rest of Flotilla.

## Goals

- Discover cmux workspaces across all windows, not just the current one.
- Preserve the existing `WorkspaceManager` trait and protocol behavior.
- Keep selection behavior compatible with the current `ws_ref` workspace identity.
- Degrade gracefully when one window query fails.

## Non-Goals

- Adding a cross-provider "workspace existence race" abstraction.
- Changing `Workspace`, `WorkspaceConfig`, or protocol-level workspace identifiers.
- Adding terminal-pool or shpool-specific duplicate-attach handling.
- Refactoring correlation or executor behavior outside the cmux adapter.

## Current Behavior

The current implementation in `crates/flotilla-core/src/providers/workspace/cmux.rs` does a single `cmux --json list-workspaces` call and parses the returned `workspaces` array into `Workspace` values. Each workspace's `directories` become `CorrelationKey::CheckoutPath` values, and refresh hands those to the existing correlation pipeline.

Because `list-workspaces` is window-scoped, this only reports workspaces in the current macOS window. A workspace moved to another window vanishes from provider data even though it still exists in cmux.

## Proposed Design

### Adapter-local global workspace inventory

`CmuxWorkspaceManager::list_workspaces()` should stop assuming that `list-workspaces` is global. Instead it should:

1. Run `cmux --json list-windows`.
2. Parse all window refs from the response.
3. Run `cmux --json list-workspaces --window <window_ref>` for each window.
4. Parse each window-local workspace list using shared JSON parsing logic.
5. Merge the results into one `Vec<(String, Workspace)>`.

The rest of Flotilla continues to treat cmux as a single workspace provider. No caller needs to know that the adapter gathered the inventory from multiple windows.

### Stable workspace identity

The workspace identity exposed to the rest of Flotilla remains the existing cmux workspace ref, such as `workspace:17`. The design does not add window refs to public workspace identifiers or to the `Workspace` struct.

This keeps refresh, correlation, intents, executor commands, and protocol serialization unchanged.

### Selection behavior

`select_workspace()` continues to accept the same `ws_ref` string. The assumption for this issue is that `cmux select-workspace --workspace <ws_ref>` is still the correct command once the workspace is known globally by cmux.

If real-world verification later shows that cmux selection also requires explicit window routing, that should be handled as a contained follow-up inside the same adapter rather than through shared Flotilla types.

## Components

### `CmuxWorkspaceManager`

Add small internal helpers in `crates/flotilla-core/src/providers/workspace/cmux.rs`:

- A helper that parses `list-windows` JSON into window refs.
- A helper that parses a `list-workspaces` JSON payload into `Vec<(String, Workspace)>`.

`list_workspaces()` becomes orchestration code over these helpers. The parser for workspace JSON should be shared so both current and new call sites use the same `Workspace` construction logic.

### No shared-model changes

No changes are needed in:

- `crates/flotilla-core/src/providers/workspace/mod.rs`
- `crates/flotilla-core/src/refresh.rs`
- `crates/flotilla-core/src/executor.rs`
- `crates/flotilla-core/src/providers/correlation.rs`

Those layers already consume normalized workspace data. The bug is caused by incomplete provider output, not by a downstream modeling flaw.

## Data Flow

The refresh path remains structurally the same:

1. Refresh asks the selected workspace manager for workspaces.
2. The cmux adapter enumerates all windows and lists workspaces per window.
3. The adapter merges the returned workspaces and derives `CorrelationKey::CheckoutPath` from each workspace directory.
4. Refresh stores the merged workspaces in provider data.
5. Existing correlation logic matches those checkout paths against checkout provider entries.

The important change is that moved workspaces remain visible to refresh, so correlated work items continue to show the existing workspace and do not look eligible for duplicate creation.

## Error Handling

### Window listing failure

If `cmux --json list-windows` fails, `list_workspaces()` should return an error. Without the window inventory there is no reliable way to enumerate cmux workspaces globally.

### Per-window workspace failure

If a single `list-workspaces --window <window_ref>` call fails, the adapter should log the failure with the window ref and continue aggregating the remaining windows. This preserves the best available workspace view and matches Flotilla's general preference for partial provider success over total failure.

### Malformed per-window JSON

If a single window returns malformed JSON or a missing `workspaces` array, treat that window as failed, log it, and continue. Malformed data for one window should not poison all workspace refresh.

### Duplicate workspace refs

If the aggregated results contain duplicate workspace refs, dedupe by workspace ref and keep the first parsed entry. This is defensive rather than required by current cmux behavior.

## Testing Strategy

Testing should stay focused on `crates/flotilla-core/src/providers/workspace/cmux.rs`.

Add unit tests for:

- listing workspaces across multiple windows
- skipping a failed window while returning successful window results
- failing when `list-windows` fails
- deduping duplicate workspace refs across windows
- preserving directory parsing into `CorrelationKey::CheckoutPath`

No protocol, executor, or refresh tests are required unless adapter-level tests expose a real gap.

## Trade-offs

### Why this design

This is the smallest fix that addresses the confirmed root cause. It keeps the change localized to the cmux adapter, preserves existing public boundaries, and avoids overfitting shared Flotilla types to a cmux-specific window model.

### Why not add window metadata to `Workspace`

That would spread a provider-specific concern into shared types without evidence that the rest of Flotilla needs it. For this issue, the missing capability is inventory completeness, not downstream awareness of window placement.

### Why not add duplicate-creation guards now

Those would be useful as broader resilience work, but they solve symptoms rather than the confirmed source of this regression. The chosen scope is intentionally narrow.

## Follow-up Opportunities

- Verify whether `cmux select-workspace --workspace <ws_ref>` is fully cross-window aware in practice.
- Consider a future generic safeguard against duplicate workspace creation when provider data is temporarily incomplete.
- Revisit whether cmux should expose a true global `list-workspaces --all` mode upstream.
