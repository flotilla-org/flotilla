# Curated scoped queries; demand-backed materialization

**Status:** Accepted
**Date:** 2026-07-14
**Relates to:** ADR 0011 (named-query result sets — this ADR grows `QueryId`
into parameterized families and adds a second materialization class), ADR 0012
(tables — this ADR **amends** its generic-tier section), ADR 0006 (resources
vs reference data — this ADR supplies reference data's query mechanism),
issue #666 (v1 table slice, rewritten against this ADR), issue #633 (M1
dogfood — the user stories this was derived from).

Pre-implementation review of #666 surfaced that the generic `Resources{kind}`
tier silently required an untyped raw-JSON list/watch API on the resource
backend and turned `QueryId` from a finite enum into an open family — pulling
against ADR 0011's typed-rows decision. Re-deriving the view layer
scenario-first from the M1 user stories (add a project; see a project's
activity including issues; start a convoy from an issue; drive it all from a
Presentation Manager) showed the generality was not load-bearing: every story
needs a *specific, typed* query. Separately, the issues story forced a second
look at what the Aggregator materializes: repos have thousands of issues and
flotilla must not become a shadow issue tracker.

## Every query family is curated and typed

The v1 view layer is built exclusively from **curated typed query families**:
`convoys{scope}`, `independents{scope}`, `checkouts{scope}`,
`issues{scope}`. Rows stay typed structs per ADR 0011; adding a family is a
`Rows` variant, a row type, and a registry entry. The mechanisms (subscription,
deltas, registry, generated actions) are generic; the code per kind is
specific. There is no untyped list/watch on the resource backend and no
raw-JSON row type.

**The generic `Resources{kind}` tier of ADR 0012 is repositioned as a
possible future tier, not part of v1.** If it is ever built, its
representation is **columnar** (a schema plus parallel column arrays —
structure-of-arrays), not variant rows: a generic consumer renders columns
positionally and never destructures per-kind row enums. Nothing in v1 may
presuppose it.

## `QueryId` is a family plus owned scope parameters

`QueryId` grows from a finite `Copy` enum to family + owned parameters, where
the parameters are **scope**: a project reference or repo key. A project
scope expands to its constituent repos inside the Aggregator — consumers
never learn a project's composition. Scope rides the View address per ADR
0013's `?key=value` channel. Global (unscoped) forms remain for fleet-wide
tables. Subscription cost scales with what is on screen. For issue queries,
a Project-level **Issue Source** override supplies one unified feed; without
one, Project scope expands to each constituent Repository's Forge-backed issue
source. The absence of a configured source is surfaced as unavailable, never
as an ordinary empty issue set.

## Two materialization classes, one wire contract

- **Store-backed** queries (`convoys`, `independents`, `checkouts`):
  incrementally maintained over the resource store; always warm, always
  complete, flotilla-authoritative.
- **Demand-backed** queries (`issues`, later `change_requests`): materialized
  by the Aggregator **only while subscribed**, fetched from the external
  system of record, discarded on unsubscribe. Nothing persists.

Both classes speak the same `ResultSet`/`ResultDelta`/seq contract, so
consumers cannot tell them apart except through set metadata: demand-backed
sets carry `as_of` (staleness is honest by construction) and `has_more`.

A demand-backed materialization is a **window** (first page, updated-desc),
kept fresh while subscribed via the provider's incremental-sync verbs.
**Fetch-more is an intent on the result set** — the materializer appends the
next page as ordinary deltas; paging state lives server-side and consumers
never stitch pages. Beyond the window you search (a separate ephemeral scoped
query), not scroll.

The Plane-A provider implementations (`IssueProvider`: paged list, search,
fetch-by-id, changed-since with ETag handling) are **reused as the fetch
layer** under the demand-backed materializer. What Plane A's deletion
condemns is the consumption pipeline (correlation → WorkItem → Snapshot),
not these adapters.

## Issues are never replicated

The Forge or Project Issue Source is the system of record. Flotilla holds
exactly two forms of issue data: **references-on-work** — a source-qualified,
opaque external ID associated with the relevant repo key, plus a small display
snapshot (title, state, `as_of`) so boards render offline, bounded by active
work — and the transient demand-backed windows above. GitHub's `728` and
Linear's `WIDGET-123` are both opaque IDs; no model assumes issues are numeric
or that the Forge must supply them. Triage/board intelligence ("what should I
or the governor pick up next") is add-on-program territory consuming these
queries, never core features. Growing issue mutation/comment/label features in
core is the reimplementing-the-tracker failure mode this section exists to
forbid.

## `independents`, not `sessions`

The free-floating-sessions query is renamed **`independents`** (rows
`IndependentRow`), after the convoy-era term for ships sailing outside any
convoy. "Session" is maximally overloaded (cloud agents, cleat, tmux, zellij)
and was already attracting misuse. The underlying resource kind remains
`TerminalSession` — at the resource layer the word is literally correct. The
exclusion rule stands: a session appears under its convoy's vessel rows *or*
in `independents`, never both.

## Scope keys are mortal

Scopes are made of repo identity, so repo identity becomes a real referent:
**Repository is a resource** (convergent-facts class per the #688 model),
natural-keyed by normalized canonical remote. Remote-less repos use the stable
Host identity plus normalized Git common directory, so worktrees of one local
repo do not mint separate identities. Machine-local SSH aliases are resolved
through host configuration; an alias that cannot be resolved is an
"unrecognised remote", never a globally meaningful repo key.

Repository remains genuinely convergent: its declaration holds immutable
canonical identity and derived Forge identity; its status is a provenance-
carrying union of default-branch observations and references to all associated
Checkout resources, grouped by Host. Mutable human choices do not masquerade
as convergent facts: branch overrides and the optional single Issue Source
belong to the definitions-class Project. Repository persists with empty status
when no checkout exists, including cloud-only Projects between ephemeral
clones.

Storage-safe typed Repository keys derive from the canonical identity, and
every lookup verifies the referent. Remotes nevertheless move as ordinary
project lifecycle (transfers, renames, forge migrations), so the model commits
to **`moved` pointers (old key → new key) and referent repair**; no referent
shape anywhere may assume repo keys are immortal. The repair mechanism is
built when first needed; the commitment is binding now.

## Consequences

- Codex's pre-#666 hidden dependencies on an untyped backend API dissolve;
  the `QueryId` change shrinks to owned params on curated families.
- Each new listable kind costs a deliberate design moment (row type, columns,
  actions). That is a feature: every table someone sees was designed.
- `Sessions` → `Independents` is a mechanical rename sweep (no compat).
- Demand-backed metadata (`as_of`, `has_more`) is a protocol addition;
  store-backed sets simply omit it.
- The fleet-staleness column #666's acceptance mentions is satisfied
  per-class: demand-backed rows via `as_of`; store-backed rows via the
  federation/replica-cache staleness landed with #632.
