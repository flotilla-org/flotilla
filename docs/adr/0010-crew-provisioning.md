# Crew provisioning: AgentAdapter, the Hull/Crew boundary, vessel-level stance

**Status:** Accepted
**Date:** 2026-07-08

How convoys launch agent crews (claude / codex / pi) that come up *properly* —
right sandboxing, right config, right prompt — without hand-holding.
(Responds to the portfolio crew-provisioning steer of 2026-07-08.)

## One definition per harness: the AgentAdapter

All knowledge of a harness lives in **one named AgentAdapter** — config file
locations and templates, skill mirror paths, credential file paths/formats,
launch-command synthesis, resume/re-prompt mechanics — consumed in **two
phases**: hull-prep reads the at-rest half; crew-launch reads the runtime
half. (Same pattern as cleat's `--aboard` providers and flotilla's discovery
factories: named definition, narrow obligations.)

Adapter obligations (the verb freeze, one level up from cleat's):
`prepare(hull, spec)` · `launch(spec) → command+env` ·
`deliver_brief(session, brief)` · `re_prompt(session, msg)` ·
`stance(spec) → flags`. No login-shell assumptions; secrets never in argv or
transcripts.

Crew instantiation lives with the hull machinery ("Clyde", i.e. today's
reconciler stack) **for now**; whether crew-launch verbs move with Clyde at
extraction or stay session-side is **explicitly deferred** — the shared
adapter definition keeps both deployments open.

## The Hull/Crew boundary (quotable)

> **The Hull carries everything a crew would find aboard no matter who crews
> it — checkouts, tools, skills, agent config defaults. The Crew launch
> carries everything that says who is aboard, what they may do, and why they
> are here — identity and credentials, permission stance, model choice, and
> the brief.**

- **Hull (allocation/refit):** checkouts; toolchain; **skills** mirrored to
  the standard paths (`~/.agents/skills` + per-harness mirrors) so
  agent-learned behaviour transfers unchanged (DE-v0 cross-scale invariance,
  one level up); agent config *templates/defaults* (settings, hooks).
- **Crew (launch, per member):** **credentials as 0600 files** (per-crew
  identity — the per-agent Forgejo-user pattern generalised; never hull
  state, so hulls recycle across crews of different authority without
  scrubbing); model selection; the brief; session-native args.
- **Memory:** neither — by reference (ADR 0009).
- **Refit** is now mechanical: re-crewing a warm hull swaps the crew layer;
  the hull is untouched.

## CrewSpec: the normalised startup contract

Extends `ProcessSource::Agent`; what a Leg's script writes, harness-agnostic:

```yaml
role: coder
agent: { capability: code }   # selector; pinning allowed
model: strong | fast | <pin>  # tier by default
brief: <ref>                  # per crew member
skills: [cleat-sessions, ...]
credentials: [forgejo-bot, ...]
```

`agent`/`model` are **requirements** (ADR 0007 pattern): v1 resolves tiers by
lookup; later, k8s-style label selectors over an *observed* capability
catalog ("good-at: design"); eventually possibly agentic resolution
(Quartermaster's first concrete job). The contract field does not change as
the resolver gets smarter.

## Stance: vessel-level, walls-first

- **`stance` is a property of the VesselRequirement, shared by all crew
  aboard** — not per-crew. A differently-stanced crew member means a
  different vessel (fan-out is cheap). Per-crew stance differentiation is
  deferred, honestly, rather than promised on mechanisms we distrust.
- v1 vocabulary (closed): `trusted` · `workspace-write` · `contained`
  (`read-only` is a known candidate, not yet added).
- **Realization is walls-first**: the vessel/environment provides the
  sandbox — docker walls, or on host-direct a wrapper sandbox *we* control
  (à la agent-seatbelt-sandbox) — because in-harness permission/escalation
  implementations are idiosyncratic. Harness flags are fallback only.
- **Floor-of-confinement semantics**: stance declares minimum confinement.
  Under-realization (the pair environment×harness cannot achieve the stance)
  is a **loud resolution failure**, never a silent downgrade — a convoy
  fanning over docker and host-direct keeps one effective permission stance
  or fails visibly. Over-confinement is permitted and **recorded**: effective
  stance lands on vessel/crew status for `ls`/panels.
- **Non-lock-in note:** the walls confine the *task execution environment*
  (actuator). Where the *agent loop pump* (turn generator) runs is
  deliberately unconstrained — today's harnesses conflate the two, but
  custom harnesses or tool-proxying may split them (pump in uishell or
  central, execution in distributed sandboxes).

## Prompting

- **The brief is a file, per crew member**, in the vessel workspace
  (`.flotilla/briefs/<leg>-<role>-<n>.md`), with shared goal context at the
  vessel level. The file is the durable, harness-agnostic truth (a restarted
  agent or an attaching human can always read why this crew is here); the
  adapter's `deliver_brief` also hands it over agent-natively as ergonomics.
  Accumulated briefs on a revisited vessel are the **script as executed, per
  character**.
- **Re-prompting is a session verb, not a subsystem**: `re_prompt` rides the
  proven driving pattern (`cleat send --submit`), with agent-native resume as
  a per-adapter upgrade. The **Bosun is the caller**; flotilla provides the
  verb. Useful to humans and template-driven convoys before any Bosun exists.

## PersistentAgent interplay

**Nothing here is blocked on flotilla#650.** The Bosun is a *consumer* of
this design (authors VesselRequirements + CrewSpecs, calls `re_prompt`), not
a component of it. One flagged touch-point: if "who may mark the task done"
is eventually answered "only the Bosun", completion authority becomes a
PersistentAgent concern — see the parked inter-crew workflow questions.

## Parked (tracked, not designed)

Inter-crew workflow: what triggers the in-vessel bounce (local autonomy vs
Bosun-always); whether one crew member holds done-authority; **artifact
sync** — code flows via commit/push, but non-code artifacts (images, videos)
need a decided fabric: marked artifacts with happens-after transfer, or
always-write-to-shared-space (object storage). In-vessel and cross-vessel
routing are the *same semantics with different transfer costs* — not two
mechanisms.
