# Convoy completion: settlement claims, integration conditions, attention observations

**Status:** Accepted
**Date:** 2026-07-22
**Relates to:** ADR 0008 (convoy orchestration), ADR 0009 (persistent
agents — the steward authorities named here), ADR 0010 (crew provisioning —
the brief that instructs self-settlement), issue #817 (the grill that fixed
this contract), issues #810/#811/#816 (restated against it), #805
(teardown residue), #785/#801 (the delete verb this contract gates).

The first completed convoy (issue #796 → PR #815, 2026-07-21) reached
`Completed` six minutes before its PR existed: the sole crew member
truthfully reported "I did my prompt" and the default rollup faithfully
promoted that testimony to convoy completion, while delivery (push, PR,
shepherd) was still in flight in the working directory. Auto-teardown on
`Completed` would have destroyed it. The #817 grill fixed the model that
prevents this class of loss without breaking the honesty of the reports.

## The three planes

A convoy's lifecycle state is spread across three planes with different
epistemics. Nothing may collapse them.

**1. Settlement claims** — the phase machinery (`CrewWorkPhase`,
`WorkPhase`, `ConvoyPhase`). Phases are **testimony stamped with an
authority**: a crew member self-reporting per its brief
(`WorkCompletionAuthority::CrewRollup`), a human override, a workflow gate,
a steward (Bosun for complex convoys, Governor for simple ones — ADR 0009),
or downstream vessels in multi-stage workflows. Claims can be honest and
premature at once; the 23:03 `Completed` was both. Claims are never
evidence that anything is safe to destroy.

**2. Integration conditions** — observations of the working directories
and the forge, on `Checkout.status`. Three-valued (`True` / `False` /
`Unknown`): absence of evidence is never evidence of absence — a failed
probe is `Unknown` (retryable, named), never `False`. The conditions:

- **`Clean`** — no uncommitted changes. Probed by git **through the
  checkout's environment runner**: host git for host checkouts,
  `docker exec` for in-container clones. Cheap; refreshed on cadence and
  always re-verified at teardown.
- **`Pushed`** — no local commits absent from the remote. Same probe
  route. In ephemeral environments (fresh clone in a container) this is
  the *survival-critical* condition: push is the only durable exit.
- **`Landed`** — the work is integrated: forge PR lookup by head branch
  across **all** PR states (in a squash-merge repo the branch is never an
  ancestor of the default branch, so PR state / patch-equivalence is the
  check, not `merge-base`). **`Landed` latches**: once observed true, the
  evidence (PR number, merge time) is recorded on the checkout and the
  condition never regresses — a merged PR falling out of open-PR queries
  must never resurrect "you have no PR" (the Plane-A confusion this
  clause exists to kill).

Environments the daemon cannot probe (vendor cloud sandboxes) are honestly
`Unknown` on the environment-side conditions forever; forge-side `Landed`
remains computable for every environment. The environment runner is
optional; the forge is not.

**3. Attention observations** — what the live process is doing right now:
`Working` / `NeedsInput` / `Idle` / `Unobservable`, with `as_of`, on
`TerminalSession.status`. Fed by harness hook events (edge-triggered,
per-harness parsers; codex's trust-gated hooks and Claude Code's lifecycle
hooks both need flotilla-side parsers) with **cleat noticing**
(screen-stability + prompt-pattern observation, level-triggered) as
fallback, cross-check, and sole source for hookless harnesses. Fresh hook
events win; staleness decays to `Unobservable` rather than lying.
Attention **never transitions a phase** — idle is not done; the
`Working`-claim + `Idle`-attention cell is surfaced for judgment
(needs-attention = `NeedsInput ∨ (Idle ∧ work unsettled)`), not
auto-resolved. That inference is what this ADR forbids.

## Phase vocabulary

- **`Abandoned`** joins `WorkPhase` and `ConvoyPhase`: a chosen terminal —
  the work was judged the wrong idea (or judged out in a try-N-ways
  selection) — distinct from `Failed` (broken). Requires an authority and
  a recorded reason; never inferred, never automatic.
- **`Completed` keeps its claim nature**: every work reached a settled
  non-failure terminal per its authority. A try-N-ways convoy with one
  landed attempt and three abandoned ones is `Completed`.
- **No `Integrated` phase, ever.** Integration is a condition; promoting
  it to a phase rebuilds the collapse this ADR cuts apart.
- **`Failed` gains `failure_source: Bootstrap | Crew | Steward`.** Today's
  two writers are conflated: vessel bootstrap failure (machinery never got
  the agent running — retryable once the infrastructure is fixed) and crew
  testimony of defeat (`flotilla crew fail` — do not blindly retry; the
  plan is wrong). Consumers must not parse message strings to tell them
  apart. `Steward` has no writer yet — it is reserved for stewards
  (ADR 0009) independently failing work once they exist.
- **Completion policy per workflow is a named, deferred extension point.**
  The current rollup (any work fails → convoy fails; all works settle →
  convoy completes) is correct for sequential workflows and wrong for
  try-N-ways, quorums, and optional stages. The policy knob lands with the
  first workflow that needs it.

## The teardown contract

Teardown-eligibility is `(Clean ∧ Pushed ∧ Landed)` **∨**
`Abandoned(authority, reason)`.

The abandonment disjunct is the **one deliberate exception** to "claims are
never evidence of safety": abandonment is a claim whose very meaning is
"we accept the loss", so it may substitute for verified integration — and
nothing else may. Mechanics:

- Verification runs at execution moment, per checkout, through each
  checkout's environment runner. Standing conditions inform displays and
  stewards; the delete re-verifies (TOCTOU guard).
- Refusals **name the dirt per checkout** ("2 uncommitted files, 3
  unpushed commits; PR #815 open, not merged"). The TUI confirm shows the
  same summary before asking.
- `--force` skips the gate and does nothing else — no silent best-effort
  push. Predictability over cleverness.
- **Abandonment is the archive path**: best-effort push of local commits
  (a judged-out branch is cheap insurance), stamp the phase, then tear
  down under the abandonment gate. Uncommitted work is what abandonment
  explicitly accepts losing.
- **Adopted checkouts are released, never deleted**, with dirt surfaced as
  a warning, not a refusal.
- `Unknown` conditions: on a reachable environment, refuse (a probe
  failure is a bug to surface); on a destroyed ephemeral environment,
  proceed as corpse-cleanup with the unknown recorded on the convoy's
  terminal record.
- Branch grooming is never part of teardown (#805 keeps only the
  zero-commit bootstrap-branch deletion).

## Proving the model

v1 lands on the single-agent workflow, which exercises every
safety-critical element: rollup claims, instructed delivery (#816), all
three conditions, both teardown gates, abandonment, failure classing, and
the attention plane. The first multi-vessel proof is the smallest useful
one: a two-vessel implement→review workflow (the in-convoy review style),
exercising `HandedBack`, mixed per-work settlement, and multi-work rollup.
Try-N-ways trails until the completion-policy extension point is real —
do not force it through the single-agent shape, and do not build a
synthetic epic to prove what a real workflow will prove for free.
