# Tables are the consumer-side view layer: two tiers over named-query result sets

**Status:** Accepted
**Date:** 2026-07-13
**Builds on:** ADR 0011 (the data plane this consumes), ADR 0005 (whose
composition view model this houses consumer-side)

Decides the generic table/view layer (#666), grilled scenario-first with
Robert 2026-07-13. Tables are the TUI's (and later any surface's) generic
answer to "show me these resources" — the presentation-side consequence of
ADR 0001's k8s-isomorphic model.

## Two tiers, one grammar

- **Curated tier**: named queries (`convoys`, `sessions`, …) — typed rows,
  joins, fleet merge, capability fields, hand-tuned column specs.
- **Generic tier**: *any* resource kind in the store gets a serviceable
  table for free — auto-derived columns (name, namespace, origin host,
  authority, age, shallow spec/status projection) and a describe view of
  the full object. This is also the substrate-debugging surface (the
  #665/#672 investigations, in the TUI instead of sqlite3).

Same table widget, same interaction grammar; the tiers differ only in
where rows and column definitions come from.

## The generic tier is a parameterized query family

`QueryId` grows a parameterized member (`Resources { kind }`); query
identity becomes *family + params*, refining ADR 0011's "identity derives
from the typed `Rows` variant" (the variant carries the params). Generic
rows ride the same `SubscribeQueries`/`seq`/delta/resubscribe machinery.
`ResourceRow` carries an **origin-host column from day one**; rows may be
local-only until replicas serve arbitrary kinds — fleet coverage arrives
without a wire change. Tables are host-filterable but never inherently
machine-scoped.

## Hierarchy is a navigation stack, never a tree-in-table

Flat tables; **Enter drills** into a scoped table (drill target declared
per kind, scope taken from the parent selection); Esc pops; breadcrumbs
show the path. One cursor regime — the #664 focus-trap class is
structurally excluded. DAG framing survives as *ordering + a `depends_on`
glyph column* in the scoped table, not as a tree. Describe (`y`) is the
orthogonal drill-*in* (full object). A **Miller-columns cascade** (k
adjacent stack levels, selection in pane *i* re-scopes pane *i+1*) is a
*layout mode over the same stack* — later, and generic for free.

## Per-kind config is a code registry in the view-model layer

A declarative registry per query family — `ColumnSpec` (label, typed
extractor, width hint, alignment, severity rule) and `ActionSpec` (label,
key, capability-field guard, executor) — living in the surface-agnostic
view-model layer (the factored-not-frozen extraction target), not in
ratatui code. Adding a curated view = one registry entry, no bespoke
widget. User TOML overrides come later (the spec shape allows naming,
reordering, hiding); **layout never lives in the store and never crosses
the wire** — either would re-conflate what ADR 0011 separated.

## Actions are generated, capability-guarded, and honest

The `.` action menu is derived from `ActionSpec`s whose guards match the
row's capability fields (`attach: Some(_)`, `complete_work`). Nothing
renders that does not work — the #664 failure mode stated as a rule.

**Attach is two-axis**: *granularity* from the row kind — session → pane,
vessel → workspace (matching the PM contract's
`materialize.target = workspace | pane`; workspace ≡ zellij tab ≡
wheelhouse Workspace) — and *strategy* from environment: suspend-and-exec
in the same terminal as the universal baseline (degrading
workspace-attach to the vessel's primary crew session), PM pane/tab
materialization when a PM is detected (also the #667 pane→identity
stamping moment), embedded views as a possible later strategy. Convoy
rows carry no attach. The table path and the connector path (latent
workspaces materialised from the PM rail) converge on this same machinery
through different doors.

The TUI never auto-starts a PM connector: the connector is PM-session
infrastructure (started by the PM layout or explicitly). The TUI may
*detect and surface* its absence.

## Tables are monitoring and exception surfaces, not launch surfaces

No create intents on tables, ever. Work starts from issues on a
project/island scope tab, an explicit workflow launch, or a
governor/bosun interaction. **Auto-attach-on-create stays the default**
presentation expectation for interactively-started work (the Plane B
successor of attachableset auto-attach); auto-initiated work (e.g.
Governor-kicked convoys) surfaces as *latent* workspaces in the PM rail
instead. Interactive attach from tables is the exception path — recovery,
cross-host inspection — and must work, but is not the daily verb.

## Composite scope tabs are the repo view's successor

A tab may be a *scope* (project/island/repo) plus a declared composition
of queries sharing that scope param — one address, default compositions
shipped in the registry so scope tabs are functional-by-default. This is
ADR 0005's composition-of-panels living consumer-side. The Plane A repo
views are untouched by #666 and retire with the observer reshape (#616):
observed resources land in the store, get generic tables for free,
curated queries where they earn them, and the default project/repo
composition replaces the repo view *then*. No `workitems` query is ever
built — the parity bar is a measurement, not a migration.

## Filtering

Ephemeral fuzzy filter (`/`) per table, plus **structured scope params**
(host, project, phase-class) that live in the query scope and therefore
in the tab address — a filtered view is deep-linkable (the PM scoped
pane). Saved queries = named addresses persisted in user config, later.

## Consequences

- #666 v1: table widget + navigation stack; curated `convoys` + `sessions`;
  generic tier (local rows, origin column); generated action menu with
  two-granularity attach (exec baseline + zellij pane upgrade); `/` filter
  + host/project scope params.
- Recorded follow-ons: cascade layout, TOML overrides, saved addresses,
  composite scope tabs, fleet rows in the generic tier, connector-absence
  hints.
- Acceptance re-weighted to monitoring/recovery: a day of convoy-driven
  work where the tables are how you observe and how you recover, while
  normal starts flow through auto-attach without touching them.
