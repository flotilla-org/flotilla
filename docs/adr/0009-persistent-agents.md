# PersistentAgent: one kind, vessel residency, island-scoped authority, pluggable memory

**Status:** Accepted
**Date:** 2026-07-07

Persistent agents (the meta-agent layer) arrive earlier than planned because
agentic-first orchestration (ADR 0008) needs an orchestrating agent to exist.
They are modelled as **one resource kind, `PersistentAgent`**, with the role as
a label — `governor` and `bosun` first; quartermaster/purser/yeoman later.

## Structure

- **Spec**: charter (duties/prompt refs), scope (its island's namespace), a
  **VesselRequirement** for its residency (ADR 0007), and a **memory ref**.
- **Residency**: a reconciler keeps the agent materialised in a
  **free-floating (convoyless) Vessel** running a harness — reusing the vessel
  mechanism for deployment/isolation rather than inventing another. A later
  "direct agent loop" (no unix environment, internal tools only) is an
  alternate *materialisation* of the same resource — a different resolver
  answer, not a model change. Always-on placement (e.g. an always-on host) is
  just a requirement, never a hard-coded host name.
- **Roles**:
  - **Governor** (island steward): reads its repos, **never edits source or
    raises its own PRs**; maintains issues and docs; watches upstreams (e.g.
    fork-tracking); launches convoys **for its own project only**.
    Frontier-class Governors may drive simple convoys directly and spin up a
    Bosun for complex ones.
  - **Bosun** (convoy-scoped): the ADR 0008 orchestrating agent — chivvies a
    convoy along (continuation, unsticking, rewind), issuing
    VesselRequirements and routing information between crews.
- **Authority / blast radius**: three layers, all real —
  1. **Charter** (guidance; worthless against confusion, still written),
  2. **Credential scoping** (read-only git; issues/docs-write forge token; no
     PR scope),
  3. **Control-plane scoping**: **namespace ≈ island/Project is the authz
     boundary**; the agent's daemon access is a per-agent minted scoped
     endpoint (the porthole capability-token pattern; the first instance of
     the long-mooted restricted-secrets/key-issuing direction).
  **v1 is deliberately fast-and-loose** (ambient credentials) while the
  concept is dogfooded; the holistic story — short-lived credential issuance,
  hand-off between agents, porthole capability tokens, proxying services whose
  tokens aren't fine-grained enough — is recorded as direction (Purser/Clyde
  territory), not designed now.
- **Memory**: a durable *directory that survives re-materialisation*,
  referenced from the spec and materialised into whatever the agent inhabits.
  Backing is **pluggable**: local filesystem (the solo-machine starter — no
  infra dependency), a git repo (what we dogfood, on the lab hub), object
  storage (the enterprise destination). Explicitly rejected: the island repo
  itself (pollutes project history, tangles the Governor's read-only stance,
  impractical for OSS forks; the bare-branch/git-notes variant is noted and
  parked as fiddly). Memory sync in/out of hulls lands on the same
  deliberately-unpinned Clyde boundary edge as token injection.
- **Triggers/schedules stay charter-side** initially: the agent wakes itself
  (harness cron/loop) and decides — "check upstream for changes" is a charter
  duty, not a flotilla Trigger subsystem. Formalise only from harvested
  practice (ADR 0008 discipline).

## Why

- A first-class kind (vs "a vessel someone started") buys: respawn-across-
  reboot ownership, representation while the vessel is down, charter upgrades
  as spec changes, dashboard identity, and the free later swap to non-vessel
  materialisations.
- Namespace ≈ island gives Governors a crisp blast radius using scoping the
  store already has.
- An auditable memory directory (especially git-backed) is the closest
  observable proxy for whether "stewardship" is actually being instilled.

## Consequences

- New kind + small reconciler (TaskWorkspace-shaped), unscheduled — sequenced
  after the observer reshape unless dogfooding pulls it forward.
- The convoy-create path gains "created-by: agent" attribution eventually.
- Purser acquires its first concrete mandate (credential issuance direction).
