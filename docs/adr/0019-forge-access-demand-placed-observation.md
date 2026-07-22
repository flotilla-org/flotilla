# Forge access: demand-placed observation, quota, and degradation

**Status:** Accepted
**Date:** 2026-07-22
**Relates to:** ADR 0014 (demand-backed curated queries — the machinery this
ADR extends and leans on), ADR 0017 (as_of/staleness idioms), ADR 0018
(attention economics — API quota joins tokens and attention as a metered
resource), issue #885 (the investigation and grill), #838 (the burn-class
lineage), the observer reshape (forge state as observed resources).

Three same-day exhaustions of the shared 5,000/hr GitHub user limit caused a
dispatch outage (convoy admission's issue fetch fails under 403). The
investigation exonerated the daemons — both forge providers already ride an
ETag conditional-request cache (304s are free) plus `changed_since`
incremental fetch — and convicted the CLI-shaped consumers: shepherd scripts
polling checks every 30s per waiting PR (~240 calls/hr each, own cache
deliberately invalidated per poll) and governor `gh` bursts. The grill fixed
the architecture around three rulings.

## Ruling 1 — the read path: one cached door per host; demand is the placement

- **Per host, the daemon is the sole cached forge read surface**, as an
  extension of the ADR 0014 demand-backed query family — never a
  transparent proxy (invisible interception bites during debugging, and
  `gh` cannot cleanly redirect github.com traffic anyway).
- **Fleet tier: independent per-host, demand-backed, ETag'd polling. No
  coordination is built.** Two properties license this:
  - Forge state is **observed data**: an observation of an external system,
    latest-wins by `as_of`, no single-writer invariant. Coordination can
    never be a correctness requirement here — only an efficiency choice —
    so it must be judged purely on cost.
  - ETag'd 304s make N-host duplication approximately free; only
    changed-data fetches multiply, and they are small.
- **Demand-backing supplies organic surrogate placement.** The host with
  live watchers on a scope is the host that polls it; hosts with no
  watchers poll nothing. "Which daemon is master for this project" is not
  elected — it emerges from where work and attention sit, and moves when
  they move. Accepted cold edge: first-watch latency on a cold host (the
  same trade ADR 0014 accepted).
- **Escape hatch, named not built**: if org-scale fleets ever make
  changed-fetch multiplication expensive, the mechanism is a **lease-based
  surrogate** per (service, scope) — a Lease resource with renewal, so a
  sleeping host lapses harmlessly; latest-wins makes lease churn safe.
  Broadcast query is rejected (moves queries, not fetches); DHT
  partitioning is rejected (wrong at fleet-of-ten scale).
- **Writes stay direct** (comments, merges, creations): unavoidable real
  calls, low volume, and daemon routing would buy audit questions this ADR
  does not need to answer.

## Ruling 2 — checks-as-provider-data: shape shelved, dependency refused

The datum crews poll for (check runs / CI status) is absent from the model.
Its shape is **recorded but deliberately not built**:

- ChangeRequest data grows `check_rollup`, `mergeable`, and
  `review_activity` (counts and timestamps, not bodies — bodies are fetched
  once per detected review round). Two demand scopes: the repo listing
  stays shallow; a **per-change-request detail scope** carries checks and
  review activity, polls only while watched (exactly the shepherd-wait
  window), at a faster cadence, with ETag making misses free. A separate
  CheckRun kind is rejected — nobody watches a check outside its PR.
- **Why shelved**: the review patterns are not yet known. Collaborate-
  through-PR likely persists, but may not be the high-volume-convoy path
  once in-vessel/in-convoy review exists (the structural direction: the
  review-and-fix loop as local crew back-and-forth, the review process
  delivered as artifact cargo, the forge touched only at delivery — which
  is simultaneously the token-burn, steward-mediation, and quota answer).
  Build the detail scope when patterns justify it; the shepherd script
  gains **no flotillad dependency** meanwhile (it serves the whole
  portfolio, not just flotilla).
- **Near term, fix the script dumbly**: adaptive backoff (1.5× decay to a
  120s cap), metadata fetched every third poll, and **expectation-first
  polling** — the script learns each repo's expected CI duration as an EMA
  from its own completed waits (zero API cost), sleeps to ~90% of it, then
  polls toward the expected completion time before entering backoff. Aim
  at the completion time; do not sample uniformly. The same
  expectation-first principle applies to the daemon detail-scope cadence
  if ruling 2's shape is ever built.

## Ruling 3 — degradation and partitioning

- **Providers back off until reset**: on 403 rate-limit, suspend the
  affected scope's polling until the reset timestamp (+ jitter) GitHub
  supplies. No retry hammering, no error spam; `as_of` already expresses
  the staleness honestly.
- **Admission goes store-first**: resolve the issue snapshot from the
  resource store when within a hard freshness bound (the daemon's
  incremental poll usually has it); fetch only when stale or missing; on
  403, **fail fast naming the reset time**. Never queue a dispatch
  silently — an unwatched dispatch firing minutes later is worse than a
  clean retry (an explicit `--queue` flag may exist someday).
  **Proceed-on-stale is rejected** even with a warning: body-is-contract
  means a stale snapshot risks a crew working a superseded body. The
  freshness bound is the line.
- **Token partitioning**: a second token on the same account is a no-op —
  the pool is per user, summed across all their tokens. The designated
  route, **built only if exhaustion recurs after the dumb fixes**, is
  **flotillad as a GitHub App installation**: separate per-installation
  pool, daemon writes attributed as the bot they are, quota scaling with
  org size (the same mechanism that keeps claude-review off the user
  pool). A machine account for scripts is the fallback and partitions by
  tool rather than role.

## Consequences

- Implementation now: provider backoff-until-reset; admission store-first
  with honest 403. Script fixes already landed (rjw-cc `bf4ab9c`,
  `0579bbf`, `4b383b5`).
- The API quota joins model tokens (#838) and principal attention
  (ADR 0018) as a metered resource the stack economises — same shape:
  observe consumption, kill repetitive burn at the source, route the
  high-volume loop away from the expensive channel.
