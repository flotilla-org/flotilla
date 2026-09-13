# Flotilla Transition Roadmap

This is the plan for getting Flotilla out of its current two-plane straddle and
onto a single coherent track. It is the "where we're going and in what order"
companion to the glossary ([`/CONTEXT.md`](../CONTEXT.md)) and the decisions in
[`docs/adr/`](adr/).

## The situation

The old fleet-observer data plane has been removed. Providers and adapters remain
in `flotilla-core`, but they no longer feed correlation, WorkItems, or the old
snapshot pipeline. Discovered checkout facts are represented as observed
resources in an ephemeral store; the Aggregator combines those facts with the
durable resource store and emits query result sets for surfaces.

The remaining straddle is narrower: resource-store federation already runs over
`HttpBackend`, while the bespoke peer subsystem still supplies mesh transport and
legacy multi-host behavior. The destination is one resource-based control plane:
observed reality and desired state use the same kinds (ADR 0003), lifecycle
authority distinguishes their ownership, and convoys are the unit of launched
work.

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

## Near-term discipline

1. **The remaining peer layer is bugfix-only.** Put new multi-host capability in
   resource federation and the eventual Tender rather than extending peer-merge.
2. **The TUI is *factored*, not frozen.** Both the TUI and uishell stay relevant:
   ~70% of `flotilla-tui` is legitimate ratatui rendering; ~30% is a
   surface-agnostic domain/view-model layer (intent/action engine, declarative
   tables, the data→view-model pipeline) that should be extracted and shared.
   Stop *duplicating* that layer per surface; do keep the TUI itself maintained.

## Sequencing

The rule that resolves bottom-up-vs-top-down: **freeze the doomed plane
immediately, but extract generic pieces only after Flotilla has proven the
boundary by consuming them.** Keep extractions as workspace crates in this repo
until boundary-proven, *then* promote to separate repos (as cleat/porthole are).

| Phase | Status | Work | Why here |
|------|--------|------|----------|
| **0** | **Substantially complete** | Keep only the remaining peer layer bugfix-only. | The deleted observer pipeline no longer needs a freeze; peer-merge still does. |
| **1** | **Complete** | **Non-k8s backing store** (sqlite) + **lift the k8s projection into the resource model** (ADR 0001-B). | Enables dogfooding convoys without a live cluster and gives resources one typed API across backends. |
| **2** | **Complete** | **Convoy as the real end-to-end launch path** (create → provision → present → TUI), dogfooded. | Proves the resource/reconciler/provisioning boundaries in the real consumer. |
| **3** | **Substantially complete** | Provider refreshes publish observed Checkout resources; lifecycle authority, the Aggregator, and deletion of the old observer pipeline have landed. Finish the Aggregator's target shape and lifecycle-authority coverage. | Completes the move from a parallel observer data plane to resource-backed observation and queries. |
| **4** | **In progress** | **Finish and extract the Tender** (generic federation); delete peer-merge. Store replication over `HttpBackend` already exists in `crates/flotilla-daemon/src/server/replicator.rs`. | The extraction boundary can now be informed by working federation rather than designed in the abstract. |
| **5** | **Pending** | **Extract the minimal control-plane core** (generic resource-client + controller runtime) as a standalone crate/repo, leaving Flotilla-specific kinds behind. | Boundary-proven by phases 1–3. Yields a small, coherent piece "that could have been in the training data." |
| **6** | **In progress** | **Provisioning extraction** (if still warranted) + **TUI↔uishell sharing**: continue extracting the surface-agnostic domain/view-model layer; TUI/web/uishell become thin **Surfaces** over it. | Depends on a stable resource/Aggregator API beneath; the TUI table view model is already factored. |

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

Meta-agents (Quartermaster, Bosun, Purser, Governor, Yeoman) sit above the
control plane and are explicitly *later*. The Aggregator's "possibly agentic"
piecing-together is the first place they touch this roadmap.
