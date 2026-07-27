# Convoy lifecycle: Landing, Landed, Anchored

**Status:** Accepted
**Date:** 2026-07-27
**Relates to:** ADR 0017 (amends its phase vocabulary and refines one of its
rules), ADR 0020 (hulls — the same warm-workspace economics), ADR 0009
(stewards), issue #1113 (the grill that fixed this contract), #1026 (idle
crews never reach Done — resolved by this model), #1071 (event-driven
shepherding — given a home state here), #1111/#1114 (the disk-exhaustion
consequences of the old shape).

Six convoys ran concurrently on one host (2026-07-26); their merged work
left six warm vessels that nothing ever reclaimed, the boot volume filled,
and the daemon wedged. The proximate framing — "nothing reaps settled
convoys" — was wrong. The defect is that `ConvoyPhase::Completed` sat
among the terminal states while being operationally non-terminal: the
vessel is deliberately kept warm, further crew turns are expected for
review feedback, and the work has not landed. A state that is terminal in
the enum and non-terminal in operation gives teardown nowhere to hang, so
reclamation became a human sweep, and the sweep was forgotten.

## The phase re-cut

`ConvoyPhase` becomes:

```
Pending | Active | Anchored | Landing | Landed | Failed | Cancelled | Abandoned
```

- **`Completed` is deleted, not retained.** We are in the no-BC phase;
  keeping an identifier whose meaning was "terminal but not really"
  invites every consumer to keep special-casing it.
- **`Landing`** — the crew's turn is done, the work is in review. Not
  terminal. The vessel stays warm (the property worth keeping: a warm
  vessel answered review feedback in seconds where a cold rebase took 18
  minutes). Change-request events wake it — this is the home state for
  event-driven shepherding (#1071): "a merge landed elsewhere and this now
  conflicts" is a message to a convoy in `Landing`. Surfaces as a Demand.
- **`Landed`** — the true terminal success state: the work is integrated
  or the change request is closed. Teardown hangs here.
- **`Anchored`** — mid-work, waiting on an external event (a dependency
  merge, a human answer, a provisioning wait). `Active ↔ Anchored` is
  re-entrant. The state is in the model from the start so it is not
  retrofitted; its entry/exit wiring lands with the event-source design it
  shares with `Landing` wakes.
- `Failed | Cancelled | Abandoned` are unchanged (including
  `failure_source` from ADR 0017).

## Writers: one claim edge, one condition edge

The two transitions around `Landing` have deliberately different writers,
preserving ADR 0017's claims/conditions split:

- **`Active → Landing` is claim-written.** The sole writer is work
  completion: the crew's own unforced `flotilla convoy work complete`
  (instructed by the brief, later automated by adapter turn-end hooks),
  or a human's `--force` override. The edge is unconditional and
  idempotent — it records "the crew's turn ended" and **never inspects
  change-request state at call time**. An earlier design branched here
  (CR outstanding → Landing, none → Landed); it was rejected as
  call-time-state-dependent: a verb firing before the CR is observed
  would skip Landing and tear down under an open PR.
- **`Landing → Landed` is condition-written.** The sole writer is the
  lifecycle reconciler, continuously evaluating the standing integration
  condition: *no change request remains outstanding* — which uniformly
  covers merged and closed change requests. A checkout for which no change
  request ever existed satisfies `Landed` only when its branch has no commits
  beyond its base ref; a divergent no-CR branch remains unsettled. Testimony
  can never write `Landed`.

### Refinement of ADR 0017's "No `Integrated` phase, ever"

`Landed` is integration as a phase, which 0017 forbade. The rule is
refined, not repealed: what 0017 actually guards against is the collapse
of claims into integration — a phase that testimony can reach. That
remains forbidden. A phase may **cache a verified condition** when its
only writer is the reconciler that verifies the condition. `Landed` is
such a cache; the condition stays the source of truth and the phase is
its materialisation in the control path (the same stance ADR 0018 takes
on materialisation generally).

## Teardown: extract → reclaim, keyed on terminality

Teardown is keyed on "phase is terminal" (`Landed | Failed | Cancelled |
Abandoned`) — one path, no per-phase special cases. It is a two-stage
pipeline:

1. **Extract** — a no-op today; the reserved slot for shipping session
   logs, cleat recordings, and transcripts to object storage before the
   workspace disappears.
2. **Reclaim** — vessels and their checkouts (including the cargo target
   directories that are the actual disk cost), terminal sessions,
   workspace tabs.

The ADR 0017 teardown-eligibility verification —
`(Clean ∧ Pushed ∧ Landed) ∨ Abandoned(authority, reason)`, verified at
execution moment per checkout — runs unchanged as the gate (TOCTOU
guard). Automatic teardown gets no exemption a human `convoy delete`
would not get.

**The convoy resource itself is retained.** It is the durable record —
provenance to issue and change request, dispatch history, host. Deletion
stays a separate, explicitly-human act (the k8s Job relationship:
`Complete` versus deleted). Presentation learns to hide terminal convoys
by default rather than the record being destroyed to keep listings clean.

## Recorded for the follow-on, not scoped here

- **Per-vessel warmth by DAG reachability.** "Keep the warm vessel" is
  per-convoy only while convoys have essentially one working vessel. The
  durable rule: a vessel no remaining path through the workflow DAG can
  re-enter is reclaimable even while its convoy is still `Landing`; a
  vessel review feedback could re-task stays warm. Vessel teardown must
  therefore be independently reachable — the model here must allow a
  `Landing` convoy to hold one warm vessel having released three.
- **Admission free-space floor** — refusing dispatch to a near-full host
  is admission policy, not lifecycle; split to #1135 under the
  deterministic-floor family of #1115 (Quartermaster).

## Consequences

- #1026 (idle crews with merged PRs never reach Done) is resolved by the
  reconciler condition, not by a reaper heuristic.
- The manual post-merge `convoy delete` sweeps end; `convoy delete`
  returns to being an explicit judgment, not hygiene.
- Crew briefs gain the completion instruction as part of the brief
  contract; adapter turn-end hooks are the hardening layer for crews that
  neither report nor survive.
