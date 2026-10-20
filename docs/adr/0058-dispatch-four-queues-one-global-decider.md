# Dispatch is four queues with one deterministic global decider: work, ideation, convoy admission, placement; missions, deconfliction, whole-convoy admission against per-host ledgers, receipts and reasons

Accepted by the owner grill on 2026-10-06 ([#1394 ruling](https://github.com/flotilla-org/flotilla/issues/1394#issuecomment-6017608090)). The earlier Quartermaster dispatch-agent direction is retired. Deterministic daemon substrate owns global queue decisions; governors, operators and decision models change judgment inputs, and a rare human emergency override remains. A navigator organises fleet ideation with operator selection.

This record was first filed as ADR 0055, the same number as the Message ADR. It was renumbered to 0058 on 2026-10-08 (#2913); issue comments written before then call it ADR 0055.

The four queues have separate owners and meanings:

1. **Work, per Project:** contract-ready issues, batched by governors into resolved convoy requests; several issues may belong to one convoy.
2. **Ideation, fleet-wide:** grills, maps and brainstorms, organised by the navigator and selected by the operator.
3. **Convoy admission, fleet-wide:** briefed convoys with resolved workflows (possibly an initial layer or unexpanded outline). The fleet home daemon owns global order; this fleet's home is udder, also responsible for fleet-ops.
4. **Placement, per host:** resource users, including vessels and image builds, admitted against that host's reservation ledger. Vessels inherit convoy priority; builds inherit the highest waiting convoy's priority.

One daemon predicate per Project defines dispatchability: open, no open native blocked-by dependencies, the judgment label `ready`, no grill/map/brainstorm type or label, no live serving convoy or open PR, and no active soft hold. Body-section parsing and client-side "unblocked means ready" computations are retired. Source failures are unavailable evidence, never proof of readiness. The propose-and-observe machinery creates no convoys; dispatch observations retain real workflow and placement choices and time from observed readiness.

Soft holds are separate, authored land-after relationships with a reason. Land-dependent holds clear on positive landing evidence, not merely issue closure. A prior merged closing PR remains landing evidence if the issue reopens; a fresh relationship is needed to await another generation. Deploy-dependent holds clear on successful deployment evidence for the specified installation; merge alone is insufficient. Governors and operators may author these inputs, and deconfliction may do so later. Deployment receipts provide the evidence seam pending #2787's installation observer.

Mission membership is computed from map sub-issues, then the first matching charter lane, then the routine lane. Mission Value, Class of service (expedite, standard, background), and Crew limit are normalised from issue fields, labels or charter declarations. Global ranking uses per-project fair share; order is class, mission value, unblocking value, age, then conflict penalty, with a visible breakdown. Boosts remain undecided.

Deconfliction uses declared or predicted footprints, replaced by the crew's real diff. File rarity weights overlap; interfaces count more. Below threshold overlap penalises order; above it creates a self-clearing land-after hold. Prediction, actual overlap and conflicts are recorded for tuning and merge-order hints. Named sub-file areas and hot-file splitting are later directions.

Admission is whole-convoy: all first-stage resource users must hold reservations and satisfy mission crew limits and project share together. Waiting convoys hold no capacity. Ledgers account for memory, scratch disk, CPU requests and limits, and slots for heavy I/O, GUI, builds and agent-account concurrency. Pressure blocks new admissions; estimates start static and learn by Project/task type. There is no preemption; future idle-crew dehydration is the sole planned exception.

Dispatch returns a queued ID or typed refusal. Queued items show one current reason and a position; expected starts appear only when honestly computable. Messages announce reason changes, admissions, stalls and landings. Governor guarantees are first come first served within class and mission, aging, expedite first, no half-admission and no crew killed for capacity.

Service health and installation-specific deploy state are substrate facts. Degraded dependencies hold new admissions; running crews receive one superseding notice, infrastructure failures are reframed, and recovery reruns checks with one recovery notice. Rolls remain human/infra decisions, with their unblocking effect visible. Project and fleet surfaces show running placement, next work and reasons; graphs remain focused neighbourhood/mission/critical-path views.

Implementation is sequenced in #2782–#2789. This ADR records the entire ruling; readiness and holds do not implement mission ranking, admission scheduling, service health or the navigator themselves.

## 2026-10-15 amendment: Priority and Weight (#3313)

This amendment supersedes the mission attributes and ordering above, and the
mission crew-limit requirement in admission. Priority is a strict Urgent / High /
Medium / Low tier (default Medium); Weight is a finite nonnegative share (default
1), not a capacity cap. Each resolves field, label, then charter. Per-ticket
Priority overrides the mission tier, superseding #3304's undecided boost.
Mission and Map issue types, plus existing `:map` labels, identify missions.

Within a tier, positive-weight ready work precedes zero-weight fallback work,
then project fair share precedes mission share. Prefer the smallest prospective
live crews per share, `(live + 1) / share`; mission crews are counted per tier.
This deterministic live-crew stride needs no persistent history and naturally
lends idle missions' shares to ready work. Deficit scheduling was rejected because
it adds a cohort-dependent mutable balance; probabilistic sampling would weaken
strict tiers. A ready snapshot ranks the next slot; live crew changes re-rank it.
Within a mission, descendants, readiness age and conflict penalty remain.
See [dispatch readiness](../dispatch-readiness.md) for normalization, explanation,
and the companion charter roll requirement. Capacity enforcement remains #2785.

GitHub Priority/Weight observations reuse `issue_field_values` from the existing
REST inventory and its durable item cache, rather than polling each ticket's
field endpoint. Per-ticket polling would multiply ready/live cardinality by the
60-second refresh cadence and exceed WORKLOAD.md's shared API budget. Same-second
field changes update cached REST items without refetching unchanged native graph
details. Live convoy reads select projects and nonterminal phases before decode,
so retained history does not increase accounting work.

## 2026-10-20 amendment: budgets, capacity pools and vessel leases

The owner's environment and placement grill of 2026-10-20 supersedes the
reservation language above, in line with
[ADR 0064](0064-primitives-model-resources-records-gated-machines-inboxes.md)
("Admission grants a cap, never a reservation" and "Admission grants a capacity
budget").

- **Admission grants a budget; it reserves nothing.** "All first-stage resource
  users must hold reservations" is replaced. Admission refuses only a convoy
  whose first stage could never fit, judged against declared capacity, not
  current free capacity. Waiting convoys still hold no capacity.
- **Placement holds capacity, as vessel leases.** Queue 4 is a placement queue
  per **capacity pool**, not per host: a host's compute and disk, or a provider
  account's quota. Each pool has one authoritative daemon, which keeps its
  ledger and grants leases. A vessel may draw on several pools; its
  environment's capability declaration names them. A lease lasts until the
  vessel is Gone. "Reservation" is retired as a term.
- **"No half-admission" is dropped** from the governor guarantees. A budget is
  not a promise that every first-stage vessel starts at once; vessels that do
  not fit wait in placement order.
- **Dynamic providers are placement candidates.** A cloud sandbox or agent
  cloud session provider counts when the convoy is permitted to use it: credits
  remain, or (later) its predicted cost fits the budget. What happens to work
  that is permitted in principle but short of budget, for example a queue
  waiting for a raised account allowance or approved spend, is open.
- The ledger dimensions listed above (memory, scratch disk, CPU, slots) stand,
  now per pool. What admission needs on the admitting host itself, versus what
  an environment consumes on each pool, is for the capacity grill under #3303.
- #2786 is renamed from per-host reservation ledgers to per-pool vessel leases.
