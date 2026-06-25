# The main view is a surface-agnostic composition of panels

**Status:** Accepted
**Date:** 2026-06-25

The main view is modelled as a **View Model**, not a fixed repo-centric table:

- **A tab is a composition of panels.**
- **A panel is a filtered view over one resource kind/query at a scope** —
  `(kind or query, scope, columns, available intents)`. Scope may be a **Project**
  (island) or **fleet**-wide. Examples for a Project dashboard: *Convoys (active
  work)*, *Open PRs*, *Assigned issues*, *Local checkouts*, *Running agents*.
- **A row is a resource.** A **Convoy** row drills into its **Task** DAG (the
  detail/tree the existing convoy view already renders).
- The View Model is **surface-agnostic**: the ratatui TUI renders panels as its
  split-table, **uishell** as its Panel/View, a web UI as cards/tables.

This subsumes both the old repo-page (its per-`SectionKind` split becomes panels,
no longer correlation-derived) and the separate Convoys tab (now just a default
panel/composition).

**Configurability:** the *shape* is composable now (a tab genuinely is a set of
panels), but near-term we ship **fixed default compositions** (a per-Project
dashboard, a fleet overview, the convoys view). User/agent (Yeoman) composition is
deferred — and supported by the shape without re-cutting data. A tab is not 1:1
with a repo (issue #518).

## Why

- It is the only model where "one place for what's relevant" (S1) survives
  alongside convoys, observed resources, and cross-host scope (S2) — and where
  observed and managed resources render through one mechanism.
- Getting the *shape* right is substrate work (it's what #614 produces and every
  Surface consumes); leaving *who composes panels* as an app-layer capability lets
  it grow toward #518 / uishell / Yeoman without a data re-cut.

## Consequences

- #614 produces a panel View Model from the **Aggregator**; the TUI split-table
  becomes one renderer of it.
- The separate `TabId::Convoys` tab folds into being a default composition.
- The current per-`SectionKind` column definitions are reworked as panel View
  Models (style-agnostic columns + intents), feeding the Phase-6 shared view-model
  extraction.
