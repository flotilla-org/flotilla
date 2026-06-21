# Flotilla Transition Roadmap

This is the plan for getting Flotilla out of its current two-plane straddle and
onto a single coherent track. It is the "where we're going and in what order"
companion to the glossary ([`/CONTEXT.md`](../CONTEXT.md)) and the decisions in
[`docs/adr/`](adr/).

## The situation

Two complete data planes live inside one daemon and barely touch:

- **Plane A — the fleet observer** (`flotilla-core` + `flotilla-tui` +
  `flotilla-daemon`, ~93k LOC, the bulk of recent commits): providers →
  correlation (union-find) → WorkItems → snapshots → TUI. *Descriptive.*
- **Plane B — the control plane** (`flotilla-resources` + `flotilla-controllers`
  + `flotilla-commands`, ~8.6k LOC, newest work): k8s-isomorphic resources →
  reconcilers → projection → convoy view. *Prescriptive.*

The destination: **Plane B subsumes Plane A's prescriptive role**, with a
long transition. Observed reality and desired state share one resource store
(ADR 0003); correlation demotes to an on-demand Aggregator; convoys become the
unit of launched work.

## Decisions already recorded

- **ADR 0001** — k8s-isomorphic resource model; k8s is a backend, not the API
  contract. Always keep a k8s representation (B); defer a k8s-compatible API edge
  (A). Typed structs are the spine.
- **ADR 0002** — multi-host is resource-store federation, not peer-merge. A slim
  per-host **Tender** owns transport + forwarding; the bespoke peer-merge layer is
  retired.
- **ADR 0003** — observed / adopted / managed resources share one store and one
  set of kinds; lifecycle authority is a per-resource property. The user's
  hand-managed local clone is permanently *adopted*, never reconciled away.

## Two freezes (the most important near-term discipline)

1. **Plane A is bugfix-only.** No net-new features on correlation, WorkItem, the
   old `ProviderData → WorkItem → Snapshot` pipeline, or peer-merge. The
   151-vs-22 commit imbalance *is* the straddle; stopping it is the highest-value
   move.
2. **The TUI is *factored*, not frozen.** Both the TUI and uishell stay relevant
   ~70% of `flotilla-tui` is legitimate ratatui rendering; ~30% is a
   surface-agnostic domain/view-model layer (intent/action engine, declarative
   tables, the data→view-model pipeline) that should be extracted and shared.
   Stop *duplicating* that layer per surface; do keep the TUI itself maintained.

## Sequencing

The rule that resolves bottom-up-vs-top-down: **freeze the doomed plane
immediately, but extract generic pieces only after Flotilla has proven the
boundary by consuming them.** Keep extractions as workspace crates in this repo
until boundary-proven, *then* promote to separate repos (as cleat/porthole are).

| Phase | Work | Why here |
|------|------|----------|
| **0** | **Freeze Plane A** (bugfix-only). | Stops the bleed; costs nothing to start. |
| **1** | **Non-k8s backing store** (sqlite/libsql) + **lift the k8s projection into the resource model** (ADR 0001-B). | Precondition for dogfooding convoys without a live cluster. Low conceptual risk — semantics are proven. Makes the sqlite store a near-mechanical port of the in-memory backend. |
| **2** | **Convoy as the real end-to-end launch path** (create → provision → present → TUI), dogfooded. | Proves the resource/reconciler/provisioning boundaries *in the real consumer* before any extraction. |
| **3** | **Reshape the observer**: providers contribute *observed resources*; add lifecycle-authority; build the **Aggregator** view; delete the old `ProviderData→WorkItem→Snapshot` pipeline. | Plane A is now gone, not just frozen. Correlation/union-find demotes to an on-demand Aggregator utility. |
| **4** | **Extract the Tender** (generic federation); delete peer-merge. | Boundary now informed by real store-federation needs. Not rushed — porthole is the likeliest first external consumer, and the family can wait a little. |
| **5** | **Extract the minimal control-plane core** (generic resource-client + controller runtime) as a standalone crate/repo, leaving Flotilla-specific kinds behind. | Boundary-proven by phases 1–3. Yields a small, coherent piece "that could have been in the training data." |
| **6** | **Provisioning extraction** (if still warranted) + **TUI↔uishell sharing**: extract the surface-agnostic domain/view-model layer; TUI/web/uishell become thin **Surfaces** over it. | Depends on a stable resource/aggregator API beneath. |

## Cross-cutting

- **Tests ride along with the refactors.** Verbose, ad-hoc tests for old
  shapes get deleted wholesale as those shapes move; new shapes get **contract
  tests** per the CLAUDE.md testing philosophy. The only standalone test work
  worth pulling forward is fixing a *harness gap* that is forcing verbosity.
- **Headless core, thin surfaces.** Flotilla's end state is a headless daemon
  exposing the resource store + Aggregator + a shared View Model + the command
  model over HTTP-over-UDS. The TUI, a future web UI, and uishell are all
  Surfaces. (This extends the existing "clients own presentation" decision.)

## The unbuilt layer above all this

Meta-agents (Quartermaster, Bosun, Purser, Governor, Yeoman) sit above observer +
control plane and are explicitly *later*. The Aggregator's "possibly agentic"
piecing-together is the first place they touch this roadmap.
