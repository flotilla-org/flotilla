# Presentation and attention: demands, regards, and the enrichment projection

**Status:** Accepted
**Date:** 2026-07-22
**Relates to:** ADR 0014 (curated scoped queries — the projection's delivery
substrate), ADR 0017 (attention observations — the work-side plane this ADR
routes to people), ADR 0009 (Yeoman, whose charter this ADR turns into a data
join), issue #813 (the grill), #808/#835/#843 (restated or dissolved below),
cleat#7 (sibling sessions — the establish-crew primitive).

The stage-5 `Presentation` resource conflated two different things, visible
directly in its shape: a convoy-stamped "look at this" issued unconditionally
at creation, and a realization tracker whose status fields assume a single
presentation manager. Dogfooding with multiple simultaneous surfaces
(andamento in zellij, wheelhouse desk and kiosk, flotilla-viz, ad-hoc
governor sessions arriving through git-watcher) broke both halves at once.
The #813 grill replaced them with the model below.

## Principals and the two attention concepts

A **Principal** is a person whose attention the system routes and economises.
Agents are never principals (persistent agents may later join their activity
data into the same monitoring channels — as a data join, never as
workspace-indication). The stack economises exactly two scarce resources:
agent tokens (#838) and principal attention (this ADR).

**Demand** — work-side. A call on a principal's attention: the work cannot
progress without a person (permission prompt, workflow human-gate, review or
demo approval). Queue-natured: raised, routed, satisfied/acknowledged.
Derived from ADR 0017's attention plane (`NeedsInput ∨ Idle ∧ unsettled`)
and from explicit gates. Never itself a phase.

**Regard** — principal-side. An intention to be looking at a thing:
*expressed* (attach, focus, opening a latent) or *implicit* (policy-emitted,
e.g. at convoy creation — expected to narrow as trust grows; per-project,
steward-settable). Regards **decay** when the target leaves actual focus
across the principal's surfaces; a **pin** is a regard that never expires.
The live regard set is the principal's **searchlight**. Any "working set" is
derived from the searchlight, never curated as a separate list.

Both are mesh-resident, per-principal, for two load-bearing reasons beyond
telemetry: demand routing is a join against the addressee's regards
(in-searchlight demands surface in place; out-of-searchlight demands
escalate — Yeoman's charter stated as a data join), and regard decay
aggregates focus observations across all of a principal's surfaces, so no
single surface can compute it.

### Regard targets and coverage

A regard targets any resource ref — vessels and convoys first; projects
legal but diffuse; **artifacts/cargo are first-class targets** (a
demo-approval demand points at an artifact, and satisfying it is a spell of
regard on that artifact — artifact-regard telemetry is "what actually got
human review" governance data). Coverage is the target's **ownership
subtree, downward only**: regard a convoy and its vessels'/crews' demands
surface in place; regard one vessel and its siblings are *not* covered (deep
focus on one try-N-ways attempt must not swallow the others' demands). No
sideways, no upward.

### Demand addressing

Three tiers: (1) default addressee is the **dispatching principal** —
dispatch provenance becomes a recorded convoy field; (2) workflow
human-gates may address explicitly (a principal ref; later a role);
(3) unroutable demands land in a **project-scoped pool** that stewards
route — escalation is a steward decision; never dropped, never broadcast.
Rejected: addressee-less demands routed purely by "whoever is looking" —
fails exactly when nobody is. Direction, not v1: selector-ish addressing
over principal attributes ("German speaker", "has-the-product-owner-baton" —
batons being transferable attributes so addressing survives handoffs),
stamped into workflows by their initiators.

### Focal and ambient surfaces

Overview-ish surfaces (a kiosk grid, flotilla-viz) are **ambient**: they
neither emit nor refresh regards — displaying everything is not regarding
everything. Only focal interaction (attach, current-tab, opening a latent)
touches the regard lifecycle. Ambient surfaces are pure projection
consumers; flotilla-viz is the deliberate degenerate case.

## The enrichment projection

One demand-backed, watchable query family (ADR 0014) — not part of any
presentation spec, and not a fleet firehose: aggregation runs only for
watched scopes. It serves two consumption bands:

- **Awareness band** (sidebars, latents): browse-scoped — everything a
  principal *could* regard: convoys active or not, ready issues, projects
  and repos with no convoy activity. Fleet-wide by **composition** (a fleet
  grouping over projects), and the same query set the TUI project page
  renders as panels — the sidebar and the project page are two renderings
  of one family. Shallow decoration; windowed/limited so surfaces elide,
  collapse, and truncate freely.
- **Regard band**: the deep enrichment tree, searchlight-driven, entitled
  to **materialization** (real terminal attachment).

A **latent** is an awareness-band entry rendered with presence but no
attachment. **Opening a latent is expressing a regard** — sidebar click →
regard emitted → materialization. Discovery and attention are one mechanism
joined at the regard.

Structure and vocabulary:

- Responses are trees: **grouping nodes** (convoy: label, summary counts,
  convoy-scope refs) containing **entries** (vessels/crews: label, phase,
  attention state, refs, links). Fields live on the node they belong to —
  the #808 class of bug (convoy-scope summary stamped on a vessel tab)
  becomes unrepresentable rather than discouraged.
- A governed **typed core** vocabulary (refs, states, counts, links) in the
  protocol crate, plus **namespaced annotations** (free-form k=v) for
  experiments, with a promotion path into the core.
- **Total-fallback conformance rule**: every surface MUST render any
  well-formed node/entry knowing nothing — label + kind, a default template
  for unknown grouping keys, never broken layout, never invisibility.
  Testable per surface.
- **Salience is computed centrally** (`none/info/attention/urgent` — the
  demand-join lives mesh-side, once), **rendering is owned by surfaces**
  (red dot, blink, ship on fire). Badges are renderings of salience plus
  typed fields, never projection objects.
- Every node carries `as_of` (ADR 0017 style). Demand surfacing rides the
  demand queue, never this projection — the projection may be lazy;
  demands may not.
- Grouping-header display details are surface config: timestamp rendering
  uniform per surface (all sections or none); the canonical section address
  on the right, hidden behind an expand affordance by default.

### Grouping is derived, never stamped

Sessions and tabs are stamped with **immutable identity facts** at creation
(vessel ref, checkout path, repo, cwd) — what it is, never where it files.
Grouping nodes are computed projection-side by a join over those facts: the
correlation-engine philosophy applied at the presentation layer. Ad-hoc
sessions observed by git-watcher join the same project grouping as
convoy-born tabs by construction. Runtime-switchable grouping (by project /
convoy / host / attention) is a different join over the same facts — a
surface-mapping setting, no re-stamping.

## Realization: the searchlight is the desired state

No per-surface desired-presentation resource for interactive surfaces. Each
focal surface holds a local **mapping** (its config: how the searchlight and
which awareness scopes project into its idiom) and converges continuously —
awareness queries → latent presence; searchlight → materialized attachments;
materialize missing, retract expired, latent the marginal. Reconcile-on-
connect (#835) is this convergence run at connect time.

Five refinements from the openable-latent and fact-semantics grills
(2026-07-23):

- **Facts use one dialect, with one meaning per key.** Project and Repository
  identity are separate grouping levels: `flotilla.project` is a Project
  resource name and exists only with Project knowledge; `vcs.repo` is the
  canonical forge slug, with the Repository's `host:path` identity as the
  slugless fallback. A Repository never masquerades as a Project. Producers
  that know the same Repository use the same `vcs.repo` value, so their
  observations join there.
- **Every producer publishes provenance as data.** Each assertion carries a
  `source` fact (`flotilla`, `git-watcher`, and so on), distinct from the
  patch protocol's authority-scoped `source_id`. Surfaces may therefore badge
  provenance without interpreting transport bookkeeping.

- **Latent tabs are a sanctioned rendering**: a surface may draw latents
  *in place in its working-set idiom* — a dimmed tab where the live tab
  would be, carrying the entry's metadata ("this tab could exist; here is
  its metadata"). Latent-vs-live is the surface's join: an entry whose
  group has a materialized member carrying the identity is live. The
  constraints stay: presence data comes from awareness queries, and only
  materialized attachments are working-set members — latent tabs are
  indicators, not curation.
- **The materialize recipe is an address, not a promise.** The capability
  fact gates *listing* on best-known state; the recipe (`flotilla attach
  --host <host> <ref>`) preserves the advertising host as part of its
  address, re-resolves through the live hop chain at execution, and fails
  honestly. For remote-placed vessels the fact derives from replicas
  (ADR 0016), so it is stale-affirmative by up to the watch latency —
  bounded by minting **only while the origin peer's route is live**.
  Recipes vanish on peer disconnect; hours-stale affirmation is a lie,
  seconds-stale is honest.
- **"Disconnected" becomes a visible state eventually** — as an
  annotation-tier fact on the affected entries (the presentation twin of
  marking peer hosts unready), never a new member of the frozen
  `status.state` vocabulary. The proposed fact key is
  `status.connectivity=disconnected`; until built, recipe absence is the
  only signal.

Surfaces report **realization and focus observations** back to the mesh —
one channel feeding regard decay, demand routing, and attention telemetry —
which preserves observability without maintaining a mirror desired-layer
whose only writer is its own reconciler. The two-level spec→reconcile
pattern instantiates **only where a second actor writes the desired layer**:
a remotely-driven surface (Yeoman-reshaped kiosk, reboot-recovering TV) gets
a per-surface-instance spec resource. The door is open; nothing is built
until that actor exists. This cut is deliberately cheap to reverse: the
convergence loop is identical either way, and a desired-layer resource is
additive later.

Accepted cost: there is no `resource get` for "what should the desk be
showing" — debugging a surface gap means comparing regards (mesh) against
realization observations (reported), via the raw inspection CLI (#847).

**A pane is not presentation.** The "open a shell/tool pane here" intent is
ad-hoc **crew addition**: (1) establish crew — a new process in the vessel's
execution context, direct or cleat-mediated (the sibling-session primitive,
cleat#7), inheriting container/namespace/cwd/env; then (2) attach the local
pane to that crew. Surfaces never create processes.

## Consequences

- The stage-5 `Presentation` resource and its replace-on-change reconciler
  retire. #843 dissolves: presence is pulled by connected surfaces, not
  pushed at hosts, so there is nothing to realize on surface-less hosts and
  no presentation homing question.
- v1 implementation order: PrincipalAttention resource (regards + demands),
  regard emission and decay wired to attach/TUI/sidebar, projection query
  family v1, surface conformance (total fallback), central salience.
- Attention telemetry (regard/focus history correlated with hook, session,
  fs, and cleat logs — where is human attention concentrated; focus
  training, tool development, and information exposure there) is the human
  twin of #838's token-burn plane, and lands on the same observation
  channel this ADR creates.
