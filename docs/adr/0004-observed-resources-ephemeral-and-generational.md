# Observed resources are ephemeral and generational; lifecycle authority selects the backing store

Observed facts (open PRs, assigned issues, local checkouts, running agents) are
contributed into the **same resource store and watch/aggregator interface** as
managed resources — but **lifecycle authority (ADR 0003) selects the backing
store**:

- **Observed / Adopted → ephemeral, in-memory backing.** Not persisted. The
  source of truth is the external system; the resource is a published projection
  of it.
- **Managed → durable backing** (sqlite, per PR #618). It carries desired state,
  so it must survive restarts with a continuous `resourceVersion` log.

Both are reached through the same `TypedResolver` / list / watch API — a surface
consumer (the dashboard, an aggregator, a remote replica) does not care which
backing a kind uses.

## Generations

The ephemeral observed store is **generational**. A daemon restart discards
observed state and begins a **new generation**, repopulated by a full provider
refresh. Therefore:

- `watch(FromVersion(v))` is valid only *within* a generation.
- The watch/snapshot stream for ephemeral kinds carries a **generation marker**.
  A consumer (including a federated replica tailing the log over the Tender) that
  sees the generation change must discard its view and re-list from the new
  generation — it must not expect version continuity across a restart.
- The durable managed log has no such reset: its `resourceVersion` is continuous
  across restarts.

## Why

- Observed facts are high-churn, derived, and have **no desired state to
  reconcile**. Modelling them as durable, optimistically-versioned objects would
  tax every poll and treat facts as if they were intentions.
- Correlation fell down precisely because, lacking a real state store, it assumed
  all relationships could be re-inferred from whatever currently exists. Making
  observed items explicit resources fixes the source-of-truth problem; making them
  ephemeral keeps them honest about *where* that truth lives.
- One interface (uniform watch/aggregator/federation) means the dashboard reads
  one place (S1) and remote observed work federates for free (S2, ADR 0002),
  without paying durability costs for ephemeral data.

## Consequences

- Lifecycle authority and durability are **deliberately coupled**, not orthogonal.
- The watch/snapshot protocol needs a generation marker on ephemeral streams.
- Aggregators must treat observed views as rebuildable, not as a continuous log.
- **Deferred (S5/S6):** when "adopt" and user-driven "grouping" land, the durable
  *intent* ("I adopted/grouped these") is a small separate **Managed** resource
  that references the observed ones — not a change to the observed resource's
  ephemeral backing. Today, Adopted is treated like Observed (ephemeral).
