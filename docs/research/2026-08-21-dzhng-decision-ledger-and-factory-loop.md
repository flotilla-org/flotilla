# dzhng's software-factory loop: the decision ledger as the review surface

**Date:** 2026-08-21

**Prompt:** Robert flagged
[x.com/dzhng/status/2090252351533973768](https://x.com/dzhng/status/2090252351533973768)
("Building software factories (with no slop)", David Zhang, 2026-08-20,
~215k views). Read via authenticated browser session; the companion skills
live at [github.com/dzhng/skills](https://github.com/dzhng/skills) (MIT,
~700 stars, active as of 2026-08-21). Follow-on to
[2026-07-26-software-factories-and-flotilla.md](2026-07-26-software-factories-and-flotilla.md),
which covered the same author's earlier thread.

**Status:** Research note, not a ruling.

## The argument, compressed

Generation outpaces review; when supply exceeds inspection capacity,
"inspection stops being a filter and becomes a formality" and slop is
guaranteed — no amount of (AI or human) line-reading fixes it, because the
reviewers are "pinned to the artifact, and the artifact is exactly the
thing going opaque". His move: treat it as an **interpretability problem,
not a review problem**. The codebase becomes a black box sliced into
domain-specific pieces with well-defined inputs/outputs; "the interface
becomes the review surface". Four artifacts must stay human-readable no
matter how opaque the code gets: **invariants**, **traces** (at the
seams), **attack surface**, and — his centerpiece — **decisions**.

## The decision ledger (the load-bearing mechanism)

Every agent session is prompted (via a skill — `audit-choices` in the
repo) to emit a ledger of **every decision made where the spec was
silent** — plain language, **ranked least-confident first**: "where it
guessed, what it guessed, what it would have asked me if asking were
free." His numbers: a two-day run produces tens of thousands of lines he
will never audit and ~thirty decisions that determine correctness; "I read
the thirty. I push back on four." Framed as what senior code review was
always for, "with the incidental part finally stripped away."

Two implementation rules he states explicitly:

- **The auditor is a separate pass from the implementer** (independent
  sub-agent), "because a model reviewing its own work is primed by its own
  intent and will rationalize."
- **The audit never blocks and cannot change code** — "the moment it can
  fix things it starts optimizing for a clean report instead of an honest
  one."

## The loop

1. **Map the fog** — scout the goal "quadrant by quadrant for what's
   known, what's unknown, and what's a blindspot", handing back rendered
   options and decision tables; carve into independently-takeable
   territories; re-slice when a territory hides more map
   (`explore-unknowns`).
2. **Codify** — write the spec; "mostly transcription", decisions made
   upstream where they were cheap (`write-spec`, `close-spec`).
3. **Build** — harness in loop mode, spec-driven, reviews fire per slice,
   plan re-slices itself when the build proves it stale
   (`implement-spec`, with per-engine variants incl.
   `implement-spec-with-codex`).
4. **Review the choices** — read the ledger least-confident-first, push
   back, re-audit. His flagship datum: one unattended run of **1 day 16
   hours** where "I reviewed the ledger, not the diff."

## Mapping to Flotilla

- **The decision ledger is the missing settlement artifact.** Crews
  already emit completion signals; a ranked
  where-the-brief-was-silent ledger attached to the settlement claim
  would give the governor (and Robert) exactly the review surface this
  essay argues for — read the ledger, not the diff. It slots into the
  existing Brief/turn vocabulary with no new machinery beyond a brief
  obligation and a place to put the artifact (a convoy annotation or a PR
  comment section the shepherd already knows how to read).
- **Auditor-separation is already house style** (independent
  claude-review, adversarial verification patterns); his
  "never blocks, cannot change code" rule is a crisp constraint worth
  stating in the review-workflow docs.
- **Fog-of-war scouting is convergent evolution with the wayfinder skill**
  (Matt Pocock's, in local use here) — quadrant sweeps ≈ breadth-first
  frontier mapping; "decision tables to react to rather than asking
  someone to imagine" ≈ prototype tickets; territories ≈ the fog/ticket
  distinction. Two independent authors landing on the same shape is
  evidence the shape is right. Since the local skill set is adjustable,
  `explore-unknowns` is worth a comparative read for pieces to
  incorporate — or to adopt alongside — rather than treating either as
  fixed.
- **Seams-and-sensors** restates the repo's own testing philosophy
  (behavior contracts over implementation reads) at codebase scale, and
  is the same instinct as the drydock extraction: pieces with interfaces
  you can interrogate.
- **Skills repo is MIT** — `audit-choices` and `explore-unknowns` are
  directly readable/liftable as brief-template material for crews.

## Adoptable ideas, ranked

1. **Ledger-in-settlement**: add a decision-ledger obligation to crew
   briefs and surface it at settlement (least-confident-first). Cheap,
   immediately useful, no schema work required to start (PR-comment
   convention first, resource later if it earns it).
2. **Non-blocking auditor rule** stated as policy where review workflows
   are documented.
3. Read `audit-choices`/`explore-unknowns` skills for concrete prompt
   patterns before the next brief-template revision.
4. Longer-horizon: his "traces at the seams" artifact aligns with the
   observability direction (event log / annotation layer) — a named
   consumer for it.
