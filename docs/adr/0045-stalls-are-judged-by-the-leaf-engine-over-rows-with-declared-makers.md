# 45. Stalls are judged by the leaf engine over rows with declared makers

Date: 2026-09-27

## Status

Accepted

Grilled 2026-09-27 on #1987 (rulings recorded there). Amends ADR 0028 and ADR 0029. Absorbs #2025 (reconciler failure semantics), and answers #1973 (idle detection) and #1983 (attention vocabulary).

## Context

Processes stall silently. One day, 2026-09-27, produced three examples:

- **A crew goes idle owing a claim.** Twice, a crew's `crew complete` was consumed by delivering a pending brief (#2117). The crew believed it had finished, handled the brief, and ended its turn. The convoy held for over an hour until an operator noticed.
- **Settlement waits on evidence nobody refreshes.** Leaf rows are armed only for convoys in `Landing` or `Anchored` (`leaf_engine::sync_rows`). A convoy holding in `Active` for a crew obligation has no rows, so the engine cannot see what it is waiting on.
- **A controller retries forever.** After the r302 roll, a quarantined credential spec failed work-credential reconciliation every 60 seconds. It was visible only in logs.

The existing proposals attack the problem from different sides:
- reinforcing the brief at turn boundaries (#1987);
- detecting idleness with agent and cleat hooks (#1973);
- separating "waiting for a prompt" from "needs attention" (#1983);
- a shared terminal-vs-retryable failure primitive for reconcilers (#2025).

Each addresses a symptom. None defines what a stall *is*, so each would need its own thresholds and its own notion of "idle too long". That is noisy for standing roles, and it cannot distinguish "waiting on CI" from "nothing will ever happen".

The leaf engine (ADR 0029) already has most of what is needed:
- a closed vocabulary of three-valued leaves;
- demand-bound refreshers;
- watchers (reconciler wakes, turn delivery, blocking `flotilla wait`).

What it lacks is any notion of *who can make a leaf true*.

## Decision

### 1. A stall is a broken invariant, not an idle heuristic

Every non-terminal holding state has at least one armed row whose maker is able to act. A state with no such row is **stalled**. Idle signals are evidence about makers, not the definition of a stall. Nudges are one reaction to a stall, not its detector.

### 2. Rows declare their maker; every holding phase arms rows

Every leaf subscription row declares its **maker**, meaning what can turn the leaf True:

| Maker | Example | Able when |
| --- | --- | --- |
| **Observed**: a refresher maintains the fact; an external system acts | `cr/… .state == merged` (CI, the forge, a human reviewer) | the refresher is fresh within the row's demand, and not stale, rate-limited or refused |
| **Actor**: a named crew member, governor or operator makes it true with a verb | crew-work `phase == Done` (coder@work, via `crew complete`) | the actor's evidence says it will act (§3) |
| **Controller**: the owning reconciler retries | a Clone after a failed clone | the declared disposition is *retryable at T*, and the retry ceiling is not exceeded (§4) |

**Arming extends beyond `Landing` and `Anchored`** (amending ADR 0028 and ADR 0029) to every holding phase of every lifecycle resource:
- `Active` arms its crew-obligation rows;
- `Landing` arms its exit table;
- controllers arm their retry rows.

Declaring `waiting_on` means arming rows. It is not a second vocabulary: ADR 0027's single subscription surface stays single.

### 3. Actors are judged by state, not timers

Agents act only inside turns, so an idle agent that owes something will not act unless prompted.

| Evidence | Judgement |
| --- | --- |
| **Working** (mid-turn) | able |
| **Idle** (turn ended) | unable. If it owns an unmet actor row, it is **stalled immediately** |
| **NeedsInput** (a question or permission prompt) | the obligation **re-routes**: the maker becomes whoever answers, per the supervision ladder (§5), and that party is judged in turn |
| **Dead / absent** (session gone, container stopped) | unable; the reaction is recovery, not a nudge |

- **There are no idle timers.** The only allowance is a debounce sized to the evidence tier:
  - Agent hooks that report the end of a turn (claude-code's Stop hook, codex's notify) warrant a debounce of roughly zero. Enriching these hooks is encouraged.
  - Cleat-only agents rely on screen-derived idleness, which can flicker (#1982). They get a short debounce, and must remain supported. Cleat enhancements, such as an explicit idle hook or prompt detection, tighten it later.
- **Evidence records its source** (for example "idle per Stop hook", or "idle per screen for 45s") so a supervisor can weigh it.
- **A standing role is just rows.** An idle governor that no row names owes nothing. It is *available*, never stalled. Once a row names it (a brief to answer, a stalled crew to supervise), the same rules apply.

### 4. Controllers declare a retry disposition

When a controller parks a resource on a failure, it declares one of two dispositions:
- **retryable, next attempt at T**: able, so not stalled, while attempts continue;
- **terminal, needs X**: no maker remains, so it is stalled immediately, naming X (for example "repository not found", "credential spec does not decode", "no placement policy satisfies adapter").

**Retrying forever is itself a stall.** Each controller declares a retry ceiling as attempts and total duration, with a substrate default. Past the ceiling, the row is judged "retrying without progress" and stalls.

This is the shared terminal-vs-retryable primitive #2025 asked for:
- The substrate owns the disposition and the durable backoff.
- #1853 (clone retries) and #1837 (operator resources) migrate onto it.
- Credential delivery refusals and refresh failures (#2100) are one instance.

### 5. Reactions: a declared ladder

The engine emits a stall; reactions climb a ladder. The substrate provides a default, and a workflow template may override it: set N, disable nudging for a role (for example a reviewer in a round-budgeted loop, #1216), or name its supervisor.

**0. Mechanical nudge** (infrastructure, not an agent):
- The daemon re-delivers the unmet obligation to the named actor through turn delivery. It therefore inherits #2111's credentials-staged-before-delivery guarantee.
- Each actor leaf kind in the closed vocabulary carries an **obligation phrasing**, for example: "You owe a settlement claim for work/coder: finish, then run `flotilla crew complete`, or `crew stall` with a reason if blocked." A leaf kind without a phrasing is refused at admission, as an unknown path is.
- Nudges are bounded: at most N per row per idle episode, default 2. An episode resets when the actor returns to Working.

**1. Supervisor agent**:
- The **supervision ladder** is declared per project or workflow. Its default is: the convoy's Bosun if it has one, otherwise the island's Governor, otherwise the operator.
- The supervisor receives the stall record, including the nudge history, as a row naming it. A supervisor that sits idle on it is itself stalled, and the stall climbs.
- Supervisors act only through verbs: re-prompt with context, escalate the model, recommend handoff or abandonment, hold, re-scope. **They never transition phases** (the Bosun ruling on #650, restated).

**2. Operator attention**:
- the backstop when the ladder is exhausted or absent;
- stalls with **no maker at all** (a controller that failed to arm a wake, a terminal controller failure) go straight here, because no agent can supply a missing wake.

### 6. The record: a `Stalled` condition, judged by the authority host

- **A stall is a typed `Stalled` condition on the stalled resource's status**, not a new resource kind. It carries:
  - the row (leaf and maker);
  - the evidence and its source;
  - the time the stall began;
  - the current rung;
  - the nudge history.

  It replicates with the resource. Because a condition field is an ordinary leaf path, supervisors can `flotilla wait --for` it: a governor waits on the stall conditions in its scope instead of polling.
- **The resource's authority host judges it**, since its leaf engine owns the rows. This is ADR 0029's authority-side observation rule; replicas never infer stalls. If the authority daemon is down, the Host resource's heartbeat staleness is the stall, and any peer can surface it.
- **Surfaces derive four states:**
  - **Working**: progressing.
  - **Available**: idle, owing nothing (standing roles). Visible, never an interruption.
  - **Stalled, handled**: rung 0 or 1 is in flight. Shown quietly, with the rung.
  - **Needs you**: the operator rung is reached, a question is routed to the operator, or a stall has no maker.

  Only **Needs you** fills Attention.

### 7. Crews can declare a stall

- **`flotilla crew stall --reason <kind> --message …`** lets a crew member say "blocked, but the work still matters".
- The reasons are a closed vocabulary, so supervisors and dashboards can act on them:
  - `infra`: credentials, authentication, network, disk, daemon, CI infrastructure, flaky jobs, environment or tooling;
  - `scope`: the brief needs cutting down or splitting, or is wrong or unsolvable as written; describe what is achievable;
  - `decision`: the brief is contradictory or blocked on a ruling; describe what is achievable;
  - `access`: a missing repository or permission;
  - `other`.
- **Crew work gains a `Stalled` phase, distinct from Done and Failed.** Stalled keeps the convoy live and the session resumable even when the brief itself cannot be completed as written. It is the declared form of NeedsInput: the obligation re-routes up the supervision ladder (§5). A crew may add `--propose <resume|reduce-scope|fail>` to recommend a disposition without enacting it. The proposed disposition is optional stored status and decodes with a default for one generation (ADR 0047).
- **Supervisor verbs on a stalled crew:**
  - resume it with guidance (turn delivery);
  - re-scope it (split the issue, abandon the convoy, dispatch the parts);
  - fix the environment and resume it;
  - escalate it;
  - convert it to failed.
- **Credentials stay staged while a crew is Stalled**, because a stalled crew usually resumes, and revoke-then-restore cycles have proved fragile (#2107). The retry ceiling (§4) still applies, so a long stall escalates.
- **Crews stall; supervisors decide failure.** A crew never enacts terminal failure, including when the brief is wrong or unsolvable. `crew fail` refuses a crew principal with a pointer to `crew stall`; supervisors use `crew supervise … convert-to-failed`, and operators may force failure. Authentication failures are usually transient credential-refresh gaps: retry once, then stall with `infra` if still blocked.

### 8. Governors watch; navigating is a separate mind

- Governors (at least in this fleet's install) **watch standing rows** over the issue, change-request and convoy leaves in their scope. The relay makes those rows cheap (ADR 0041).
- The governor's work becomes supervision, triage and dispatch over that feed, plus the ladder rung it holds.
- Design, grilling and prototyping are a different kind of thinking. They belong to an ephemeral **navigator** role that the governor activates, so the two keep distinct context windows. Where the navigator runs is an orthogonal placement decision. The navigator role is recorded as its own brainstorm and is not decided here.

## Consequences

- **One mechanism serves several purposes:** waits, reconciler wakes, turn delivery, stall detection and supervisor feeds are all rows in one engine. New workflows compose from rows rather than bespoke watchdogs.
- **Controller authors take on one obligation: arm your rows honestly.** A non-terminal resource with no live row is itself flagged, so a controller that forgets to declare what it waits on is caught by the invariant rather than by an operator.
- **Silent stalls become visible within seconds:**
  - The swallowed-completion case (#2117) is caught by the crew-obligation row and nudged, whether or not the completion path is fixed.
  - The Landing-only arming gap is exposed as a no-maker stall.
  - Log-only controller retry loops surface as retry-ceiling stalls.
- **Attention stops filling with idle standing agents.** They show as Available.
- **Cost:**
  - more rows, since every holding phase now arms;
  - obligation phrasings must be written for every actor leaf kind;
  - evidence plumbing must be built per agent harness.
- **Superseded:**
  - #1987's prompt reinforcement is replaced by rung 0.
  - #1973 becomes the evidence-tier work.
  - #1983 becomes the surface-state work.
  - #2025 is absorbed into §4.
- **Not decided here:**
  - the navigator role;
  - workflow-specific ladder policies beyond the default;
  - the exact default retry ceilings (tunables, chosen during implementation).
