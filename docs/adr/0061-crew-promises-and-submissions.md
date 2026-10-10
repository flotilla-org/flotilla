# 61. Crew promises and submissions

**Status:** Accepted (owner grill, 2026-10-10; #2981)
**Date:** 2026-10-10
**Amends:** ADR 0017 (a Settlement Claim no longer carries the deliverable reference), ADR 0028 (Landing also requires every promise to be kept or retracted)
**Relates to:** ADR 0027, ADR 0045 (stalls), ADR 0047 (stored data stays decodable for one generation), ADR 0059 (agent sessions), #2979 (landing observability), #2982 (flotilla-commands skill)

## Context

Two convoys briefed to deliver several PRs landed after their first PR merged, tearing down the worktree that held the next PR (#2979). The substrate did what it was built to do. The stock `merged-unclaimed` turn delivery told the crew to run `flotilla crew complete` with the PR URL. That claim moved the convoy to Landing. The exit table then evaluated only the subjects bound so far, and one merged PR satisfied it.

The stock workflow assumes one deliverable per convoy. A brief that asks for more is prose the crew must remember, and nothing in the substrate records it. Meanwhile the workflow already declares per-role obligations: each crew role's `completion_conditions` require a `decision-ledger` artifact, which is an implicit promise.

## Decision

### Promises

A **Promise** is something a crew member undertakes to produce, which someone else can check.

- **Owner.** Every promise belongs to one crew member, addressed by vessel and role. There are no unowned promises. Handoff moves a crew member's open promises with the work.
- **Sources.** Promises come from three places:
  - the **workflow template**, which declares an exhaustive general set; each convoy winnows it to the feature's scope, by the governor now and by decision-model calls later;
  - the **dispatch**, for example `--promise` when a plan already names three PRs; it names an owning role, or defaults to the role the workflow marks as the deliverer;
  - the **crew**, at runtime.
- **Kind.** Every promise has a kind from one flat namespace, such as `pr`, `ref`, `decision-ledger` or `demo-video`. Each kind carries the rule for when a promise of that kind is kept:
  - a PR when it merges;
  - a pushed ref when it is at the submitted commit on the remote;
  - a decision ledger when it exists;
  - a demo video when a human approves it.

  There are no free-form promises. New kinds are added as workflows need them; the generic "artifact" becomes specific kinds.

The workflow's per-role `completion_conditions` become template-sourced promises. Checks such as "the PR is ready" are not promises. They belong to the `pr` kind's rules.

### Submissions

A **Submission** is one attempt to keep a promise: the PR, ref, upload or ledger submitted, with traceable metadata (for example the commit or digest), and the verdict on it once given.

- A promise's state is `open → submitted → kept`, or `retracted`.
- A rejected submission returns the promise to `open`, keeps the submission and its verdict in the promise's history, and wakes the owner with the reason.
- Who may give a verdict comes from the kind's rule: merging for a PR, a human for a demo video, a reviewer role for a design document.
- A submission made without a matching promise creates a crew-sourced promise in the `submitted` state. Today's discovered `produces` subjects follow this path, so nothing a crew submits escapes landing.

### Retraction

- A crew member retracts its own crew-sourced promises with a reason. A retracted promise stays visible, with that reason.
- A crew member cannot retract a template- or dispatch-sourced promise. If it believes one cannot be kept, it raises a `retraction-proposed` stall carrying the reason (ADR 0045). The stall goes to whoever supplied the promise, who either accepts the retraction (the promise is retracted) or refuses it (the promise stays open and the crew member is woken with the answer).

### Completion and landing

- `crew complete` remains the Settlement Claim, a separate act with no deliverable reference. It is refused while any of that crew member's promises is open or submitted.
- The convoy enters Landing once every crew member has filed its claim.
- The convoy lands when its exit table fires and every promise is kept or retracted.
- The stock turn-delivery briefs point the crew at its open promises instead of telling it to complete.

### Storage and surfaces

- Promises and their submissions live in Convoy status, per crew member, beside `crew_work`. Promise, submit and retract are convoy status patches, as `crew complete` is.
- Cross-convoy views are projections maintained from the Convoy watch, never per-request scans over every convoy (#2953). An example is the queue of submissions waiting on a human.
- Wheelhouse shows promises, through `flotilla pm connect` today, in the planned agent view and a convoys overview. The CLI shows them in `convoy list` and `convoy explain`.
- Routing a human verdict to the right person when there is more than one is out of scope.
- A separate Promise resource kind would be more normalised. It remains open if the status shape proves awkward.

### Names and verbs

- The noun is **Promise**, each attempt is a **Submission**, and the states are `open`, `submitted`, `kept` and `retracted`.
- The CLI verbs are `flotilla crew promise`, `crew submit`, `crew retract` and `crew complete`.
- The names avoid existing terms:
  - Delivery and deliverable (Turn Delivery, the Delivery Ladder);
  - Fulfilment (Fulfilment Kind);
  - Completion (the admission-time stage);
  - "settled" for promises (it sits too close to Settlement Claim).
- Crews learn the flow from the flotilla-commands skill (#2982), not from per-brief text.

## Consequences

- A convoy can deliver several PRs, a ref, an approved video and a ledger. It lands when all are kept or retracted.
- Briefs stop carrying multi-deliverable prose; dispatch states the plan as promises.
- Crews that never promise anything behave as today, because their submissions create promises.

### Stored-data impact (ADR 0047)

- The new Convoy status fields are additive, with `default`.
- Workflow templates and frozen workflow snapshots move from `completion_conditions` to promise declarations. Decoders accept `completion_conditions` for one generation and map them to template-sourced promises.
- The out-of-repo sources that author workflow templates must be named in the implementing PR.
