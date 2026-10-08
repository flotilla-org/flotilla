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
