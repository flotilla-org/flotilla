# Observed, adopted, and managed resources share one store; lifecycle authority is per-resource

**Status:** Accepted
**Date:** 2026-06-21

Observed reality and desired state live in **one resource store and the same
kinds** (ADR 0001's model), distinguished not by a binary observed/desired flag
but by a **lifecycle-authority** property on each resource:

- **Observed** — Flotilla only reports the resource exists. It reconciles nothing.
- **Adopted** — Flotilla acts on the resource for specific, requested purposes
  (open a PR from it, attach a Convoy to it) but does **not** own its lifecycle.
  The canonical case: a user's hand-managed local clone in their favoured `~/dev`
  directory, kept "forever," from which they submit PRs (e.g. open-source
  contributions). Reconcilers must never move, recreate, or garbage-collect it.
- **Managed** — Flotilla provisioned the resource and may destroy/GC it (e.g. a
  Convoy's throwaway worktree + container).

Adoption is a **promotion along the authority axis**, never a kind change.

## Why

- **Correlation only existed because Flotilla didn't create the work.** Union-find
  over `CorrelationKey` is *inference*. For Flotilla-launched (Convoy) work,
  linkage is known by construction via owner references / labels — no inference.
  Union-find is therefore **demoted** from a core always-on service to an on-demand
  **Aggregator** utility used only for purely-observed state (and possibly agentic
  later). It is demoted, not deleted.
- **Providers stop emitting fragments-for-correlation** and instead **contribute
  observed resources** into the shared store.
- One store + one set of kinds + a lifecycle-authority axis is what makes the
  Plane-A/Plane-B unification (observe + orchestrate) real, and gives users a
  smooth on-ramp: start with existing work (observed), let Flotilla act on it
  (adopted), opt into Flotilla-managed workflows (managed) — without any object
  ever changing type.

## Consequences

- Every reconciler must check lifecycle authority and **refuse to
  create/destroy** observed/adopted resources — it may only perform explicitly
  requested operations on them.
- The legacy `ProviderData → correlation → WorkItem → Snapshot` core pipeline is
  dismantled; the TUI work-item table becomes an **Aggregator view over the
  resource store**, not a consumer of core-correlated snapshots.
- A user's local clone is a first-class, permanently-adopted resource — never a
  second-class "external" thing bolted on.
