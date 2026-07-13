# Agentic-first orchestration; workflows are harvested as programs, not authored as YAML

**Status:** Accepted
**Date:** 2026-07-07

We deliberately do **not** grow the declarative WorkflowTemplate language ahead
of practice. The primary intended author of convoys for real projects is an
**orchestrating agent** that works out the right workflow with intelligence —
issuing **VesselRequirement** requests as tool calls (ADR 0007) and routing
information between crews via prompts and ordinary git (result branches,
fetch-and-collate) — because the hard parts of a workflow (what to prompt each
crew, what data flows back) are trivial to express in prompts and a research
project to formalise declaratively. Formal semantics are **harvested from
dynamic practice**, not designed ahead of it.

## Structure

- **Three authors, one mechanism.** Declarative templates (kept as minimal as
  today's: DAG + briefs), orchestrating agents, and humans (CLI/picker) all
  author the same runtime primitives: convoy, Leg (script/prompt-routing
  metadata), VesselRequirement.
- **Extraction target: programs.** When a convoy shape recurs, it is frozen not
  as YAML but as a **script/program** — exactly what agents are good at
  producing. This pulls a **Python and/or TypeScript client SDK** forward
  (sooner rather than later), tracking the CLI surface closely. Simple
  workflow programs stay simply analysable; some will be very dynamic.
- **Inter-crew data flow stays deliberately unformalised** (prompts + git)
  until patterns recur. *(Note 2026-07-11, #680 grill: when it does formalise,
  the shape is per-workflow explicitness about what a crew passes on and
  receives — filesystem state via vcs push/pull or mutagen-style sync,
  artifacts in object storage — never a blanket implicit sync rule; workflows
  may legitimately call for experimentally isolated branches.)*

## Open: durable execution

Long-lived workflow *programs* must survive restarts and resume "replayed up
to the current decision" — the temporal.io problem. Recorded as open, with
three candidate shapes rather than a decision:

1. **Level-triggered idempotent programs** that re-derive position from
   observed convoy/resource state (the k8s-controller answer; no determinism
   constraints).
2. **Event-sourced replay** against the resource store's own log —
   `resourceVersion`/watch is already the replication log (ADR 0002), so the
   journal substrate exists.
3. **Agent-transcript-as-state** for agent-orchestrated convoys: resumption is
   re-prompting with current state, not deterministic replay.

Avoid importing temporal's determinism constraints unless (1) and (3) prove
insufficient.

## Consequences

- Issue #624 (agentic/dynamic convoy launch) is promoted from parked Brainstorm
  to the intended orchestration path.
- A client SDK (Python/TS) becomes a tracked near-term direction.
- The WorkflowTemplate language accepts new features only when harvested
  practice has already made their semantics obvious.
