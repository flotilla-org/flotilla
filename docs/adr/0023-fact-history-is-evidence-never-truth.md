# Observed-fact history is evidence, never truth

**Status:** Accepted
**Date:** 2026-07-27
**Relates to:** ADR 0004 (observed resources are ephemeral and
generational — this ADR governs what may be kept *about* them), #1051
(the ops grill that fixed this boundary), #940 (trace-driven stewards,
the intended consumer), #748 (object store, the eventual home).

Observed facts are ephemeral and non-durable by design (ADR 0004). There
is nonetheless a real case for logging their history as timeline and
analytics material: explaining agent behaviour after the fact, spotting a
flapping fact, feeding trace-driven maintenance.

## The boundary

Such a history is **evidence for analysis, never a source of truth**:

- Nothing in the control plane may read fact history to make decisions.
  Reconcilers, admission, and lifecycle act only on current observed and
  declared state.
- The history may inform *humans* and *analysis agents* (stewards,
  post-hoc debugging), whose outputs re-enter the system only as ordinary
  declared changes — a steward that spots a flapping fact files a change;
  it does not replay history into the control path.
- "We should keep the history" must never quietly become "the history is
  authoritative". This ADR exists so that drift gets caught.

Everything else — retention, granularity, storage (plausibly the #748
object store), schema — is deliberately undecided until the
trace-analysis direction (#940) needs it.
