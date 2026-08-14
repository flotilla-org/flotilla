# The first standing agent is a governor, and it is an entry point before it is a loop

**Status:** Accepted
**Date:** 2026-08-13
**Relates to:** ADR 0009 (PersistentAgent roles — the Governor this ADR
materializes first), ADR 0017 (settlement planes the charter enforces),
ADR 0027 (the workflow substrate whose `ensure` entries declare standing
convoys), ADR 0028 (the delivery ladder this ADR's restart ruling composes
with), #1412 (usage-observer thread where governor-first was ruled), #1440
(the chartering grill — all six rulings), #1442 (busy-crew message delivery,
the open delivery question this shape promotes), #1428 (the ensure mechanism).

## Context

The ensure mechanism (ADR 0027, #1428) made standing convoys declarable, and
the usage observer (#1412) was nearly its first user: a tool-crewed poller
patching one resource's status on a five-minute cadence. Two objections
stopped it. First, its semantics turned out to be substantial design work
about *usage* — gauge-versus-telemetry, coalescing, multi-writer ordering,
subject identity — none of which taught us anything about standing agents.
Second, and sharper: a substrate's first user shapes it. A no-judgment poller
would have tuned the ensure mechanism toward "cadenced script patching a
status" while leaving untested everything standing agents exist for — agent
crews, memory across restarts, tracker-writing authority, credential
delivery.

Meanwhile the Governor role (ADR 0009, `docs/charters/governor.md`) had
accumulated the deepest manual practice of any persistent-agent role: an
operator-plus-agent session had been performing it across weeks of dogfood —
dispatching, settling, chivvying, recording rulings — with the charter
already encoding its authority model, settlement discipline, and escalation
list.

## Decision

**The first standing agent is a Governor for a non-platform island, run as a
standing convoy, and it is an entry point the operator talks to — not a
scheduled loop.**

### Governor-first, on someone else's island

The first governed island is **andamento**, not flotilla. A flotilla-scoped
governor is meta-circular during substrate churn: fleet bumps restart its own
daemon beneath it, no-compat wire changes invalidate its CLI mid-turn, and
the ensure mechanism it stands on is itself under construction. Flotilla's
own governor arrives once the mechanism is boring. Andamento is the
representative case: its work does not touch the platform, it has a live
ticket queue and its own CI gates, and — since most flotillas will steward
work unrelated to flotilla itself — the unrelated island is the general
case, not the exception. (A cold-start island with no convoy history is the
natural *second* entry.)

The usage observer is demoted to a background entry added later; its merged
artifacts (the Usage record shape, the generic status-patch verb, the
workload-script pattern) stand regardless.

### Entry point before loop

The first-cut governor has no scheduler and no wake cadence. On start it runs
**one read-only orientation sweep** (fleet state, open PRs, ready queue) and
posts a short on-station report; then it idles, and every mutating action is
an operator-initiated turn. Scheduled behaviour arrives later as *additional
input sources into the same standing session* — first watches over in-flight
work, then event-driven wakes — because operator conversation is expected to
remain a major share of interactions even in the proactive era. The ensure
loop's whole job in this cut is presence.

### The record is the memory

A restarted governor knows the charter and what the durable record tells it:
tracker issues, ruling comments, ADRs, fleet surfaces. No private state
survives restart — no memory directory, no transcript continuation — in the
first cut. This composes with ADR 0028's delivery ladder rather than
replacing it: upper rungs (warm session, adapter resume) restore
conversational comfort when available, but the **bottom rung — fresh agent,
reconstructed brief — must be sufficient**. Anything whose loss would hurt at
the bottom rung belonged on the record, so a painful restart indicts the
recording discipline, not the mechanism. This is also the honest test of
that discipline, which the charter already mandates (rulings land when made;
bodies are contracts).

### Layered charter

The platform charter (`docs/charters/governor.md`) is generic — one island,
seven verbs, settlement planes — and is *referenced, not copied* by the
standing brief. Project-specific guidance (CI gates, priorities, house
conventions) lives in the island's ops member, which the standing brief
points into: "look in your ops repo for X."

### Crewing and placement

The workflow names a **capability** (`governor`), not a harness; seeding
binds it to an adapter and model per host. Model choice follows judgment
density: the governor is the most judgment-concentrated, lowest-volume role
in the fleet, so it gets the strongest planner available (fable at time of
writing), with per-island and budget-driven adaptivity as trajectory, not
cut-one machinery. Stance is contained; placement is an always-on host —
never a desk machine whose lid couples governor liveness. Placement is free
because dispatch works from any credentialed host (ADR 0031's
rank-retirement).

Engine and model selection is **declarative** — the capability table (and,
as trajectory, issue labels driving a table lookup) decides; an inline
governor override at dispatch time is an exception that must name its
reason. Inline model-picking mid-context is suspect by default: it gets
poisoned by whatever else the governor is holding (ruled 2026-08-14 on
#1440; see #1501 for the label-driven direction).

## Consequences

- The ensure mechanism's first real exerciser tests presence, restart
  sufficiency, agent crewing, and credential delivery — not cadence tuning.
- Message delivery to a busy standing crew becomes a primary interface
  question rather than a resume edge case; #1442 (refuse vs queue vs steer)
  is promoted accordingly.
- The on-station report makes restarts self-evidencing: a restart-looping
  governor is visible as a timestamped series, without inspecting backoff
  internals.
- **Terminology:** the ensure-declared service is a **standing convoy**.
  "Independent" is *not* used for it — that term is reserved by the glossary
  for terminal sessions with no convoy association.
- The flotilla-island governor, transcript-carrying restarts, memory
  directories, and scheduled wakes are all deliberate deferrals, each with a
  named trigger (mechanism boring; bottom rung shown insufficient; a restart
  that loses something the tracker couldn't have held; the proactive phase).
