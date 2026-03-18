# AttachableSet Lifecycle Design

## Summary

Define lifecycle rules for `AttachableSet` so that sets are scoped to their repo, stable across refresh cycles, cleaned up when their owning checkout is destroyed, and terminal pool sessions are torn down on deletion.

This builds on the identity model from `2026-03-16-attachable-set-identity-design.md` and addresses the gaps described in issue #385.

## Problem

Three concrete problems exist today:

1. **Sets never get removed.** The registry grows unboundedly. Bindings, attachables, and sets persist forever once created. The `disconnected_known_terminals` logic stops emitting terminals after missed scans, but never removes the underlying registry entries.

2. **Sets appear in every repo.** `list_terminals()` returns all shpool sessions globally. `project_attachable_data()` projects any set referenced by any terminal, regardless of which repo is being refreshed. A set created for repo A appears in repo B's view.

3. **Correlation flaps.** Set visibility depends on whether a terminal was observed this scan cycle. If shpool transiently fails or a session is missed, the set vanishes from the projection, breaking correlation. Next scan it reappears.

## Goals

- Sets appear only in the repo whose checkouts match the set's checkout path.
- Sets are stable in the projection regardless of terminal scan results.
- Sets owned by a checkout are removed when that checkout is deleted.
- Terminal pool sessions are torn down when their set is deleted.
- Orphaned shpool sessions (no matching binding) are reaped as a safety net.
- Keep the ownership model loose enough for future cross-host and composed sets.

## Non-goals

- Explicit owning-context model beyond checkout path (future, post-#256).
- Staleness-based auto-deletion of unowned sets.
- Non-terminal attachable lifecycle.
- Changes to the `AttachableSet` / `Attachable` data model itself (naming, fields).

## Design

### 1. Repo-scoped projection

Replace the current "referenced sets" filter in `project_attachable_data()` with a repo-checkout-match filter.

**Current behavior:** Walk `pd.managed_terminals`, resolve bindings to set IDs, collect referenced sets, project only those.

**New behavior:** Two steps in `project_attachable_data()`:
1. **Enrichment:** Resolve terminal and workspace bindings to populate `attachable_id` / `attachable_set_id` fields on `ManagedTerminal` and `Workspace` (unchanged from today).
2. **Set selection:** Iterate all sets in the registry, project those whose `checkout` HostPath matches any checkout in `pd.checkouts`. This replaces the "referenced sets" collection.

The match is on HostPath equality (host + path). For remote checkouts: a set created with a remote hostname will match when that remote host's checkouts appear in `pd.checkouts` (which happens when remote provider data is merged). A set for `desktop:/home/user/repo` will not match `local:/home/user/repo` — these are distinct checkouts on distinct hosts. This is intentional and correct.

This means:
- A set always appears in its repo's view as long as a matching checkout exists, regardless of terminal scan results.
- A set never leaks into another repo's view.
- Sets with no checkout (future, not currently created) are excluded from all repo views for now.

### 2. Ownership by checkout path

A set is implicitly owned by its `checkout` HostPath. No explicit owner field is introduced.

All currently created sets have both `host_affinity` and `checkout` populated at creation time. This invariant is maintained.

Future work may introduce an explicit owning-context to support cross-host or composed sets, but for now checkout path is the ownership link.

### 3. Cascade delete on checkout destruction

When `CheckoutManager::delete_checkout` succeeds, the executor:

1. Finds all sets whose `checkout` matches the deleted HostPath. (Currently 1:1 due to `ensure_terminal_set` deduplication, but the cascade uses a collection to allow future 1:N.)
2. For each set:
   a. Collects member attachable IDs and their terminal pool binding external refs (session names).
   b. Removes all bindings referencing the set or its members.
   c. Removes all member attachables.
   d. Removes the set.
3. Persists the registry once after all sets are processed.
4. For each collected terminal session ref, fires `kill_terminal()` on the terminal pool — best-effort, failures are logged but do not block the delete.

The checkout is already gone at this point, so the set removal is unconditional.

**New store APIs required:** `AttachableStoreApi` currently has no removal methods. Add:
- `remove_set(id: &AttachableSetId) -> Option<RemovedSetInfo>` — atomically removes the set, its member attachables, and all associated bindings. Returns `RemovedSetInfo` containing terminal pool binding external refs for teardown.
- `sets_for_checkout(checkout: &HostPath) -> Vec<AttachableSetId>` — query method to find sets owned by a checkout path.

**Executor interface change:** The checkout delete path (`build_remove_checkout_plan`) currently takes pre-resolved `ManagedTerminalId` keys. It needs access to `SharedAttachableStore` and the deleted checkout's `HostPath` to perform the cascade lookup. The step-plan closures will capture these.

**Workspace teardown:** Destroying workspaces that present the deleted set is deferred. On the next refresh, these workspaces will appear unbound. A future workspace-cascade can be layered on without changing the set lifecycle model.

### 4. Terminal pool as liveness probe

With the identity model in place, terminal discovery happens at workspace creation time (when bindings are persisted). `list_terminals()` becomes a liveness probe:

1. Query shpool for live sessions.
2. Match each against known bindings in the attachable store.
3. Update status: `Connected` if the session exists in shpool's list, `Disconnected` if not.
4. Return the full set of known terminals (from bindings) with updated statuses.

New shpool sessions with no matching binding are not adopted — they are orphans handled by the reaper.

### 5. Orphan session reaper

A periodic reap cycle in the shpool provider (running as part of the existing `list_terminals()` refresh):

1. List all live shpool sessions.
2. If shpool is unreachable (daemon down, socket error), skip the reap cycle entirely — do not treat an empty session list as "all sessions are orphans."
3. For each live session, check if a matching binding exists in the registry.
4. Any session with no binding is orphaned — kill it.
5. Log what was reaped. `kill_terminal()` on an already-dead session must be treated as success (idempotent).

This is safe because the shpool instance is private to Flotilla. "No binding = shouldn't exist."

This catches: failed explicit teardowns, daemon crashes, manual interference, leaked sessions from any cause. The cascade-delete path may have already killed some sessions; the reaper tolerates this gracefully since kill is idempotent.

### 6. Simplify disconnected terminal tracking

The existing `disconnected_known_terminals` / `missed_scans` tracking in shpool becomes unnecessary:

- **Before:** Tracked missed scans to decide when to stop emitting a `ManagedTerminal`. After `MAX_MISSED_SCANS`, the terminal was silently dropped from results but its binding persisted.
- **After:** Known terminals (from bindings) are always emitted. Their status is `Connected` or `Disconnected` based on the shpool session list. The reaper handles actual cleanup of orphans, and cascade delete handles cleanup when checkouts are destroyed.

The `missed_scans` HashMap, the `missed_scans: Mutex<HashMap<String, u32>>` field on `ShpoolTerminalPool`, and the `MAX_MISSED_SHPOOL_SCANS_BEFORE_REAP` constant can all be removed.

## Data Flow

### Refresh cycle (per repo)

```
list_terminals()
  ├── Query shpool for live sessions
  │   └── If shpool unreachable → skip reap, return known terminals as Disconnected
  ├── Match live sessions against known bindings → update status (Connected/Disconnected)
  ├── Reap orphan sessions (live in shpool, no binding) — idempotent kill
  └── Return all known terminals (from bindings) with updated statuses

project_attachable_data()
  ├── Enrichment: resolve terminal/workspace bindings → populate attachable_id/set_id fields
  ├── Set selection: iterate all registry sets, keep those where checkout ∈ pd.checkouts
  └── Project matching sets into pd.attachable_sets

correlate()
  ├── Sets always present (stable, not scan-dependent)
  ├── CorrelationKey::CheckoutPath merges set with checkout
  └── CorrelationKey::AttachableSet merges terminals/workspaces with set
```

### Checkout delete flow

```
delete_checkout(path)
  ├── CheckoutManager::delete_checkout(path) → success
  ├── Find sets where set.checkout == deleted HostPath
  ├── For each set:
  │   └── Collect member terminal session refs
  ├── Remove bindings, attachables, sets from registry
  ├── Persist registry (once)
  └── For each session ref:
      └── kill_terminal() — best-effort, log failures
```

## Terminal Pool Trait

No changes needed. The existing `kill_terminal(&self, id: &ManagedTerminalId)` method is sufficient for teardown. The executor already has access to the terminal pool through the provider registry.

The reaper uses the same `kill_terminal()` method for orphan cleanup.

## Migration

No data migration is needed. The registry format does not change. The behavioral changes are:

1. Projection filter changes from "referenced this cycle" to "checkout matches repo."
2. Cascade delete is new behavior on an existing action (checkout delete).
3. Orphan reaping is additive.
4. Missed-scan tracking is removed (simplification).

Existing registries will work as-is. Sets that have accumulated for deleted checkouts will not appear in any repo view (since their checkout won't match). Their shpool sessions (if any) will be killed by the orphan reaper. The set/attachable/binding entries themselves will remain as invisible ghosts in the registry — acceptable debt until a registry GC pass is added (not in scope for this design).

## Testing

- **Repo scoping:** Given sets for checkouts A and B, repo with only checkout A sees only set A.
- **Stable projection:** Set appears in repo view even when its terminals are not observed this scan.
- **Cascade delete:** Deleting a checkout removes its set, attachables, and bindings from registry.
- **Terminal teardown:** After cascade delete, `kill_terminal()` is called for each member's session.
- **Orphan reaping:** A shpool session with no binding gets killed on the next refresh.
- **No cross-repo leakage:** Set for repo A never appears in repo B, even if both have terminals in the same shpool instance.
- **Reaper skips when shpool is down:** When shpool is unreachable, no sessions are killed (the reaper does not treat empty results as "all orphaned").
- **Idempotent kill:** `kill_terminal()` on an already-dead session succeeds silently.
- **Cascade + reaper overlap:** After cascade delete kills sessions, the reaper on next refresh does not produce spurious errors for the same sessions.

## Related Issues

- #385 — Define AttachableSet lifecycle and deletion policy (this design)
- #378 — AttachableSet identity rollout (parent tracking issue)
- #360 — Remote workspace correlation through AttachableSetId
- #239 — Managed terminal pool with identity and lifecycle management
- #256 — Log-based architecture (will simplify cascading lifecycle in future)
