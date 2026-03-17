# Terminal Scan Authority Design

## Summary

Terminal scans should report liveness, not create identity.

For owned terminal pools like shpool, attachable identity should come from persisted bindings and attachable sets created during prepare/workspace flows. A later scan may confirm that a known terminal is present, absent, or disconnected, but it should not mint a second synthetic attachable set from fallback paths like `.flotilla/attachable/<checkout>`.

## Problem

The current shpool refresh path scans sessions and calls `register_attachable()` for every observed terminal. When shpool cannot report a working directory, the code falls back to a synthetic checkout path:

- `.flotilla/attachable/<checkout>`

That fallback path differs from the real checkout path used by workspace binding persistence. The result is two attachable sets for the same logical checkout:

- one set created by prepare/workspace persistence using the real checkout path
- one set created by scan-time registration using the synthetic fallback path

This produces duplicate correlated items and splits terminal correlation away from the workspace-bound checkout.

## Desired Behavior

### Identity

- Persisted attachable sets and bindings are authoritative.
- Terminal scans must not create new attachable sets for unknown or unbound sessions.
- Unknown scanned terminals should be logged only for now.

### Liveness

- Known terminals should remain in provider data even when missing from one scan.
- On first miss, mark them `Disconnected`.
- Remove them from live provider data only after a grace period based on repeated misses or timeout.

### Lifecycle

Attachable-set deletion is out of scope for this change. That will be handled in a follow-up issue covering:

- explicit user deletion
- cascade deletion from owned checkout/workspace teardown
- staleness/timeout policy for future unowned or cross-cutting attachable sets

## Design

### 1. Stop scan-time identity creation for unknown shpool sessions

Change shpool refresh so scan-time registration only updates attachable state for sessions that are already known via persisted binding.

Practical rule:

- if `lookup_binding("terminal_pool", "shpool", Attachable, session_name)` succeeds:
  - update the existing attachable and associated set membership/state
- otherwise:
  - log the unknown scanned session
  - do not create a new attachable or attachable set

This preserves the ability to reconcile known sessions while preventing synthetic duplicate sets.

### 2. Track known-but-missing terminals through a grace period

When a previously known terminal is absent from a scan:

- keep it in projected provider data
- mark status as `Disconnected`
- increment a missed-scan counter or evaluate a `last_seen_at` timeout

Only reap it from live provider data after the grace threshold is exceeded.

This should affect terminal presence only, not attachable-set persistence.

### 3. Preserve current UI flexibility

Disconnected terminals should stay in the data model even if current UI code does not render them prominently. Future UI can decide how to display disconnected terminals without reworking backend state semantics.

## Testing

Add regression coverage for:

1. shpool scan encountering an unknown session:
   - does not create a new attachable set
   - does not create a new attachable binding

2. shpool scan encountering a known session:
   - updates the existing attachable
   - does not create a second set

3. known session missing from scan:
   - remains in projected provider data as `Disconnected`
   - is reaped only after grace-threshold behavior is met

4. in-process daemon scenario:
   - workspace-bound checkout plus known scanned terminal stay on one attachable set
   - no synthetic standalone attachable-set duplicate appears
