# The decision ledger, the Demand's shape, and attention admission

**Status:** Accepted
**Date:** 2026-08-21
**Relates to:** ADR 0017 (settlement claims / conditions / attention — this
ADR adds a claims-plane artifact and two admission rules), ADR 0018
(Demands and Regards — this ADR completes the Demand's shape), ADR 0027
(the episode escalation ladder the never-claimed ruling leans on), ADR 0028
(the durability fence the ledger rides), ADR 0010 (briefs — which gain the
ledger obligation). Sources: the 2026-08-21 research sweep
(`docs/research/2026-08-21-*.md`, PR #1612), especially the decision-ledger
essay note and the landscape survey's settlement-gate section.

The 2026-08-21 research sweep found four independent bodies of work
converging on Flotilla's settlement surface. Measured against ADR 0017's
three planes, most of what they offer is already ruled — the
claim-versus-observation split that Argo Workflows enforces structurally is
0017's planes 1 and 2 with stronger epistemics. What survives contact is
one genuinely new artifact, two field-level completions, and two admission
rules. This ADR records them.

## The decision ledger: claims-plane testimony about choices

Every crew brief gains an obligation: at settlement-claim time, report the
**decision ledger** — every decision made *where the brief was silent*,
ranked least-confident first. Each entry: where the brief was silent, what
was chosen, the alternative considered, and what the crew would have asked
if asking were free.

**Epistemics.** The ledger is plane-1 testimony (ADR 0017): disclosure by
the implementer, honest and possibly incomplete, stamped with its
authority, never proof. It is not judgment — the independence requirement
that applies to *auditing* work does not apply to *disclosing* one's own
choices. Checking the ledger's completeness (finding the undisclosed
decisions) is observation-plane work for a future independent auditor.
Two rules for that auditor are pinned now, before it exists: **it never
blocks settlement, and it cannot change code** — an auditor that can fix
things optimizes for a clean report instead of an honest one. Its natural
home is the in-crew review workflow (0017's implement→review shape) when
that arrives.

**Where it lives.** The ledger rides the settlement claim's durability
fence (ADR 0028): claim without ledger is flagged, not rejected — the same
flag-don't-wedge stance as the rest of the fence. Its surface is a
structured section in a PR comment — the shepherd and the governor already
read those, and the forge keeps them forever. The claim record carries a
pointer. No new resource kind unless the PR-comment convention proves
insufficient; no ledger files in the repo — archiving what the forge
already records is the memory-discipline mistake in another costume.

**Lifecycle: review first, then triage — no new queue.**

1. **Pre-merge, the ledger is review material.** Wrong decisions get
   revised in the same review cycle as everything else — crew re-briefed,
   code changed, ledger updated. This is the primary payoff: the ledger is
   the review surface for exactly the choices a diff read misses.
2. **At settlement, the governor triages the ledger inside the settle
   step it already performs.** Most entries die with the PR. Each entry
   gets a one-line disposition — *fine* / *revised* / *graduated → where*
   — which is the audit trail that triage happened. Graduation targets
   are the places knowledge already lives:
   - an **ADR amendment** — a real architectural choice made silently
     (this generalizes the existing ADR-carry review discipline; the
     ledger is its input stream);
   - a **CONTEXT.md gloss** — a term the crew had to invent;
   - a **follow-up issue** — a decision that is really deferred work;
   - a **brief-template fix** — the instrument-only finding: a
     *recurring* silent spot means the briefs systematically
     under-specify something, and nothing but a ledger stream would ever
     reveal it.

## The never-claimed gap: attention, not settlement

The landscape survey's structural prior art (Argo's force-settle timeout
for a claim that never arrives; session models with liveness-derived
staleness) poses the question: what happens when a crew evaporates before
claiming? The ruling: **nothing may settle it**. A timeout that
auto-settles would rebuild the inference ADR 0017 forbids — attention
never transitions a phase. The never-claimed convoy is already visible as
`Idle ∧ work unsettled`, which is the needs-attention formula; what the
timeout becomes in Flotilla's shape is a **deadline on the escalation**,
not a settlement transition: past the deadline the demand escalates
loudly (ADR 0027's episode-escalation-ladder stance), and the convoy stays where
truth left it. Force-settle exists in the field because those systems
have no attention plane; Flotilla has one — use it.

## The Demand completes its shape

Three fields, transcribed from well-worked prior art (the HumanLayer
post-mortem and the hosted-platform outcome APIs surveyed in the
landscape note), that ADR 0018's Demand lacks:

- **Typed verdict.** A resolved Demand records *how* — an enumerated
  disposition, not free text — so downstream automation can branch on it
  and history can be queried.
- **Enumerated response options.** A Demand may carry the responses that
  make sense (approve / revise / abandon / a workflow-specific set), so
  surfaces render actions instead of a text box, and the verdict is one
  of the offered options or an explicit other.
- **Expiry.** A Demand may carry a deadline with a declared
  on-expiry disposition (most commonly: escalate — never silently
  resolve). This is also the never-claimed ruling's mechanism: the
  escalation deadline is Demand expiry, used for attention, not
  settlement.

## Attention admission: two rules, no taxonomy

The activity-classification idea from the sweep collapses, on Flotilla's
structure, into two admission rules — Flotilla already knows what kind of
thing each process is (`CrewSource::Agent` crew versus bare workspace
pane commands), so no classification vocabulary is imported:

- **Working is the default and is never surfaced.** No spinners. Agent
  crew attention stays exactly 0017's formula. A build an agent runs
  inside its own session is the agent's business, not the attention
  system's.
- **Bare panes contribute exactly one event: exit.** *(Amended
  2026-08-21, same day: the original ruling added an expected-to-persist
  template hint with failure- versus completion-flavored exits. Struck —
  template panes are only ever standing things (dev servers, test
  runners, agents aside); nobody declares a one-shot command in a
  workspace template, so the hint was a flag with one value.)* A template
  pane exiting is the attention event; nothing surfaces while it runs.
  No template schema change. When a crew agent wants a *bounded* run to
  be observable, the path is adoption, not templates: run it in a cleat
  session and it becomes adopted crew, visible through the existing
  session machinery — observability arriving with the need.
  *(Amended 2026-08-24: observed Checkouts are no longer presentable tree
  entries. Checkout observation remains the path-matching source for pane
  exits; the resulting attention is joined onto every real Project whose
  repository catalog contains that checkout. A checkout with no catalogued
  Project remains observation-only and does not create a repository-keyed
  pseudo-project.)*
- **Occupancy suppression.** A session the principal is currently
  attached to raises no Demands — the principal is looking at it. This
  is strictly better than a static "interactive tool" class: the same
  editor is silenced while attended and legitimately surfaceable when
  abandoned for a day. Flotilla can do this because cleat knows
  attachment; systems that imported a static class lacked that state.

## Consequences

- Brief templates gain the ledger obligation and its format (ranked,
  least-confident first, four fields per entry).
- The shepherd's review processing reads the ledger section; the
  governor's settle step gains per-entry triage dispositions.
- `Demand` grows verdict, response-options, and expiry fields; demand
  admission gains the occupancy check; the escalation path gains
  deadline-driven raising.
- Workspace-template panes are unchanged (the persist hint was struck by
  the same-day amendment); a pane exit is the single attention event.
- The independent completeness-auditor remains future work with its two
  rules (non-blocking, cannot change code) binding from now.
